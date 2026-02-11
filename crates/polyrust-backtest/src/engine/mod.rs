use chrono::{DateTime, Utc};
use indicatif::{ProgressBar, ProgressStyle};
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use polyrust_core::actions::Action;
use polyrust_core::context::{BalanceState, StrategyContext};
use polyrust_core::error::Result;
use polyrust_core::events::{Event, MarketDataEvent, OrderEvent};
use polyrust_core::strategy::Strategy;
use polyrust_core::types::*;
use polyrust_store::Store;

use crate::config::BacktestConfig;
use crate::data::store::HistoricalDataStore;

/// Historical market data loaded from the database for replay.
#[derive(Debug, Clone)]
pub struct HistoricalEvent {
    pub timestamp: DateTime<Utc>,
    pub token_id: String,
    pub event: Event,
}

/// How a closing trade (sell) was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CloseReason {
    /// Strategy issued the sell order
    Strategy,
    /// Market expired, binary resolution ($1/$0)
    Settlement,
    /// Backtest ended, position still open — binary settlement applied
    ForceClose,
}

/// A completed backtest trade with realized P&L.
#[derive(Debug, Clone)]
pub struct BacktestTrade {
    pub timestamp: DateTime<Utc>,
    pub token_id: String,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub realized_pnl: Option<Decimal>,
    /// None for buys, Some(reason) for sells
    pub close_reason: Option<CloseReason>,
}

/// Per-bucket trade aggregation tracking last buy and sell prices.
struct BucketAgg {
    token_id: String,
    last_price: Decimal,
    last_buy: Option<Decimal>,
    last_sell: Option<Decimal>,
}

/// Synthesize PriceChange events from trade data by bucketing trades into N-second windows.
///
/// For each token, trades are grouped into time buckets of `fidelity_secs` duration.
/// The last trade's price in each bucket becomes a PriceChange event, timestamped
/// at the bucket's end (bucket_start + fidelity_secs).
///
/// Produces realistic bid/ask spread from actual buy/sell trade prices within each bucket.
///
/// Accepts the full event list and filters for Trade events internally
/// (avoids cloning all trades into a separate Vec).
fn synthesize_price_events_from_trades(
    events: &[HistoricalEvent],
    fidelity_secs: u64,
) -> Vec<HistoricalEvent> {
    if events.is_empty() || fidelity_secs == 0 {
        return Vec::new();
    }

    let fidelity = fidelity_secs as i64;
    let default_spread = Decimal::new(1, 2); // 0.01 (1 tick)

    // Group trades by token_id, then bucket by time window
    // BTreeMap ensures deterministic ordering of buckets
    let mut token_buckets: HashMap<String, BTreeMap<i64, BucketAgg>> = HashMap::new();

    for event in events {
        if let Event::MarketData(MarketDataEvent::Trade {
            token_id, price, ..
        }) = &event.event
        {
            let ts = event.timestamp.timestamp();
            let bucket_start = (ts / fidelity) * fidelity;

            // Determine trade side from the HistoricalEvent's associated trade data.
            // We need to find the corresponding HistoricalTrade side info.
            // Since trades come from the subgraph with side encoded, we look at
            // the surrounding context. For synthesized events, default to mid-price.

            let bucket = token_buckets
                .entry(token_id.clone())
                .or_default()
                .entry(bucket_start)
                .or_insert(BucketAgg {
                    token_id: token_id.clone(),
                    last_price: *price,
                    last_buy: None,
                    last_sell: None,
                });

            bucket.last_price = *price;
        }
    }

    // Second pass: capture buy/sell sides from the raw historical trade data
    // The Trade events don't carry side info in the Event enum, but the
    // HistoricalTrade records in the DB do. We approximate by tracking the
    // last price change direction: price increase → buy, decrease → sell.
    let mut prev_prices: HashMap<String, Decimal> = HashMap::new();
    for event in events {
        if let Event::MarketData(MarketDataEvent::Trade {
            token_id, price, ..
        }) = &event.event
        {
            let ts = event.timestamp.timestamp();
            let bucket_start = (ts / fidelity) * fidelity;

            // Infer side from price movement
            let prev = prev_prices.get(token_id).copied();
            let is_buy = prev.is_none_or(|p| *price >= p);
            prev_prices.insert(token_id.clone(), *price);

            if let Some(bucket) = token_buckets
                .get_mut(token_id)
                .and_then(|b| b.get_mut(&bucket_start))
            {
                if is_buy {
                    bucket.last_buy = Some(*price);
                } else {
                    bucket.last_sell = Some(*price);
                }
            }
        }
    }

    let mut synthetic_events = Vec::new();

    for buckets in token_buckets.values() {
        for (&bucket_start, agg) in buckets {
            let bucket_end = bucket_start + fidelity;
            let timestamp = DateTime::from_timestamp(bucket_end, 0).unwrap_or_else(|| {
                Utc::now() // Fallback; shouldn't happen with valid trade timestamps
            });

            // Derive bid/ask from actual buy/sell sides
            let (best_bid, best_ask) = match (agg.last_sell, agg.last_buy) {
                (Some(sell), Some(buy)) => (sell, buy),
                (Some(sell), None) => (sell, sell + default_spread),
                (None, Some(buy)) => ((buy - default_spread).max(Decimal::new(1, 2)), buy),
                (None, None) => (
                    (agg.last_price - default_spread).max(Decimal::new(1, 2)),
                    agg.last_price + default_spread,
                ),
            };

            synthetic_events.push(HistoricalEvent {
                timestamp,
                token_id: agg.token_id.clone(),
                event: Event::MarketData(MarketDataEvent::PriceChange {
                    token_id: agg.token_id.clone(),
                    price: agg.last_price,
                    side: OrderSide::Buy,
                    best_bid,
                    best_ask,
                }),
            });
        }
    }

    synthetic_events
}

/// Backtesting engine that replays historical events through a strategy.
///
/// This engine:
/// - Loads historical data from HistoricalDataStore (persistent cache)
/// - Generates a chronologically-sorted event stream
/// - Advances a simulated clock through each event
/// - Executes strategy logic and collects actions
/// - Simulates immediate fills at current market price
/// - Tracks positions and balance
/// - Optionally records trades to an in-memory Store (using existing schema)
pub struct BacktestEngine {
    config: BacktestConfig,
    strategy: Box<dyn Strategy>,
    data_store: Arc<HistoricalDataStore>,
    /// Optional Store for trade persistence. None in sweep mode (skip SQLite overhead).
    store: Option<Arc<Store>>,
    ctx: StrategyContext,
    current_time: DateTime<Utc>,
    /// Token price cache: token_id -> latest price
    token_prices: HashMap<String, Decimal>,
    /// Track entry prices for P&L calculation: token_id -> (size, avg_entry_price)
    position_entries: HashMap<String, (Decimal, Decimal)>,
    /// Market-level token mapping: market_id -> (token_a, token_b)
    market_tokens: HashMap<String, (String, String)>,
    /// Reverse mapping: token_id -> market_id (for fill events)
    token_to_market: HashMap<String, String>,
    /// Optional progress bar for event replay (None in sweep mode).
    progress_bar: Option<ProgressBar>,
    // --- Funnel instrumentation counters ---
    markets_discovered: usize,
    orders_submitted: usize,
    orders_filled: usize,
    orders_rejected: usize,
}

impl BacktestEngine {
    /// Create a new backtest engine with trade persistence to a Store.
    ///
    /// - `config`: backtest configuration
    /// - `strategy`: strategy to test
    /// - `data_store`: historical data cache (persistent DB)
    /// - `store`: fresh in-memory Store for recording simulated trades
    pub async fn new(
        config: BacktestConfig,
        strategy: Box<dyn Strategy>,
        data_store: Arc<HistoricalDataStore>,
        store: Arc<Store>,
    ) -> Self {
        Self::new_inner(config, strategy, data_store, Some(store)).await
    }

    /// Create a new backtest engine without Store persistence (for sweep mode).
    ///
    /// Trades are tracked in-memory only — no SQLite overhead per run.
    pub async fn new_without_store(
        config: BacktestConfig,
        strategy: Box<dyn Strategy>,
        data_store: Arc<HistoricalDataStore>,
    ) -> Self {
        Self::new_inner(config, strategy, data_store, None).await
    }

    async fn new_inner(
        config: BacktestConfig,
        strategy: Box<dyn Strategy>,
        data_store: Arc<HistoricalDataStore>,
        store: Option<Arc<Store>>,
    ) -> Self {
        let ctx = StrategyContext::new();
        let current_time = config.start_date;

        // Initialize balance
        let balance = BalanceState {
            available_usdc: config.initial_balance,
            ..Default::default()
        };

        // Update context with initial balance
        {
            let mut bal = ctx.balance.write().await;
            *bal = balance;
        }

        Self {
            config,
            strategy,
            data_store,
            store,
            ctx,
            current_time,
            token_prices: HashMap::new(),
            position_entries: HashMap::new(),
            market_tokens: HashMap::new(),
            token_to_market: HashMap::new(),
            progress_bar: None,
            markets_discovered: 0,
            orders_submitted: 0,
            orders_filled: 0,
            orders_rejected: 0,
        }
    }

    /// Run the backtest from start_date to end_date.
    ///
    /// Returns the list of all trades executed during the backtest.
    pub async fn run(&mut self) -> Result<Vec<BacktestTrade>> {
        info!(
            strategy = self.strategy.name(),
            start = %self.config.start_date,
            end = %self.config.end_date,
            "Starting backtest"
        );

        // Call strategy.on_start
        self.strategy.on_start(&self.ctx).await?;

        // Load historical events
        let events = self.load_events().await?;
        info!(event_count = events.len(), "Loaded historical events");

        // Auto-create progress bar for standalone runs (not sweep mode)
        if self.progress_bar.is_none() {
            let pb = ProgressBar::new(events.len() as u64);
            pb.set_style(
                ProgressStyle::with_template(
                    "[{elapsed_precise}] {bar:40.green/black} {pos}/{len} events ({eta}) {msg}",
                )
                .unwrap(),
            );
            self.progress_bar = Some(pb);
        }

        let result = self.run_with_events(&events).await;

        // Finish and clear bar
        if let Some(ref pb) = self.progress_bar {
            pb.finish_and_clear();
        }
        self.progress_bar = None;

        result
    }

    /// Run the backtest with pre-loaded events (avoids re-loading from DB).
    ///
    /// Used by sweep runner to share a single event load across many runs.
    /// The engine must have been initialized with the correct config
    /// (including market_ids for market_tokens/token_to_market mappings).
    pub async fn run_with_events(&mut self, events: &[HistoricalEvent]) -> Result<Vec<BacktestTrade>> {
        // Call strategy.on_start if run() didn't already
        // (For sweep mode, run_with_events is called directly)

        // Validate that we have events to replay
        if events.is_empty() {
            return Err(polyrust_core::error::PolyError::Config(
                "No historical events found for configured market_ids and date range. \
                Check that data has been fetched and cached in the backtest database."
                    .to_string(),
            ));
        }

        let mut trades = Vec::new();

        // Replay events in chronological order
        let total_events = events.len();

        for (i, historical_event) in events.iter().enumerate() {
            self.current_time = historical_event.timestamp;

            // Update token price cache for price events
            match &historical_event.event {
                Event::MarketData(MarketDataEvent::PriceChange {
                    token_id,
                    price,
                    best_bid,
                    best_ask,
                    ..
                }) => {
                    self.token_prices.insert(token_id.clone(), *price);
                    // Populate orderbook so strategies can read best ask/bid
                    let mut md = self.ctx.market_data.write().await;
                    md.orderbooks.insert(
                        token_id.clone(),
                        OrderbookSnapshot {
                            token_id: token_id.clone(),
                            bids: vec![OrderbookLevel {
                                price: *best_bid,
                                size: Decimal::new(1000, 0),
                            }],
                            asks: vec![OrderbookLevel {
                                price: *best_ask,
                                size: Decimal::new(1000, 0),
                            }],
                            timestamp: self.current_time,
                        },
                    );
                }
                Event::MarketData(MarketDataEvent::ExternalPrice { symbol, price, .. }) => {
                    // Store in external_prices keyed by coin symbol (used by strategy discovery)
                    self.token_prices.insert(symbol.clone(), *price);
                    self.ctx
                        .market_data
                        .write()
                        .await
                        .external_prices
                        .insert(symbol.clone(), *price);
                }
                _ => {}
            }

            // Count market discoveries
            if matches!(&historical_event.event, Event::MarketData(MarketDataEvent::MarketDiscovered(_))) {
                self.markets_discovered += 1;
            }

            // Advance simulated clock before strategy sees the event
            {
                let mut clock = self.ctx.simulated_clock.write().await;
                *clock = Some(self.current_time);
            }

            // Call strategy.on_event
            let actions = self
                .strategy
                .on_event(&historical_event.event, &self.ctx)
                .await?;

            // Execute actions and feed fill events back to strategy
            for action in actions {
                match action {
                    Action::PlaceOrder(order_req) => {
                        self.orders_submitted += 1;
                        trades.extend(self.execute_and_notify(order_req).await?);
                    }
                    Action::PlaceBatchOrder(orders) => {
                        for order in orders {
                            self.orders_submitted += 1;
                            trades.extend(self.execute_and_notify(order).await?);
                        }
                    }
                    other => {
                        if let Some(trade) = self.execute_action(other).await? {
                            trades.push(trade);
                        }
                    }
                }
            }

            // Settle positions on market expiry (binary resolution: winner→$1, loser→$0)
            if let Event::MarketData(MarketDataEvent::MarketExpired(market_id)) =
                &historical_event.event
                && let Some((token_a, token_b)) = self.market_tokens.get(market_id).cloned()
            {
                for token_id in [token_a, token_b] {
                    if let Some((size, _entry)) = self.position_entries.get(&token_id).cloned()
                        && size > Decimal::ZERO
                    {
                        let last_price = self
                            .token_prices
                            .get(&token_id)
                            .copied()
                            .unwrap_or(Decimal::ZERO);
                        // Binary resolution: price > 0.5 means winning token → $1
                        let settlement_price = if last_price > Decimal::new(5, 1) {
                            Decimal::ONE
                        } else {
                            Decimal::ZERO
                        };

                        debug!(
                            market_id,
                            token_id = %token_id,
                            size = %size,
                            last_price = %last_price,
                            settlement_price = %settlement_price,
                            "Settling position at market expiry"
                        );

                        // Always record the sell — $1 for winners, $0 for losers.
                        // $0 sells correctly record the loss as realized_pnl = -cost_basis.
                        self.token_prices.insert(token_id.clone(), settlement_price);

                        let sell = OrderRequest::new(
                            token_id,
                            settlement_price,
                            size,
                            OrderSide::Sell,
                            OrderType::Gtc,
                            false,
                        );
                        let mut settled = self.execute_and_notify(sell).await?;
                        for t in &mut settled {
                            if t.side == OrderSide::Sell {
                                t.close_reason = Some(CloseReason::Settlement);
                            }
                        }
                        trades.extend(settled);
                    }
                }
            }

            // Update progress bar (if present)
            if let Some(ref pb) = self.progress_bar {
                pb.set_position((i + 1) as u64);
            }
        }

        // MarketExpired events are injected per-market in load_events at their actual end_date.

        // Count filled/rejected from executed trades
        let buy_count = trades.iter().filter(|t| t.side == OrderSide::Buy).count();
        let sell_count = trades.iter().filter(|t| t.side == OrderSide::Sell).count();
        self.orders_filled = buy_count + sell_count;
        self.orders_rejected = self.orders_submitted.saturating_sub(self.orders_filled);

        // Log funnel summary
        info!(
            markets_discovered = self.markets_discovered,
            total_events = total_events,
            orders_submitted = self.orders_submitted,
            orders_filled = self.orders_filled,
            orders_rejected = self.orders_rejected,
            trades_buy = buy_count,
            trades_sell = sell_count,
            "Backtest funnel summary"
        );

        // Force-close remaining positions at end of backtest (markets that expire after end_date)
        let remaining_tokens: Vec<(String, Decimal)> = self
            .position_entries
            .iter()
            .filter(|(_, (size, _))| *size > Decimal::ZERO)
            .map(|(token, (size, _))| (token.clone(), *size))
            .collect();

        if !remaining_tokens.is_empty() {
            debug!(
                remaining = remaining_tokens.len(),
                "Force-closing remaining positions at end of backtest"
            );
        }

        for (token_id, size) in remaining_tokens {
            let last_price = self
                .token_prices
                .get(&token_id)
                .copied()
                .unwrap_or(Decimal::ZERO);
            // Binary settlement: same as market expiry resolution
            let settlement_price = if last_price > Decimal::new(5, 1) {
                Decimal::ONE
            } else {
                Decimal::ZERO
            };

            debug!(
                token_id = %token_id,
                size = %size,
                last_price = %last_price,
                settlement_price = %settlement_price,
                "Force-closing position with binary settlement"
            );

            self.token_prices.insert(token_id.clone(), settlement_price);
            let sell = OrderRequest::new(
                token_id,
                settlement_price,
                size,
                OrderSide::Sell,
                OrderType::Gtc,
                false,
            );
            let mut force_closed = self.execute_and_notify(sell).await?;
            for t in &mut force_closed {
                if t.side == OrderSide::Sell {
                    t.close_reason = Some(CloseReason::ForceClose);
                }
            }
            trades.extend(force_closed);
        }

        // Call strategy.on_stop
        let final_actions = self.strategy.on_stop(&self.ctx).await?;
        for action in final_actions {
            match action {
                Action::PlaceOrder(order_req) => {
                    trades.extend(self.execute_and_notify(order_req).await?);
                }
                Action::PlaceBatchOrder(orders) => {
                    for order in orders {
                        trades.extend(self.execute_and_notify(order).await?);
                    }
                }
                other => {
                    if let Some(trade) = self.execute_action(other).await? {
                        trades.push(trade);
                    }
                }
            }
        }

        info!(
            strategy = self.strategy.name(),
            trade_count = trades.len(),
            "Backtest complete"
        );

        Ok(trades)
    }

    /// Call strategy.on_start (public for sweep runner).
    pub async fn strategy_on_start(&mut self) -> Result<()> {
        self.strategy.on_start(&self.ctx).await
    }

    /// Get token maps built during load_events() (for sharing with sweep engines).
    pub fn token_maps(&self) -> (HashMap<String, (String, String)>, HashMap<String, String>) {
        (self.market_tokens.clone(), self.token_to_market.clone())
    }

    /// Inject pre-built token maps (used by sweep runner to avoid re-loading).
    pub fn set_token_maps(
        &mut self,
        market_tokens: HashMap<String, (String, String)>,
        token_to_market: HashMap<String, String>,
    ) {
        self.market_tokens = market_tokens;
        self.token_to_market = token_to_market;
    }

    /// Load historical events from the data store.
    ///
    /// Public so sweep runner can call it once and share events across runs.
    pub async fn load_events(&mut self) -> Result<Vec<HistoricalEvent>> {
        let mut events = Vec::new();

        // Pre-load crypto kline data indexed by coin for discovery price lookup.
        // This replaces hardcoded prices (BTC=$100K, ETH=$3K, SOL=$200) with
        // actual historical prices at each market's discovery timestamp.
        let mut coin_klines: HashMap<String, Vec<(i64, Decimal)>> = HashMap::new();
        for coin in &["BTC", "ETH", "SOL", "XRP"] {
            let prices = self
                .data_store
                .get_crypto_prices(coin, self.config.start_date, self.config.end_date)
                .await
                .map_err(|e| polyrust_core::error::PolyError::Config(e.to_string()))?;
            if !prices.is_empty() {
                let mut sorted: Vec<(i64, Decimal)> = prices
                    .iter()
                    .map(|p| (p.timestamp.timestamp(), p.close))
                    .collect();
                sorted.sort_by_key(|(ts, _)| *ts);
                info!(
                    coin,
                    count = sorted.len(),
                    "Pre-loaded klines for discovery price lookup"
                );
                coin_klines.insert(coin.to_string(), sorted);
            }
        }

        // Helper closure: find closest kline price at a given timestamp
        let find_kline_price = |coin: &str, target_ts: i64| -> Option<Decimal> {
            let klines = coin_klines.get(coin)?;
            if klines.is_empty() {
                return None;
            }
            let idx = klines.partition_point(|(ts, _)| *ts <= target_ts);
            // Check both the entry at idx-1 (last <= target) and idx (first > target)
            let candidates: Vec<_> = [idx.checked_sub(1), Some(idx)]
                .into_iter()
                .flatten()
                .filter(|&i| i < klines.len())
                .collect();
            candidates
                .into_iter()
                .min_by_key(|&i| (klines[i].0 - target_ts).unsigned_abs())
                .map(|i| klines[i].1)
        };

        // For each market_id, load prices and trades for both tokens,
        // and inject MarketDiscovered/MarketExpired lifecycle events.
        for market_id in &self.config.market_ids {
            // Query the historical_markets table to get both token IDs
            let market = self
                .data_store
                .get_historical_market(market_id)
                .await
                .map_err(|e| polyrust_core::error::PolyError::Config(e.to_string()))?;

            let token_ids = if let Some(ref m) = market {
                // Build market_id -> (token_a, token_b) mapping for settlement
                self.market_tokens
                    .insert(m.market_id.clone(), (m.token_a.clone(), m.token_b.clone()));
                // Build reverse mapping: token_id -> market_id (for fill events)
                self.token_to_market
                    .insert(m.token_a.clone(), m.market_id.clone());
                self.token_to_market
                    .insert(m.token_b.clone(), m.market_id.clone());
                vec![m.token_a.clone(), m.token_b.clone()]
            } else {
                // Market not found in cache - assume market_id IS a token_id for backwards compatibility
                warn!(market_id, "Market not found in cache, treating as token_id");
                vec![market_id.clone()]
            };

            // Inject MarketDiscovered event at market start_date (or backtest start if earlier)
            if let Some(ref m) = market {
                let discover_ts = m.start_date.max(self.config.start_date);
                let market_info = MarketInfo {
                    id: m.market_id.clone(),
                    slug: m.slug.clone(),
                    question: m.question.clone(),
                    start_date: Some(m.start_date),
                    end_date: m.end_date,
                    token_ids: TokenIds {
                        outcome_a: m.token_a.clone(),
                        outcome_b: m.token_b.clone(),
                    },
                    accepting_orders: true,
                    neg_risk: m.neg_risk,
                    min_order_size: Decimal::new(5, 0),
                    tick_size: Decimal::new(1, 2),
                    fee_rate_bps: 0,
                };
                events.push(HistoricalEvent {
                    timestamp: discover_ts,
                    token_id: m.token_a.clone(),
                    event: Event::MarketData(MarketDataEvent::MarketDiscovered(market_info)),
                });

                // Inject an immediate ExternalPrice event right after discovery so
                // the pending market gets promoted. Uses actual kline price at
                // discover_ts when available, falls back to hardcoded base prices.
                let slug_lower = m.slug.to_lowercase();
                let coin_symbol = if slug_lower.starts_with("btc") {
                    Some("BTC")
                } else if slug_lower.starts_with("eth") {
                    Some("ETH")
                } else if slug_lower.starts_with("sol") {
                    Some("SOL")
                } else {
                    None
                };
                if let Some(coin) = coin_symbol {
                    let discover_unix = discover_ts.timestamp();
                    let price = find_kline_price(coin, discover_unix).unwrap_or_else(|| {
                        let fallback = match coin {
                            "BTC" => Decimal::new(100_000, 0),
                            "ETH" => Decimal::new(3_000, 0),
                            "SOL" => Decimal::new(200, 0),
                            _ => Decimal::new(100, 0),
                        };
                        warn!(
                            coin,
                            market_id = %m.market_id,
                            fallback = %fallback,
                            "No kline data at discovery time, using hardcoded fallback"
                        );
                        fallback
                    });
                    // Use discover_ts + 1ns to sort after the MarketDiscovered event
                    let price_ts = discover_ts + chrono::Duration::nanoseconds(1);
                    events.push(HistoricalEvent {
                        timestamp: price_ts,
                        token_id: coin.to_string(),
                        event: Event::MarketData(MarketDataEvent::ExternalPrice {
                            symbol: coin.to_string(),
                            price,
                            source: "backtest-discovery".to_string(),
                            timestamp: price_ts,
                        }),
                    });
                }

                // Inject MarketExpired event at market end_date
                if m.end_date <= self.config.end_date {
                    events.push(HistoricalEvent {
                        timestamp: m.end_date,
                        token_id: m.token_a.clone(),
                        event: Event::MarketData(MarketDataEvent::MarketExpired(m.market_id.clone())),
                    });
                }
            }

            // Load data for each token in the market
            for token_id in token_ids {
                // Load price history
                let prices = self
                    .data_store
                    .get_historical_prices(
                        &token_id,
                        self.config.start_date,
                        self.config.end_date,
                    )
                    .await
                    .map_err(|e| polyrust_core::error::PolyError::Config(e.to_string()))?;

                for price in prices {
                    // Apply realistic spread (1 tick = 0.01) to cached prices,
                    // matching the synthesizer's spread logic
                    let best_bid = (price.price - Decimal::new(1, 2)).max(Decimal::new(1, 2));
                    let best_ask = price.price + Decimal::new(1, 2);
                    events.push(HistoricalEvent {
                        timestamp: price.timestamp,
                        token_id: price.token_id.clone(),
                        event: Event::MarketData(MarketDataEvent::PriceChange {
                            token_id: price.token_id,
                            price: price.price,
                            side: OrderSide::Buy,
                            best_bid,
                            best_ask,
                        }),
                    });
                }

                // Load trade history for this token
                let trades = self
                    .data_store
                    .get_historical_trades(
                        &token_id,
                        self.config.start_date,
                        self.config.end_date,
                    )
                    .await
                    .map_err(|e| polyrust_core::error::PolyError::Config(e.to_string()))?;

                for trade in trades {
                    events.push(HistoricalEvent {
                        timestamp: trade.timestamp,
                        token_id: trade.token_id.clone(),
                        event: Event::MarketData(MarketDataEvent::Trade {
                            token_id: trade.token_id,
                            price: trade.price,
                            size: trade.size,
                            timestamp: trade.timestamp,
                        }),
                    });
                }
            } // end token_ids loop
        } // end market_ids loop

        // Load real crypto prices from Binance klines (historical_crypto_prices table).
        // If no klines are available, fall back to synthetic prices from market probability.
        {
            // Determine which coins are in the backtest from market slugs
            let coin_prefixes = ["btc", "eth", "sol", "xrp"];
            let mut coins_in_backtest: Vec<String> = Vec::new();
            for market_id in &self.config.market_ids {
                if let Ok(Some(m)) = self
                    .data_store
                    .get_historical_market(market_id)
                    .await
                    .map_err(|e| polyrust_core::error::PolyError::Config(e.to_string()))
                {
                    let slug_lower = m.slug.to_lowercase();
                    for prefix in &coin_prefixes {
                        if slug_lower.starts_with(prefix) {
                            let coin = prefix.to_uppercase();
                            if !coins_in_backtest.contains(&coin) {
                                coins_in_backtest.push(coin);
                            }
                            break;
                        }
                    }
                }
            }

            let mut total_crypto_events = 0usize;

            for coin in &coins_in_backtest {
                // Try loading real Binance klines first
                let prices = self
                    .data_store
                    .get_crypto_prices(coin, self.config.start_date, self.config.end_date)
                    .await
                    .map_err(|e| polyrust_core::error::PolyError::Config(e.to_string()))?;

                if !prices.is_empty() {
                    info!(
                        coin,
                        count = prices.len(),
                        "Loaded real Binance klines for ExternalPrice events"
                    );
                    for p in prices {
                        events.push(HistoricalEvent {
                            timestamp: p.timestamp,
                            token_id: coin.clone(),
                            event: Event::MarketData(MarketDataEvent::ExternalPrice {
                                symbol: coin.clone(),
                                price: p.close,
                                source: p.source,
                                timestamp: p.timestamp,
                            }),
                        });
                        total_crypto_events += 1;
                    }
                } else {
                    // Fallback: synthesize from market probability (original behavior)
                    warn!(
                        coin,
                        "No Binance klines found, falling back to synthetic ExternalPrice from market probability"
                    );

                    let nominal_bases: HashMap<&str, Decimal> = [
                        ("BTC", Decimal::new(100_000, 0)),
                        ("ETH", Decimal::new(3_000, 0)),
                        ("SOL", Decimal::new(200, 0)),
                        ("XRP", Decimal::new(1, 0)),
                    ]
                    .into();

                    let base = nominal_bases
                        .get(coin.as_str())
                        .copied()
                        .unwrap_or(Decimal::new(100, 0));
                    let scale = Decimal::new(1, 1); // 0.1
                    let half = Decimal::new(5, 1); // 0.5

                    // Build up_token -> coin map for this coin
                    let mut up_tokens: Vec<String> = Vec::new();
                    for market_id in &self.config.market_ids {
                        if let Ok(Some(m)) = self
                            .data_store
                            .get_historical_market(market_id)
                            .await
                            .map_err(|e| polyrust_core::error::PolyError::Config(e.to_string()))
                            && m.slug.to_lowercase().starts_with(&coin.to_lowercase())
                        {
                            up_tokens.push(m.token_a.clone());
                        }
                    }

                    let trade_events: Vec<_> = events
                        .iter()
                        .filter_map(|e| {
                            if let Event::MarketData(MarketDataEvent::Trade {
                                token_id, price, ..
                            }) = &e.event
                            {
                                if up_tokens.contains(token_id) {
                                    let synthetic_price =
                                        base * (Decimal::ONE + scale * (*price - half));
                                    Some((e.timestamp, coin.clone(), synthetic_price))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        })
                        .collect();

                    for (ts, sym, price) in trade_events {
                        events.push(HistoricalEvent {
                            timestamp: ts,
                            token_id: sym.clone(),
                            event: Event::MarketData(MarketDataEvent::ExternalPrice {
                                symbol: sym,
                                price,
                                source: "backtest-synthetic".to_string(),
                                timestamp: ts,
                            }),
                        });
                        total_crypto_events += 1;
                    }
                }
            }

            info!(
                coins = ?coins_in_backtest,
                total_crypto_events,
                "ExternalPrice events loaded for backtest"
            );
        }

        // Synthesize PriceChange events from trades at configured fidelity
        {
            let synthetic = synthesize_price_events_from_trades(
                &events,
                self.config.data_fidelity_secs,
            );
            info!(
                synthetic_prices = synthetic.len(),
                fidelity_secs = self.config.data_fidelity_secs,
                "Synthesized PriceChange events from trade data"
            );
            events.extend(synthetic);
        }

        // Remove raw Trade events — they've already been consumed by ExternalPrice
        // and PriceChange synthesis above. Replaying them through the strategy is
        // wasteful (the engine's token_prices cache gets the same data from synthesized
        // events). This typically cuts event count by ~65% (e.g. 9.7M -> 3.4M).
        let before = events.len();
        events.retain(|e| !matches!(&e.event, Event::MarketData(MarketDataEvent::Trade { .. })));
        info!(
            before = before,
            after = events.len(),
            removed = before - events.len(),
            "Filtered raw Trade events (consumed by synthesis)"
        );

        // Sort events chronologically
        events.sort_by_key(|e| e.timestamp);

        Ok(events)
    }


    /// Execute a single action from the strategy.
    ///
    /// Returns Some(BacktestTrade) if the action resulted in a trade.
    async fn execute_action(&mut self, action: Action) -> Result<Option<BacktestTrade>> {
        match action {
            Action::PlaceOrder(order_req) => self.execute_order(order_req).await,
            Action::PlaceBatchOrder(orders) => {
                // Execute each order in the batch
                // NOTE: All trades are persisted to Store and included in the final report.
                // This return value only affects the in-memory trades list used for logging.
                let mut batch_trades = Vec::new();
                for order in orders {
                    if let Some(trade) = self.execute_order(order).await? {
                        batch_trades.push(trade);
                    }
                }
                // Return the first trade for simplicity (all trades are in Store)
                Ok(batch_trades.into_iter().next())
            }
            Action::Log { level, message } => {
                match level {
                    polyrust_core::actions::LogLevel::Debug => debug!("{}", message),
                    polyrust_core::actions::LogLevel::Info => info!("{}", message),
                    polyrust_core::actions::LogLevel::Warn => warn!("{}", message),
                    polyrust_core::actions::LogLevel::Error => {
                        tracing::error!("{}", message)
                    }
                }
                Ok(None)
            }
            Action::CancelOrder(order_id) => {
                // Feed OrderEvent::Cancelled back to the strategy so it can
                // clean up open_limit_orders and unblock has_market_exposure.
                let cancelled_event =
                    Event::OrderUpdate(OrderEvent::Cancelled(order_id.clone()));
                let actions = self.strategy.on_event(&cancelled_event, &self.ctx).await?;
                for action in actions {
                    // Use Box::pin to avoid infinite future size from recursion
                    Box::pin(self.execute_action(action)).await?;
                }
                Ok(None)
            }
            _ => {
                // Other actions (EmitSignal, etc.) are not simulated in backtest
                debug!("Ignoring action: {:?}", action);
                Ok(None)
            }
        }
    }

    /// Execute an order and feed Placed+Filled events back to the strategy.
    ///
    /// Returns the primary trade plus any secondary trades triggered by the strategy
    /// reacting to the fill events (e.g. stop-loss exits).
    async fn execute_and_notify(
        &mut self,
        order: OrderRequest,
    ) -> Result<Vec<BacktestTrade>> {
        let mut trades = Vec::new();
        if let Some(trade) = self.execute_order(order).await? {
            let order_id = Uuid::new_v4().to_string();
            let market_id = self
                .token_to_market
                .get(&trade.token_id)
                .cloned()
                .unwrap_or_default();

            // 1. Feed OrderEvent::Placed to strategy
            let placed_event = Event::OrderUpdate(OrderEvent::Placed(OrderResult {
                success: true,
                order_id: Some(order_id.clone()),
                token_id: trade.token_id.clone(),
                price: trade.price,
                size: trade.size,
                side: trade.side,
                status: Some("Filled".to_string()),
                message: "backtest-fill".to_string(),
            }));
            let placed_actions = self.strategy.on_event(&placed_event, &self.ctx).await?;
            for action in placed_actions {
                if let Some(t) = self.execute_action(action).await? {
                    trades.push(t);
                }
            }

            // 2. Feed OrderEvent::Filled to strategy
            let filled_event = Event::OrderUpdate(OrderEvent::Filled {
                order_id,
                market_id,
                token_id: trade.token_id.clone(),
                side: trade.side,
                price: trade.price,
                size: trade.size,
                strategy_name: self.strategy.name().to_string(),
            });
            let filled_actions = self.strategy.on_event(&filled_event, &self.ctx).await?;
            for action in filled_actions {
                if let Some(t) = self.execute_action(action).await? {
                    trades.push(t);
                }
            }

            trades.push(trade);
        }
        Ok(trades)
    }

    /// Execute an order immediately at the current market price.
    ///
    /// This is a simplified "Immediate fill mode" implementation.
    /// Historical orderbook depth is not available from Polymarket APIs.
    async fn execute_order(&mut self, order: OrderRequest) -> Result<Option<BacktestTrade>> {
        let current_price = self
            .token_prices
            .get(&order.token_id)
            .cloned()
            .unwrap_or(order.price);

        // Validate price and size
        // Allow price == 0 for sells (expired worthless positions)
        let price_invalid = match order.side {
            OrderSide::Buy => order.price <= Decimal::ZERO || order.price > Decimal::ONE,
            OrderSide::Sell => order.price < Decimal::ZERO || order.price > Decimal::ONE,
        };
        if price_invalid {
            warn!(
                token_id = %order.token_id,
                price = %order.price,
                "Invalid order price, skipping"
            );
            return Ok(None);
        }
        if order.size <= Decimal::ZERO {
            warn!(
                token_id = %order.token_id,
                size = %order.size,
                "Invalid order size, skipping"
            );
            return Ok(None);
        }

        let mut balance = self.ctx.balance.write().await;
        let mut positions = self.ctx.positions.write().await;

        match order.side {
            OrderSide::Buy => {
                // Calculate cost (price * size) + dynamic fee
                // Fee depends on order type: FOK = taker fee, GTC/GTD = maker (0%)
                let cost = current_price * order.size;
                let fee = match order.order_type {
                    OrderType::Fok => {
                        Decimal::TWO * current_price * (Decimal::ONE - current_price)
                            * self.config.fees.taker_fee_rate * order.size
                    }
                    _ => Decimal::ZERO, // GTC/GTD = maker = 0% fee
                };
                let total_cost = cost + fee;

                if balance.available_usdc < total_cost {
                    warn!(
                        token_id = %order.token_id,
                        cost = %total_cost,
                        balance = %balance.available_usdc,
                        "Insufficient balance for BUY, skipping"
                    );
                    return Ok(None);
                }

                // Deduct balance
                balance.available_usdc -= total_cost;

                // Update position entry tracking
                // Include fees in the effective entry price for accurate P&L calculation
                let fee_per_share = match order.order_type {
                    OrderType::Fok => {
                        Decimal::TWO * current_price * (Decimal::ONE - current_price)
                            * self.config.fees.taker_fee_rate
                    }
                    _ => Decimal::ZERO,
                };
                let effective_buy_price = current_price + fee_per_share;

                let (cur_size, cur_entry) = self
                    .position_entries
                    .get(&order.token_id)
                    .cloned()
                    .unwrap_or((Decimal::ZERO, Decimal::ZERO));
                let new_size = cur_size + order.size;
                let new_entry = if new_size > Decimal::ZERO {
                    (cur_entry * cur_size + effective_buy_price * order.size) / new_size
                } else {
                    effective_buy_price
                };
                self.position_entries
                    .insert(order.token_id.clone(), (new_size, new_entry));

                // Update PositionState
                // Find existing position or create new one
                let existing_pos = positions
                    .open_positions
                    .iter()
                    .find(|(_, p)| {
                        p.token_id == order.token_id && p.strategy_name == self.strategy.name()
                    })
                    .map(|(id, _)| *id);

                if let Some(pos_id) = existing_pos {
                    // Update existing position
                    if let Some(pos) = positions.open_positions.get_mut(&pos_id) {
                        pos.size = new_size;
                        pos.entry_price = new_entry;
                        pos.current_price = current_price;
                    }
                } else {
                    // Create new position
                    let position_id = Uuid::new_v4();
                    positions.open_positions.insert(
                        position_id,
                        Position {
                            id: position_id,
                            market_id: String::new(), // Not tracked in backtest
                            token_id: order.token_id.clone(),
                            side: OutcomeSide::Yes, // Simplified
                            entry_price: new_entry,
                            size: new_size,
                            current_price,
                            entry_time: self.current_time,
                            strategy_name: self.strategy.name().to_string(),
                        },
                    );
                }

                // Record trade in Store (if available)
                if let Some(ref store) = self.store {
                    let trade = Trade {
                        id: Uuid::new_v4(),
                        order_id: Uuid::new_v4().to_string(),
                        market_id: String::new(),
                        token_id: order.token_id.clone(),
                        side: OrderSide::Buy,
                        price: current_price,
                        size: order.size,
                        realized_pnl: None,
                        strategy_name: self.strategy.name().to_string(),
                        timestamp: self.current_time,
                    };
                    store.insert_trade(&trade).await.map_err(|e| {
                        polyrust_core::error::PolyError::Execution(format!(
                            "Failed to insert trade: {}",
                            e
                        ))
                    })?;
                }

                debug!(
                    token_id = %order.token_id,
                    price = %current_price,
                    size = %order.size,
                    cost = %total_cost,
                    "BUY order filled"
                );

                Ok(Some(BacktestTrade {
                    timestamp: self.current_time,
                    token_id: order.token_id,
                    side: OrderSide::Buy,
                    price: current_price,
                    size: order.size,
                    realized_pnl: None,
                    close_reason: None,
                }))
            }
            OrderSide::Sell => {
                // Check if we have enough position
                let (cur_size, entry_price) = self
                    .position_entries
                    .get(&order.token_id)
                    .cloned()
                    .unwrap_or((Decimal::ZERO, Decimal::ZERO));

                if cur_size < order.size {
                    warn!(
                        token_id = %order.token_id,
                        requested = %order.size,
                        available = %cur_size,
                        "Insufficient position for SELL, skipping"
                    );
                    return Ok(None);
                }

                // Calculate revenue (price * size) - dynamic fee
                // Fee depends on order type: FOK = taker fee, GTC/GTD = maker (0%)
                let revenue = current_price * order.size;
                let fee = match order.order_type {
                    OrderType::Fok => {
                        Decimal::TWO * current_price * (Decimal::ONE - current_price)
                            * self.config.fees.taker_fee_rate * order.size
                    }
                    _ => Decimal::ZERO, // GTC/GTD = maker = 0% fee
                };
                let net_revenue = revenue - fee;

                // Calculate realized P&L
                let cost_basis = entry_price * order.size;
                let realized_pnl = net_revenue - cost_basis;

                // Add revenue to balance
                balance.available_usdc += net_revenue;

                // Update position tracking
                let new_size = cur_size - order.size;
                if new_size > Decimal::ZERO {
                    self.position_entries
                        .insert(order.token_id.clone(), (new_size, entry_price));
                } else {
                    self.position_entries.remove(&order.token_id);
                }

                // Update PositionState (remove or reduce position)
                // Find the position to update
                let position_to_update = positions
                    .open_positions
                    .iter()
                    .find(|(_, p)| p.token_id == order.token_id && p.strategy_name == self.strategy.name())
                    .map(|(id, _)| *id);

                if let Some(pos_id) = position_to_update {
                    if new_size > Decimal::ZERO {
                        if let Some(pos) = positions.open_positions.get_mut(&pos_id) {
                            pos.size = new_size;
                            pos.current_price = current_price;
                        }
                    } else {
                        positions.open_positions.remove(&pos_id);
                    }
                }

                // Record trade in Store (if available)
                if let Some(ref store) = self.store {
                    let trade = Trade {
                        id: Uuid::new_v4(),
                        order_id: Uuid::new_v4().to_string(),
                        market_id: String::new(),
                        token_id: order.token_id.clone(),
                        side: OrderSide::Sell,
                        price: current_price,
                        size: order.size,
                        realized_pnl: Some(realized_pnl),
                        strategy_name: self.strategy.name().to_string(),
                        timestamp: self.current_time,
                    };
                    store.insert_trade(&trade).await.map_err(|e| {
                        polyrust_core::error::PolyError::Execution(format!(
                            "Failed to insert trade: {}",
                            e
                        ))
                    })?;
                }

                debug!(
                    token_id = %order.token_id,
                    price = %current_price,
                    size = %order.size,
                    revenue = %net_revenue,
                    realized_pnl = %realized_pnl,
                    "SELL order filled"
                );

                Ok(Some(BacktestTrade {
                    timestamp: self.current_time,
                    token_id: order.token_id,
                    side: OrderSide::Sell,
                    price: current_price,
                    size: order.size,
                    realized_pnl: Some(realized_pnl),
                    close_reason: Some(CloseReason::Strategy),
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FeeConfig;
    use async_trait::async_trait;
    use polyrust_core::actions::Action;
    use polyrust_core::context::StrategyContext;
    use polyrust_core::error::Result;
    use polyrust_core::events::Event;
    use polyrust_core::strategy::Strategy;
    use rust_decimal_macros::dec;

    // Simple test strategy that buys on first PriceChange, sells on second
    struct TestStrategy {
        price_event_count: usize,
    }

    #[async_trait]
    impl Strategy for TestStrategy {
        fn name(&self) -> &str {
            "test-strategy"
        }

        fn description(&self) -> &str {
            "Test strategy for backtest engine"
        }

        async fn on_event(&mut self, event: &Event, _ctx: &StrategyContext) -> Result<Vec<Action>> {
            match event {
                Event::MarketData(MarketDataEvent::PriceChange { token_id, .. }) => {
                    self.price_event_count += 1;
                    if self.price_event_count == 1 {
                        // First PriceChange: BUY
                        Ok(vec![Action::PlaceOrder(OrderRequest::new(
                            token_id.clone(),
                            dec!(0.50),
                            dec!(10),
                            OrderSide::Buy,
                            OrderType::Gtc,
                            false,
                        ))])
                    } else if self.price_event_count == 2 {
                        // Second PriceChange: SELL
                        Ok(vec![Action::PlaceOrder(OrderRequest::new(
                            token_id.clone(),
                            dec!(0.60),
                            dec!(10),
                            OrderSide::Sell,
                            OrderType::Gtc,
                            false,
                        ))])
                    } else {
                        Ok(vec![])
                    }
                }
                _ => Ok(vec![]),
            }
        }
    }

    #[tokio::test]
    async fn backtest_engine_executes_buy_and_sell() {
        // Create an in-memory Store
        let store = Arc::new(Store::new(":memory:").await.unwrap());

        // Create an in-memory HistoricalDataStore
        let data_store = Arc::new(HistoricalDataStore::new(":memory:").await.unwrap());

        // Insert test price data
        data_store
            .insert_historical_prices(vec![
                crate::data::store::HistoricalPrice {
                    token_id: "token1".to_string(),
                    timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                    price: dec!(0.50),
                    source: "test".to_string(),
                },
                crate::data::store::HistoricalPrice {
                    token_id: "token1".to_string(),
                    timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                    price: dec!(0.60),
                    source: "test".to_string(),
                },
            ])
            .await
            .unwrap();

        // Create config
        let config = BacktestConfig {
            strategy_name: "test-strategy".to_string(),
            market_ids: vec!["token1".to_string()],
            start_date: DateTime::from_timestamp(500, 0).unwrap(),
            end_date: DateTime::from_timestamp(3000, 0).unwrap(),
            initial_balance: dec!(1000),
            data_fidelity_secs: 60,
            data_db_path: ":memory:".to_string(),
            fees: FeeConfig {
                taker_fee_rate: dec!(0.01),
            },
            market_duration_secs: None,

            fetch_concurrency: 10,
            offline: false,
            sweep: None,
        };

        let strategy = Box::new(TestStrategy { price_event_count: 0 });

        let mut engine = BacktestEngine::new(config, strategy, data_store, store.clone()).await;

        let trades = engine.run().await.unwrap();

        // Should have 2 trades: BUY and SELL
        assert_eq!(trades.len(), 2);

        // First trade: BUY at 0.50
        assert_eq!(trades[0].side, OrderSide::Buy);
        assert_eq!(trades[0].price, dec!(0.50));
        assert_eq!(trades[0].size, dec!(10));

        // Second trade: SELL at 0.60
        assert_eq!(trades[1].side, OrderSide::Sell);
        assert_eq!(trades[1].price, dec!(0.60));
        assert_eq!(trades[1].size, dec!(10));

        // Check realized P&L on SELL trade
        // GTC = maker = 0% fee, so:
        // BUY at 0.50, no fee → effective entry = 0.50
        // SELL at 0.60, no fee → net revenue = 6.00
        // P&L = 6.00 - 5.00 = 1.00
        assert!(trades[1].realized_pnl.is_some());
        let pnl = trades[1].realized_pnl.unwrap();
        assert_eq!(pnl, dec!(1.0)); // Exact 1.00 with 0% maker fee

        // Verify trades were recorded in Store
        let stored_trades = store.list_trades(Some("test-strategy"), 10).await.unwrap();
        assert_eq!(stored_trades.len(), 2);
    }

    #[tokio::test]
    async fn backtest_engine_sorts_events_chronologically() {
        let store = Arc::new(Store::new(":memory:").await.unwrap());
        let data_store = Arc::new(HistoricalDataStore::new(":memory:").await.unwrap());

        // Insert price data in reverse chronological order
        data_store
            .insert_historical_prices(vec![
                crate::data::store::HistoricalPrice {
                    token_id: "token1".to_string(),
                    timestamp: DateTime::from_timestamp(3000, 0).unwrap(),
                    price: dec!(0.70),
                    source: "test".to_string(),
                },
                crate::data::store::HistoricalPrice {
                    token_id: "token1".to_string(),
                    timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                    price: dec!(0.50),
                    source: "test".to_string(),
                },
                crate::data::store::HistoricalPrice {
                    token_id: "token1".to_string(),
                    timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                    price: dec!(0.60),
                    source: "test".to_string(),
                },
            ])
            .await
            .unwrap();

        let config = BacktestConfig {
            strategy_name: "test-strategy".to_string(),
            market_ids: vec!["token1".to_string()],
            start_date: DateTime::from_timestamp(500, 0).unwrap(),
            end_date: DateTime::from_timestamp(4000, 0).unwrap(),
            initial_balance: dec!(1000),
            data_fidelity_secs: 60,
            data_db_path: ":memory:".to_string(),
            fees: FeeConfig {
                taker_fee_rate: dec!(0.01),
            },
            market_duration_secs: None,

            fetch_concurrency: 10,
            offline: false,
            sweep: None,
        };

        let strategy = Box::new(TestStrategy { price_event_count: 0 });
        let mut engine = BacktestEngine::new(config, strategy, data_store, store).await;

        let trades = engine.run().await.unwrap();

        // Strategy should receive events in chronological order
        // First event at t=1000 (0.50) -> BUY
        // Second event at t=2000 (0.60) -> SELL
        assert_eq!(trades[0].timestamp.timestamp(), 1000);
        assert_eq!(trades[0].price, dec!(0.50));
        assert_eq!(trades[1].timestamp.timestamp(), 2000);
        assert_eq!(trades[1].price, dec!(0.60));
    }

    #[tokio::test]
    async fn backtest_engine_insufficient_balance_skips_order() {
        let store = Arc::new(Store::new(":memory:").await.unwrap());
        let data_store = Arc::new(HistoricalDataStore::new(":memory:").await.unwrap());

        data_store
            .insert_historical_prices(vec![crate::data::store::HistoricalPrice {
                token_id: "token1".to_string(),
                timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                price: dec!(0.50),
                source: "test".to_string(),
            }])
            .await
            .unwrap();

        let config = BacktestConfig {
            strategy_name: "test-strategy".to_string(),
            market_ids: vec!["token1".to_string()],
            start_date: DateTime::from_timestamp(500, 0).unwrap(),
            end_date: DateTime::from_timestamp(2000, 0).unwrap(),
            initial_balance: dec!(1.0), // Insufficient for 0.50 * 10 = 5.00 + fee
            data_fidelity_secs: 60,
            data_db_path: ":memory:".to_string(),
            fees: FeeConfig {
                taker_fee_rate: dec!(0.01),
            },
            market_duration_secs: None,

            fetch_concurrency: 10,
            offline: false,
            sweep: None,
        };

        let strategy = Box::new(TestStrategy { price_event_count: 0 });
        let mut engine = BacktestEngine::new(config, strategy, data_store, store).await;

        let trades = engine.run().await.unwrap();

        // Should have 0 trades (BUY was skipped due to insufficient balance)
        assert_eq!(trades.len(), 0);
    }

    #[test]
    fn synthesize_price_events_empty_input() {
        let result = synthesize_price_events_from_trades(&[], 5);
        assert!(result.is_empty());
    }

    #[test]
    fn synthesize_price_events_from_trades_basic() {
        // 10 trades over 30 seconds at 5-second fidelity -> 6 buckets
        let base_ts = 1000i64;
        let trades: Vec<HistoricalEvent> = (0..10)
            .map(|i| {
                let ts = base_ts + (i * 3); // trades at 0, 3, 6, 9, 12, 15, 18, 21, 24, 27s
                HistoricalEvent {
                    timestamp: DateTime::from_timestamp(ts, 0).unwrap(),
                    token_id: "token_a".to_string(),
                    event: Event::MarketData(MarketDataEvent::Trade {
                        token_id: "token_a".to_string(),
                        price: dec!(0.50) + Decimal::new(i, 2), // 0.50, 0.51, ..., 0.59
                        size: dec!(10),
                        timestamp: DateTime::from_timestamp(ts, 0).unwrap(),
                    }),
                }
            })
            .collect();

        let result = synthesize_price_events_from_trades(&trades, 5);

        // Buckets (5s): [1000-1005), [1005-1010), [1010-1015), [1015-1020), [1020-1025), [1025-1030)
        // Trade at t=1000 -> bucket 1000, t=1003 -> bucket 1000, t=1006 -> bucket 1005, ...
        assert_eq!(result.len(), 6, "Expected 6 buckets for 10 trades over 30s at 5s fidelity");

        // All should be PriceChange events
        for event in &result {
            assert!(matches!(
                &event.event,
                Event::MarketData(MarketDataEvent::PriceChange { .. })
            ));
        }

        // Timestamps should be at bucket ends (bucket_start + fidelity)
        let mut timestamps: Vec<i64> = result.iter().map(|e| e.timestamp.timestamp()).collect();
        timestamps.sort();
        assert_eq!(timestamps, vec![1005, 1010, 1015, 1020, 1025, 1030]);

        // Last trade in first bucket (t=1000, t=1003) should have price from t=1003 trade
        // Bucket 1000 has trades at i=0 (t=1000, p=0.50) and i=1 (t=1003, p=0.51)
        // Last inserted wins in BTreeMap: i=1 at bucket_start=1000
        let first_bucket = result
            .iter()
            .find(|e| e.timestamp.timestamp() == 1005)
            .unwrap();
        if let Event::MarketData(MarketDataEvent::PriceChange { price, .. }) = &first_bucket.event {
            assert_eq!(*price, dec!(0.51));
        } else {
            panic!("Expected PriceChange event");
        }
    }

    #[test]
    fn synthesize_price_events_multiple_tokens() {
        let trades = vec![
            HistoricalEvent {
                timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                token_id: "token_a".to_string(),
                event: Event::MarketData(MarketDataEvent::Trade {
                    token_id: "token_a".to_string(),
                    price: dec!(0.50),
                    size: dec!(10),
                    timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                }),
            },
            HistoricalEvent {
                timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                token_id: "token_b".to_string(),
                event: Event::MarketData(MarketDataEvent::Trade {
                    token_id: "token_b".to_string(),
                    price: dec!(0.30),
                    size: dec!(5),
                    timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                }),
            },
            HistoricalEvent {
                timestamp: DateTime::from_timestamp(1007, 0).unwrap(),
                token_id: "token_a".to_string(),
                event: Event::MarketData(MarketDataEvent::Trade {
                    token_id: "token_a".to_string(),
                    price: dec!(0.55),
                    size: dec!(10),
                    timestamp: DateTime::from_timestamp(1007, 0).unwrap(),
                }),
            },
        ];

        let result = synthesize_price_events_from_trades(&trades, 5);

        // token_a: buckets at 1000 (trade t=1000) and 1005 (trade t=1007) -> 2 events
        // token_b: bucket at 1000 (trade t=1000) -> 1 event
        assert_eq!(result.len(), 3);

        let token_a_events: Vec<_> = result.iter().filter(|e| e.token_id == "token_a").collect();
        let token_b_events: Vec<_> = result.iter().filter(|e| e.token_id == "token_b").collect();
        assert_eq!(token_a_events.len(), 2);
        assert_eq!(token_b_events.len(), 1);

        // Verify token_b price
        if let Event::MarketData(MarketDataEvent::PriceChange { price, .. }) =
            &token_b_events[0].event
        {
            assert_eq!(*price, dec!(0.30));
        } else {
            panic!("Expected PriceChange event");
        }
    }

    // Strategy that counts PriceChange events it receives
    struct PriceCountStrategy {
        price_change_count: usize,
        trade_count: usize,
    }

    #[async_trait]
    impl Strategy for PriceCountStrategy {
        fn name(&self) -> &str {
            "price-count-strategy"
        }

        fn description(&self) -> &str {
            "Counts PriceChange and Trade events"
        }

        async fn on_event(&mut self, event: &Event, _ctx: &StrategyContext) -> Result<Vec<Action>> {
            match event {
                Event::MarketData(MarketDataEvent::PriceChange { .. }) => {
                    self.price_change_count += 1;
                }
                Event::MarketData(MarketDataEvent::Trade { .. }) => {
                    self.trade_count += 1;
                }
                _ => {}
            }
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn backtest_engine_sub_minute_fidelity_synthesizes_prices() {
        let store = Arc::new(Store::new(":memory:").await.unwrap());
        let data_store = Arc::new(HistoricalDataStore::new(":memory:").await.unwrap());

        // Insert trade data only (no price history — simulating sub-minute mode)
        let trades: Vec<crate::data::store::HistoricalTrade> = (0..6)
            .map(|i| crate::data::store::HistoricalTrade {
                id: format!("trade_{}", i),
                token_id: "token1".to_string(),
                timestamp: DateTime::from_timestamp(1000 + i * 3, 0).unwrap(),
                price: dec!(0.50) + Decimal::new(i, 2),
                size: dec!(10),
                side: "buy".to_string(),
                source: "subgraph".to_string(),
            })
            .collect();

        data_store.insert_historical_trades(trades).await.unwrap();

        let config = BacktestConfig {
            strategy_name: "price-count-strategy".to_string(),
            market_ids: vec!["token1".to_string()],
            start_date: DateTime::from_timestamp(900, 0).unwrap(),
            end_date: DateTime::from_timestamp(1100, 0).unwrap(),
            initial_balance: dec!(1000),
            data_fidelity_secs: 5, // Sub-minute!
            data_db_path: ":memory:".to_string(),
            fees: FeeConfig {
                taker_fee_rate: dec!(0.01),
            },
            market_duration_secs: None,

            fetch_concurrency: 10,
            offline: false,
            sweep: None,
        };

        let strategy = Box::new(PriceCountStrategy {
            price_change_count: 0,
            trade_count: 0,
        });

        let mut engine = BacktestEngine::new(config, strategy, data_store, store).await;
        let _trades = engine.run().await.unwrap();

        // Strategy should have received both Trade events and synthesized PriceChange events
        // Downcast to check counts — access via the engine's strategy field
        // Since we can't downcast easily, verify indirectly: the engine should have
        // price entries in token_prices from synthesized PriceChange events
        assert!(
            engine.token_prices.contains_key("token1"),
            "Token prices should be populated from synthesized PriceChange events"
        );

        // Verify the final price is from the last trade
        let final_price = engine.token_prices.get("token1").unwrap();
        assert_eq!(*final_price, dec!(0.55)); // Last trade: 0.50 + 0.05
    }
}
