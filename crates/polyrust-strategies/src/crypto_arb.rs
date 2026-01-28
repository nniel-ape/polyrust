use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as FmtWrite;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tracing::{info, warn};

use polyrust_core::prelude::*;
use polyrust_market::ChainlinkHistoricalClient;

/// Escape a string for safe inclusion in HTML content.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Fee model configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FeeConfig {
    /// Taker fee rate (default 0.0315 = 3.15% at 50/50).
    pub taker_fee_rate: Decimal,
}

impl Default for FeeConfig {
    fn default() -> Self {
        Self {
            taker_fee_rate: Decimal::new(315, 4), // 0.0315
        }
    }
}

/// Spike detection configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpikeConfig {
    /// Minimum price change percentage to count as a spike.
    pub threshold_pct: Decimal,
    /// Lookback window in seconds for spike detection.
    pub window_secs: u64,
    /// Maximum number of spike events to retain.
    pub history_size: usize,
}

impl Default for SpikeConfig {
    fn default() -> Self {
        Self {
            threshold_pct: Decimal::new(5, 3), // 0.005
            window_secs: 10,
            history_size: 50,
        }
    }
}

/// Hybrid order mode configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OrderConfig {
    /// Use GTC maker orders for Confirmed/TwoSided modes.
    pub hybrid_mode: bool,
    /// Price offset below best ask for GTC limit orders.
    pub limit_offset: Decimal,
    /// Cancel stale GTC orders after this many seconds.
    pub max_age_secs: u64,
}

impl Default for OrderConfig {
    fn default() -> Self {
        Self {
            hybrid_mode: true,
            limit_offset: Decimal::new(1, 2), // 0.01
            max_age_secs: 30,
        }
    }
}

/// Position sizing configuration (Kelly criterion).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SizingConfig {
    /// Base position size in USDC.
    pub base_size: Decimal,
    /// Kelly fraction multiplier (fractional Kelly).
    pub kelly_multiplier: Decimal,
    /// Minimum position size in USDC.
    pub min_size: Decimal,
    /// Maximum position size in USDC.
    pub max_size: Decimal,
    /// Whether to use Kelly sizing (vs fixed).
    pub use_kelly: bool,
}

impl Default for SizingConfig {
    fn default() -> Self {
        Self {
            base_size: Decimal::new(10, 0),
            kelly_multiplier: Decimal::new(25, 2), // 0.25
            min_size: Decimal::new(2, 0),
            max_size: Decimal::new(25, 0),
            use_kelly: true,
        }
    }
}

/// Stop-loss configuration (dual-trigger + trailing).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StopLossConfig {
    /// Crypto price reversal percentage trigger (e.g. 0.005 = 0.5%).
    pub reversal_pct: Decimal,
    /// Minimum market price drop to confirm stop-loss (e.g. 0.05 = 5¢).
    pub min_drop: Decimal,
    /// Enable trailing stop-loss.
    pub trailing_enabled: bool,
    /// Trailing stop distance from peak bid.
    pub trailing_distance: Decimal,
    /// Tighten trailing distance as time remaining decreases.
    pub time_decay: bool,
}

impl Default for StopLossConfig {
    fn default() -> Self {
        Self {
            reversal_pct: Decimal::new(5, 3),  // 0.005
            min_drop: Decimal::new(5, 2),      // 0.05
            trailing_enabled: true,
            trailing_distance: Decimal::new(3, 2), // 0.03
            time_decay: true,
        }
    }
}

/// Cross-market correlation configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CorrelationConfig {
    /// Enable cross-market correlation signals.
    pub enabled: bool,
    /// Minimum spike percentage in leader coin to trigger follower signals.
    pub min_spike_pct: Decimal,
    /// Leader → follower coin pairs (e.g. BTC → [ETH, SOL]).
    pub pairs: Vec<(String, Vec<String>)>,
}

impl Default for CorrelationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_spike_pct: Decimal::new(1, 2), // 0.01
            pairs: vec![
                ("BTC".into(), vec!["ETH".into(), "SOL".into()]),
                ("ETH".into(), vec!["SOL".into()]),
            ],
        }
    }
}

/// Performance tracking and auto-disable configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PerformanceConfig {
    /// Minimum trades before auto-disable can trigger.
    pub min_trades: u64,
    /// Minimum win rate to keep a mode enabled.
    pub min_win_rate: Decimal,
    /// Rolling window size for recent P&L tracking.
    pub window_size: usize,
    /// Automatically disable modes with poor performance.
    pub auto_disable: bool,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            min_trades: 20,
            min_win_rate: Decimal::new(40, 2), // 0.40
            window_size: 50,
            auto_disable: false,
        }
    }
}

/// Configuration for the crypto arbitrage strategy.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArbitrageConfig {
    /// Coins to track (e.g. ["BTC", "ETH", "SOL", "XRP"])
    pub coins: Vec<String>,
    /// Maximum concurrent positions
    pub max_positions: usize,
    /// Minimum profit margin for confirmed mode
    pub min_profit_margin: Decimal,
    /// Minimum profit margin in late window (120-300s)
    pub late_window_margin: Decimal,
    /// Interval in seconds between market discovery scans
    pub scan_interval_secs: u64,
    /// Whether to use on-chain Chainlink RPC for resolution reference price
    pub use_chainlink: bool,
    /// Polygon RPC endpoints for on-chain Chainlink lookups (tried in order).
    /// Only used when `use_chainlink` is true.
    #[serde(default = "default_chainlink_rpc_urls")]
    pub chainlink_rpc_urls: Vec<String>,
    /// Fee model configuration.
    #[serde(default)]
    pub fee: FeeConfig,
    /// Spike detection configuration.
    #[serde(default)]
    pub spike: SpikeConfig,
    /// Hybrid order mode configuration.
    #[serde(default)]
    pub order: OrderConfig,
    /// Position sizing configuration.
    #[serde(default)]
    pub sizing: SizingConfig,
    /// Stop-loss configuration.
    #[serde(default)]
    pub stop_loss: StopLossConfig,
    /// Cross-market correlation configuration.
    #[serde(default)]
    pub correlation: CorrelationConfig,
    /// Performance tracking configuration.
    #[serde(default)]
    pub performance: PerformanceConfig,
}

fn default_chainlink_rpc_urls() -> Vec<String> {
    vec!["https://polygon-rpc.com".to_string()]
}

impl Default for ArbitrageConfig {
    fn default() -> Self {
        Self {
            coins: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            max_positions: 5,
            min_profit_margin: Decimal::new(3, 2),      // 0.03
            late_window_margin: Decimal::new(2, 2),     // 0.02
            scan_interval_secs: 30,
            use_chainlink: true,
            chainlink_rpc_urls: default_chainlink_rpc_urls(),
            fee: FeeConfig::default(),
            spike: SpikeConfig::default(),
            order: OrderConfig::default(),
            sizing: SizingConfig::default(),
            stop_loss: StopLossConfig::default(),
            correlation: CorrelationConfig::default(),
            performance: PerformanceConfig::default(),
        }
    }
}

/// Parse a unix timestamp from a slug suffix (e.g. `btc-updown-15m-1706000000` → timestamp).
/// Returns `None` if the slug doesn't end with a valid unix timestamp.
fn parse_slug_timestamp(slug: &str) -> Option<i64> {
    let last_segment = slug.rsplit('-').next()?;
    let ts: i64 = last_segment.parse().ok()?;
    // Sanity: must be a reasonable unix timestamp (after 2020)
    if ts > 1_577_836_800 {
        Some(ts)
    } else {
        None
    }
}

/// Format a USD price with 2 decimal places and thousands separators (e.g. `$88,959.37`).
fn fmt_usd(price: Decimal) -> String {
    let rounded = price.round_dp(2);
    let s = format!("{:.2}", rounded);
    // Split on decimal point and add thousands separators to integer part
    let parts: Vec<&str> = s.split('.').collect();
    let int_part = parts[0];
    let dec_part = parts.get(1).unwrap_or(&"00");

    let negative = int_part.starts_with('-');
    let digits: &str = if negative { &int_part[1..] } else { int_part };

    let with_commas: String = digits
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<&str>>()
        .join(",");

    if negative {
        format!("$-{}.{}", with_commas, dec_part)
    } else {
        format!("${}.{}", with_commas, dec_part)
    }
}

/// Format a market probability price with 2 decimal places (e.g. `0.50`).
fn fmt_market_price(price: Decimal) -> String {
    format!("{:.2}", price.round_dp(2))
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// How accurately the reference price matches the market's actual start-of-window price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceQuality {
    /// On-chain Chainlink RPC lookup — the exact settlement price Polymarket uses.
    OnChain,
    /// Boundary snapshot captured within 2s of window start (best real-time).
    Exact,
    /// Closest historical price entry; staleness in seconds from window start.
    Historical(u64),
    /// Price at discovery time — existing fallback behavior (least accurate).
    Current,
}

impl ReferenceQuality {
    /// Confidence discount factor based on reference accuracy.
    /// OnChain = 1.0 (exact settlement price), Exact = 1.0,
    /// Historical(<5s) = 0.95, Historical(>=5s) = 0.85, Current = 0.70.
    pub fn quality_factor(&self) -> Decimal {
        match self {
            ReferenceQuality::OnChain => Decimal::ONE,
            ReferenceQuality::Exact => Decimal::ONE,
            ReferenceQuality::Historical(s) if *s < 5 => Decimal::new(95, 2),
            ReferenceQuality::Historical(_) => Decimal::new(85, 2),
            ReferenceQuality::Current => Decimal::new(70, 2),
        }
    }
}

/// A price snapshot captured at a 15-minute window boundary.
#[derive(Debug, Clone)]
pub struct BoundarySnapshot {
    pub timestamp: DateTime<Utc>,
    pub price: Decimal,
    /// Price source (e.g. "chainlink", "binance")
    pub source: String,
}

/// Market enriched with the reference crypto price at discovery time.
#[derive(Debug, Clone)]
pub struct MarketWithReference {
    pub market: MarketInfo,
    /// Crypto price at the moment the market was discovered
    pub reference_price: Decimal,
    /// How accurately the reference price matches the window start price.
    pub reference_quality: ReferenceQuality,
    pub discovery_time: DateTime<Utc>,
    /// Coin symbol (e.g. "BTC")
    pub coin: String,
}

impl MarketWithReference {
    /// Predict the winning outcome based on current price vs reference.
    /// Returns `None` when price equals reference (no directional signal).
    pub fn predict_winner(&self, current_price: Decimal) -> Option<OutcomeSide> {
        if current_price > self.reference_price {
            Some(OutcomeSide::Up)
        } else if current_price < self.reference_price {
            Some(OutcomeSide::Down)
        } else {
            None
        }
    }

    /// Multi-signal confidence score in [0, 1].
    ///
    /// Three regimes based on time remaining:
    /// - Tail-end (< 120s, market >= 0.90): confidence 1.0
    /// - Late window (120-300s): distance-weighted with market boost
    /// - Early window (> 300s): distance-weighted, lower base
    ///
    /// The raw confidence is then discounted by `reference_quality.quality_factor()`
    /// to reflect how accurately the reference price matches the window start price.
    pub fn get_confidence(
        &self,
        current_price: Decimal,
        market_price: Decimal,
        time_remaining_secs: i64,
    ) -> Decimal {
        let distance_pct = if self.reference_price.is_zero() {
            Decimal::ZERO
        } else {
            ((current_price - self.reference_price) / self.reference_price).abs()
        };

        let raw = if time_remaining_secs < 120 && market_price >= Decimal::new(90, 2) {
            // Tail-end: highest confidence — quality factor still applies
            Decimal::ONE
        } else if time_remaining_secs < 300 {
            // Late window
            let base = distance_pct * Decimal::new(66, 0);
            let market_boost =
                Decimal::ONE + (market_price - Decimal::new(50, 2)) * Decimal::new(5, 1);
            (base * market_boost).min(Decimal::ONE)
        } else {
            // Early window
            (distance_pct * Decimal::new(50, 0)).min(Decimal::ONE)
        };

        (raw * self.reference_quality.quality_factor()).min(Decimal::ONE)
    }
}

// ---------------------------------------------------------------------------
// Fee helpers
// ---------------------------------------------------------------------------

/// Compute the Polymarket taker fee per share at a given probability price.
///
/// Formula: `2 * p * (1 - p) * rate`
/// At p=0.50, fee = 0.50 * rate. At p=0.95, fee ≈ 0.095 * rate.
pub fn taker_fee(price: Decimal, rate: Decimal) -> Decimal {
    Decimal::new(2, 0) * price * (Decimal::ONE - price) * rate
}

/// Compute the net profit margin for an entry at `entry_price`, assuming the
/// winning outcome resolves to $1.
///
/// - Gross margin = `1 - entry_price`
/// - Entry fee: taker fee for taker orders, $0 for maker (GTC) orders
/// - Exit fee: ~$0 (resolution at $1 has negligible fee)
///
/// Returns `gross_margin - entry_fee`.
pub fn net_profit_margin(entry_price: Decimal, fee_rate: Decimal, is_maker: bool) -> Decimal {
    let gross = Decimal::ONE - entry_price;
    if is_maker {
        gross // Maker fee = $0
    } else {
        gross - taker_fee(entry_price, fee_rate)
    }
}

/// Three arbitrage trading modes, ordered by priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArbitrageMode {
    /// < 2 min remaining, market price >= 90%
    TailEnd,
    /// Both outcomes priced below combined $0.98 (guaranteed profit)
    TwoSided,
    /// Standard directional with dynamic confidence
    Confirmed,
}

/// A detected arbitrage opportunity ready for execution.
#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    pub mode: ArbitrageMode,
    pub market_id: MarketId,
    pub outcome_to_buy: OutcomeSide,
    pub token_id: TokenId,
    pub buy_price: Decimal,
    pub confidence: Decimal,
    pub profit_margin: Decimal,
    /// Estimated taker fee per share at entry (0 for maker/GTC orders).
    pub estimated_fee: Decimal,
    /// Net profit margin after fees: `profit_margin - estimated_fee`.
    pub net_margin: Decimal,
}

/// Tracks an active arbitrage position.
#[derive(Debug, Clone)]
pub struct ArbitragePosition {
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub side: OutcomeSide,
    pub entry_price: Decimal,
    pub size: Decimal,
    pub reference_price: Decimal,
    pub coin: String,
    pub order_id: Option<OrderId>,
    pub entry_time: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Strategy
// ---------------------------------------------------------------------------

/// A pending order awaiting confirmation from the execution backend.
#[derive(Debug, Clone)]
struct PendingOrder {
    market_id: MarketId,
    token_id: TokenId,
    side: OutcomeSide,
    price: Decimal,
    size: Decimal,
    reference_price: Decimal,
    coin: String,
}

/// Crypto arbitrage strategy that exploits mispricing in 15-minute Up/Down
/// crypto prediction markets on Polymarket.
/// Number of price history entries to keep per coin.
/// At ~5s RTDS intervals, 200 entries covers ~16 minutes — enough for a full
/// 15-minute window plus discovery delay.
const PRICE_HISTORY_SIZE: usize = 200;

/// Maximum time (seconds) from a window boundary to consider a snapshot "exact".
const BOUNDARY_TOLERANCE_SECS: i64 = 2;

/// 15 minutes in seconds (window duration).
const WINDOW_SECS: i64 = 900;

pub struct CryptoArbitrageStrategy {
    config: ArbitrageConfig,
    /// On-chain Chainlink RPC client for exact settlement price lookups.
    /// `None` when `config.use_chainlink` is false.
    chainlink_client: Option<Arc<ChainlinkHistoricalClient>>,
    active_markets: HashMap<MarketId, MarketWithReference>,
    /// Price history per coin: (timestamp, price, source).
    /// Kept at PRICE_HISTORY_SIZE entries for retroactive reference lookup.
    price_history: HashMap<String, VecDeque<(DateTime<Utc>, Decimal, String)>>,
    /// Proactive price snapshots at 15-min window boundaries, keyed by "{COIN}-{unix_ts}".
    boundary_prices: HashMap<String, BoundarySnapshot>,
    positions: HashMap<MarketId, Vec<ArbitragePosition>>,
    /// Orders submitted but not yet confirmed — keyed by token_id.
    /// Prevents re-entry while orders are in flight.
    pending_orders: HashMap<TokenId, PendingOrder>,
    /// Token IDs with active stop-loss sell orders awaiting confirmation.
    /// Positions are only removed once the sell is confirmed or rejected.
    pending_stop_loss: HashSet<TokenId>,
    last_scan: Option<tokio::time::Instant>,
    /// Throttle for dashboard-update signal emission (~5 seconds).
    last_dashboard_emit: Option<tokio::time::Instant>,
    /// Cached best-ask prices per token_id, updated on orderbook events.
    /// Used by render_view() to display UP/DOWN market prices.
    cached_asks: HashMap<TokenId, Decimal>,
    /// Markets discovered before prices were available, keyed by coin.
    /// Promoted to active_markets once a price arrives for the coin.
    pending_discovery: HashMap<String, MarketInfo>,
}

impl CryptoArbitrageStrategy {
    pub fn new(config: ArbitrageConfig) -> Self {
        let chainlink_client = if config.use_chainlink {
            Some(Arc::new(ChainlinkHistoricalClient::new(
                config.chainlink_rpc_urls.clone(),
            )))
        } else {
            None
        };

        Self {
            config,
            chainlink_client,
            active_markets: HashMap::new(),
            price_history: HashMap::new(),
            boundary_prices: HashMap::new(),
            positions: HashMap::new(),
            pending_orders: HashMap::new(),
            pending_stop_loss: HashSet::new(),
            last_scan: None,
            last_dashboard_emit: None,
            cached_asks: HashMap::new(),
            pending_discovery: HashMap::new(),
        }
    }

    // -- Event handlers -----------------------------------------------------

    async fn on_crypto_price(
        &mut self,
        symbol: &str,
        price: Decimal,
        source: &str,
        ctx: &StrategyContext,
    ) -> Result<Vec<Action>> {
        let now = Utc::now();

        // Record price history with source (keep last PRICE_HISTORY_SIZE entries)
        let history = self.price_history.entry(symbol.to_string()).or_default();
        history.push_back((now, price, source.to_string()));
        if history.len() > PRICE_HISTORY_SIZE {
            history.pop_front();
        }

        // Capture boundary snapshot if we just crossed a 15-min boundary.
        // A boundary is at :00, :15, :30, :45 of each hour.
        let ts = now.timestamp();
        let boundary_ts = ts - (ts % WINDOW_SECS);
        let secs_from_boundary = ts - boundary_ts;
        if secs_from_boundary.unsigned_abs() <= BOUNDARY_TOLERANCE_SECS as u64 {
            let key = format!("{symbol}-{boundary_ts}");
            // Only record if we haven't already (prefer Chainlink source)
            let should_insert = match self.boundary_prices.get(&key) {
                None => true,
                Some(existing) => {
                    // Prefer chainlink over other sources
                    source.eq_ignore_ascii_case("chainlink")
                        && !existing.source.eq_ignore_ascii_case("chainlink")
                }
            };
            if should_insert {
                self.boundary_prices.insert(
                    key.clone(),
                    BoundarySnapshot {
                        timestamp: now,
                        price,
                        source: source.to_string(),
                    },
                );
                info!(
                    coin = %symbol,
                    boundary_ts = boundary_ts,
                    price = %price,
                    source = %source,
                    "Captured boundary price snapshot"
                );
            }
            // Prune old boundary snapshots (keep last 4 per coin = 1 hour)
            self.prune_boundary_snapshots(symbol);
        }

        let mut actions = Vec::new();

        // Promote any pending market for this coin now that we have a price
        if let Some(market) = self.pending_discovery.remove(symbol) {
            let window_ts = market
                .start_date
                .map(|d| d.timestamp())
                .or_else(|| parse_slug_timestamp(&market.slug))
                .unwrap_or(boundary_ts);
            let (reference_price, reference_quality) =
                self.find_best_reference(symbol, window_ts, price).await;
            let mwr = MarketWithReference {
                market: market.clone(),
                reference_price,
                reference_quality,
                discovery_time: Utc::now(),
                coin: symbol.to_string(),
            };
            info!(
                coin = %symbol,
                market = %market.id,
                reference = %reference_price,
                quality = ?reference_quality,
                "Activated buffered market (price now available)"
            );
            self.active_markets.insert(market.id.clone(), mwr);
            actions.push(Action::SubscribeMarket(market.id.clone()));
        }

        // Evaluate each active market for this coin
        let matching_market_ids: Vec<MarketId> = self
            .active_markets
            .iter()
            .filter(|(_, m)| m.coin == symbol)
            .map(|(id, _)| id.clone())
            .collect();

        for market_id in matching_market_ids {
            let market = match self.active_markets.get(&market_id) {
                Some(m) => m.clone(),
                None => continue,
            };

            let opps = self.evaluate_opportunity(&market, price, ctx).await?;
            let total_positions: usize = self.positions.values().map(|v| v.len()).sum();
            let total_pending = self.pending_orders.len();
            if !opps.is_empty()
                && (total_positions + total_pending + opps.len()) <= self.config.max_positions
            {
                // For TwoSided mode, compute equal share count across both outcomes
                // so total cost = position_size and each side gets N shares.
                let two_sided_size = if opps.len() == 2 && opps[0].mode == ArbitrageMode::TwoSided {
                    let combined_price = opps[0].buy_price + opps[1].buy_price;
                    if combined_price > Decimal::ZERO {
                        Some(self.config.sizing.base_size / combined_price)
                    } else {
                        None
                    }
                } else {
                    None
                };

                for opp in &opps {
                    if opp.buy_price.is_zero() {
                        warn!(market = %market_id, "skipping opportunity with zero buy_price");
                        continue;
                    }
                    let size =
                        two_sided_size.unwrap_or_else(|| self.config.sizing.base_size / opp.buy_price);
                    let order = OrderRequest {
                        token_id: opp.token_id.clone(),
                        price: opp.buy_price,
                        size,
                        side: OrderSide::Buy,
                        order_type: OrderType::Fok,
                        neg_risk: false,
                    };
                    info!(
                        mode = ?opp.mode,
                        market = %market_id,
                        confidence = %opp.confidence,
                        price = %opp.buy_price,
                        side = ?opp.outcome_to_buy,
                        "Submitting arbitrage order"
                    );
                    // Track pending order — position recorded only on confirmed fill
                    self.pending_orders.insert(
                        opp.token_id.clone(),
                        PendingOrder {
                            market_id: market_id.clone(),
                            token_id: opp.token_id.clone(),
                            side: opp.outcome_to_buy,
                            price: opp.buy_price,
                            size,
                            reference_price: market.reference_price,
                            coin: market.coin.clone(),
                        },
                    );
                    actions.push(Action::PlaceOrder(order));
                }
            }
        }

        Ok(actions)
    }

    /// Evaluate opportunity across three modes in priority order.
    /// Returns zero or more opportunities. TwoSided mode returns two (one per outcome).
    async fn evaluate_opportunity(
        &self,
        market: &MarketWithReference,
        current_price: Decimal,
        ctx: &StrategyContext,
    ) -> Result<Vec<ArbitrageOpportunity>> {
        let time_remaining = market.market.seconds_remaining();

        // Skip ended or almost-ended markets
        if time_remaining <= 0 {
            return Ok(vec![]);
        }

        // Already have a position or pending order in this market
        if self.positions.contains_key(&market.market.id) {
            return Ok(vec![]);
        }
        // Check if any pending orders target this market's tokens
        if self
            .pending_orders
            .values()
            .any(|p| p.market_id == market.market.id)
        {
            return Ok(vec![]);
        }

        let md = ctx.market_data.read().await;

        let up_ask = md
            .orderbooks
            .get(&market.market.token_ids.outcome_a)
            .and_then(|ob| ob.best_ask());
        let down_ask = md
            .orderbooks
            .get(&market.market.token_ids.outcome_b)
            .and_then(|ob| ob.best_ask());

        // 1. Tail-End mode: < 120s remaining + predicted winner ask >= 0.90
        if time_remaining < 120
            && let Some(predicted) = market.predict_winner(current_price)
        {
            let (token_id, ask) = match predicted {
                OutcomeSide::Up | OutcomeSide::Yes => (&market.market.token_ids.outcome_a, up_ask),
                OutcomeSide::Down | OutcomeSide::No => {
                    (&market.market.token_ids.outcome_b, down_ask)
                }
            };
            if let Some(ask_price) = ask
                && ask_price >= Decimal::new(90, 2)
            {
                let profit_margin = Decimal::ONE - ask_price;
                let estimated_fee = taker_fee(ask_price, self.config.fee.taker_fee_rate);
                let net_margin = profit_margin - estimated_fee;
                return Ok(vec![ArbitrageOpportunity {
                    mode: ArbitrageMode::TailEnd,
                    market_id: market.market.id.clone(),
                    outcome_to_buy: predicted,
                    token_id: token_id.clone(),
                    buy_price: ask_price,
                    confidence: Decimal::ONE,
                    profit_margin,
                    estimated_fee,
                    net_margin,
                }]);
            }
        }

        // 2. Two-Sided mode: sum of both asks < 0.98 — buy BOTH outcomes
        //    for guaranteed profit (one resolves to $1, the other to $0,
        //    total cost < $1 so net profit = 1 - combined).
        if let (Some(ua), Some(da)) = (up_ask, down_ask) {
            let combined = ua + da;
            if combined < Decimal::new(98, 2) {
                let profit_margin = Decimal::ONE - combined;
                // Fees on both legs (taker orders)
                let fee_up = taker_fee(ua, self.config.fee.taker_fee_rate);
                let fee_down = taker_fee(da, self.config.fee.taker_fee_rate);
                let total_fee = fee_up + fee_down;
                let net_margin = profit_margin - total_fee;
                // Skip if net margin is negative after fees
                if net_margin <= Decimal::ZERO {
                    return Ok(vec![]);
                }
                return Ok(vec![
                    ArbitrageOpportunity {
                        mode: ArbitrageMode::TwoSided,
                        market_id: market.market.id.clone(),
                        outcome_to_buy: OutcomeSide::Up,
                        token_id: market.market.token_ids.outcome_a.clone(),
                        buy_price: ua,
                        confidence: Decimal::ONE,
                        profit_margin,
                        estimated_fee: fee_up,
                        net_margin,
                    },
                    ArbitrageOpportunity {
                        mode: ArbitrageMode::TwoSided,
                        market_id: market.market.id.clone(),
                        outcome_to_buy: OutcomeSide::Down,
                        token_id: market.market.token_ids.outcome_b.clone(),
                        buy_price: da,
                        confidence: Decimal::ONE,
                        profit_margin,
                        estimated_fee: fee_down,
                        net_margin,
                    },
                ]);
            }
        }

        // 3. Confirmed mode: confidence >= threshold + sufficient margin
        if let Some(predicted) = market.predict_winner(current_price) {
            let (token_id, ask) = match predicted {
                OutcomeSide::Up | OutcomeSide::Yes => (&market.market.token_ids.outcome_a, up_ask),
                OutcomeSide::Down | OutcomeSide::No => {
                    (&market.market.token_ids.outcome_b, down_ask)
                }
            };

            if let Some(ask_price) = ask {
                let confidence = market.get_confidence(current_price, ask_price, time_remaining);
                let profit_margin = Decimal::ONE - ask_price;
                let estimated_fee = taker_fee(ask_price, self.config.fee.taker_fee_rate);
                let net_margin = profit_margin - estimated_fee;
                let min_margin = if time_remaining < 300 {
                    self.config.late_window_margin
                } else {
                    self.config.min_profit_margin
                };

                if confidence >= Decimal::new(50, 2) && net_margin >= min_margin {
                    return Ok(vec![ArbitrageOpportunity {
                        mode: ArbitrageMode::Confirmed,
                        market_id: market.market.id.clone(),
                        outcome_to_buy: predicted,
                        token_id: token_id.clone(),
                        buy_price: ask_price,
                        confidence,
                        profit_margin,
                        estimated_fee,
                        net_margin,
                    }]);
                }
            }
        }

        Ok(vec![])
    }

    async fn on_orderbook_update(
        &mut self,
        snapshot: &OrderbookSnapshot,
        ctx: &StrategyContext,
    ) -> Result<Vec<Action>> {
        // Update market data in shared context
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks
                .insert(snapshot.token_id.clone(), snapshot.clone());
        }

        // Cache best-ask price for dashboard display
        if let Some(best_ask) = snapshot.asks.first() {
            self.cached_asks
                .insert(snapshot.token_id.clone(), best_ask.price);
        }

        // Check stop-losses on open positions
        let mut actions = Vec::new();
        let position_ids: Vec<MarketId> = self.positions.keys().cloned().collect();

        for market_id in position_ids {
            let positions = match self.positions.get(&market_id) {
                Some(p) => p.clone(),
                None => continue,
            };

            for pos in &positions {
                // Only check if this snapshot is for the position's token
                if pos.token_id != snapshot.token_id {
                    continue;
                }

                // Skip if stop-loss sell already in flight for this token
                if self.pending_stop_loss.contains(&pos.token_id) {
                    continue;
                }

                if let Some(action) = self.check_stop_loss(pos, snapshot)? {
                    info!(
                        market = %market_id,
                        entry = %pos.entry_price,
                        side = ?pos.side,
                        "Stop-loss triggered, selling position"
                    );
                    // Track pending stop-loss — position removed only on sell confirmation
                    self.pending_stop_loss.insert(pos.token_id.clone());
                    actions.push(action);
                }
            }
        }

        Ok(actions)
    }

    /// Check if stop-loss should trigger for a position.
    ///
    /// Triggers when:
    /// 1. Crypto price reversed by >= stop_loss_reversal_pct (0.5%)
    /// 2. Market price dropped by >= stop_loss_min_drop (5¢) from entry
    /// 3. Time remaining > 60s (don't sell in final minute)
    fn check_stop_loss(
        &self,
        pos: &ArbitragePosition,
        snapshot: &OrderbookSnapshot,
    ) -> Result<Option<Action>> {
        let market = match self.active_markets.get(&pos.market_id) {
            Some(m) => m,
            None => return Ok(None),
        };

        let time_remaining = market.market.seconds_remaining();
        // Don't trigger stop-loss in the final 60 seconds
        if time_remaining <= 60 {
            return Ok(None);
        }

        // Check crypto price reversal
        let current_crypto = self
            .price_history
            .get(&pos.coin)
            .and_then(|h| h.back().map(|(_, p, _)| *p));

        let crypto_reversed = if let Some(current) = current_crypto {
            let reversal = match pos.side {
                OutcomeSide::Up | OutcomeSide::Yes => {
                    // We bet Up, so reversal = price went down
                    (pos.reference_price - current) / pos.reference_price
                }
                OutcomeSide::Down | OutcomeSide::No => {
                    // We bet Down, so reversal = price went up
                    (current - pos.reference_price) / pos.reference_price
                }
            };
            reversal >= self.config.stop_loss.reversal_pct
        } else {
            false
        };

        // Check market price drop from entry
        let current_bid = match snapshot.best_bid() {
            Some(bid) => bid,
            None => return Ok(None), // No bids — cannot sell, skip stop-loss
        };
        let price_drop = pos.entry_price - current_bid;
        let market_dropped = price_drop >= self.config.stop_loss.min_drop;

        if crypto_reversed && market_dropped {
            let order = OrderRequest {
                token_id: pos.token_id.clone(),
                price: current_bid,
                size: pos.size,
                side: OrderSide::Sell,
                order_type: OrderType::Fok,
                neg_risk: false,
            };
            Ok(Some(Action::PlaceOrder(order)))
        } else {
            Ok(None)
        }
    }

    async fn on_market_discovered(
        &mut self,
        market: &MarketInfo,
        ctx: &StrategyContext,
    ) -> Result<Vec<Action>> {
        // Check if this is a crypto market we care about
        let coin = match self.extract_coin(&market.question) {
            Some(c) => c,
            None => return Ok(vec![]),
        };

        if !self.config.coins.contains(&coin) {
            return Ok(vec![]);
        }

        // Get the current crypto price — needed as final fallback
        let md = ctx.market_data.read().await;
        let current_price = match md.external_prices.get(&coin) {
            Some(&p) => p,
            None => {
                info!(coin = %coin, market = %market.id, "No price yet for coin, buffering market for later activation");
                drop(md);
                self.pending_discovery.insert(coin, market.clone());
                return Ok(vec![]);
            }
        };
        drop(md);

        // Determine window start timestamp
        let window_ts = market
            .start_date
            .map(|d| d.timestamp())
            .or_else(|| parse_slug_timestamp(&market.slug))
            .unwrap_or_else(|| {
                // Fallback: align to nearest 15-min boundary
                let now = Utc::now().timestamp();
                now - (now % WINDOW_SECS)
            });

        let (reference_price, reference_quality) =
            self.find_best_reference(&coin, window_ts, current_price).await;

        let mwr = MarketWithReference {
            market: market.clone(),
            reference_price,
            reference_quality,
            discovery_time: Utc::now(),
            coin: coin.clone(),
        };

        info!(
            coin = %coin,
            market = %market.id,
            reference = %reference_price,
            quality = ?reference_quality,
            "Discovered crypto market"
        );

        self.active_markets.insert(market.id.clone(), mwr);

        Ok(vec![Action::SubscribeMarket(market.id.clone())])
    }

    async fn on_market_expired(
        &mut self,
        market_id: &str,
        _ctx: &StrategyContext,
    ) -> Result<Vec<Action>> {
        if let Some(market) = self.active_markets.remove(market_id) {
            info!(
                market = %market_id,
                coin = %market.coin,
                "Market expired, removing from active markets"
            );
        }

        let mut actions = vec![Action::UnsubscribeMarket(market_id.to_string())];

        if let Some(positions) = self.positions.remove(market_id) {
            for pos in &positions {
                warn!(
                    market = %market_id,
                    side = ?pos.side,
                    entry = %pos.entry_price,
                    "Position in expired market — awaiting resolution"
                );
            }
            actions.push(Action::Log {
                level: LogLevel::Info,
                message: format!(
                    "Market {} expired with {} open position(s)",
                    market_id,
                    positions.len()
                ),
            });
        }

        Ok(actions)
    }

    /// Handle order placement result — only record position on confirmed success.
    fn on_order_placed(&mut self, result: &OrderResult) -> Vec<Action> {
        // Check if this is a stop-loss sell confirmation
        if self.pending_stop_loss.remove(&result.token_id) {
            if result.success {
                // Sell confirmed — remove the position
                self.remove_position_by_token(&result.token_id);
                info!(
                    token_id = %result.token_id,
                    "Stop-loss sell confirmed, position removed"
                );
            } else {
                warn!(
                    token_id = %result.token_id,
                    message = %result.message,
                    "Stop-loss sell failed, position retained for retry"
                );
            }
            return vec![];
        }

        let pending = match self.pending_orders.remove(&result.token_id) {
            Some(p) => p,
            None => return vec![], // Not our order
        };

        if !result.success {
            warn!(
                token_id = %result.token_id,
                market = %pending.market_id,
                message = %result.message,
                "Order rejected, removing pending entry"
            );
            return vec![];
        }

        let position = ArbitragePosition {
            market_id: pending.market_id.clone(),
            token_id: pending.token_id,
            side: pending.side,
            entry_price: pending.price,
            size: pending.size,
            reference_price: pending.reference_price,
            coin: pending.coin,
            order_id: result.order_id.clone(),
            entry_time: Utc::now(),
        };

        info!(
            market = %pending.market_id,
            side = ?position.side,
            price = %position.entry_price,
            size = %position.size,
            "Position confirmed after order fill"
        );

        self.positions
            .entry(pending.market_id)
            .or_default()
            .push(position);

        vec![]
    }

    /// Remove a position by token_id across all markets.
    fn remove_position_by_token(&mut self, token_id: &str) {
        let mut empty_markets = Vec::new();
        for (market_id, positions) in &mut self.positions {
            positions.retain(|p| p.token_id != token_id);
            if positions.is_empty() {
                empty_markets.push(market_id.clone());
            }
        }
        for market_id in empty_markets {
            self.positions.remove(&market_id);
        }
    }

    // -- Dashboard ----------------------------------------------------------

    /// Emit a dashboard-update signal if enough time has elapsed since the last one.
    /// Returns `Some(Action)` when the throttle interval (5 seconds) has passed.
    /// Pre-renders the view HTML and includes it in the payload so the SSE handler
    /// doesn't need to re-acquire the strategy lock (which would deadlock).
    fn maybe_emit_dashboard_update(&mut self) -> Option<Action> {
        let now = tokio::time::Instant::now();
        let should_emit = match self.last_dashboard_emit {
            Some(last) => now.duration_since(last) >= std::time::Duration::from_secs(5),
            None => true,
        };
        if should_emit {
            self.last_dashboard_emit = Some(now);
            let html = self.render_view().unwrap_or_default();
            Some(Action::EmitSignal {
                signal_type: "dashboard-update".to_string(),
                payload: serde_json::json!({
                    "view_name": self.view_name(),
                    "rendered_html": html,
                }),
            })
        } else {
            None
        }
    }

    // -- Reference price helpers -----------------------------------------------

    /// Find the most accurate reference price for a coin at a given window start.
    ///
    /// Priority:
    /// 0. On-chain Chainlink RPC lookup (exact settlement price)
    /// 1. Exact boundary snapshot (captured within 2s of window start)
    /// 2. Closest historical price entry (within 30s of window start)
    /// 3. Current price (fallback)
    async fn find_best_reference(
        &self,
        coin: &str,
        window_ts: i64,
        current_price: Decimal,
    ) -> (Decimal, ReferenceQuality) {
        // 0. On-chain Chainlink RPC — the exact price Polymarket uses for settlement
        if let Some(client) = &self.chainlink_client {
            match client
                .get_price_at_timestamp(coin, window_ts as u64, 100)
                .await
            {
                Ok(cp) => {
                    let staleness = cp.timestamp.abs_diff(window_ts as u64);
                    info!(
                        coin = %coin,
                        price = %cp.price,
                        staleness_s = staleness,
                        round_id = cp.round_id,
                        "On-chain Chainlink reference price retrieved"
                    );
                    return (cp.price, ReferenceQuality::OnChain);
                }
                Err(e) => {
                    warn!(
                        coin = %coin,
                        error = %e,
                        "On-chain Chainlink lookup failed, falling back to local data"
                    );
                }
            }
        }

        // 1. Exact boundary snapshot
        let key = format!("{coin}-{window_ts}");
        if let Some(snap) = self.boundary_prices.get(&key) {
            return (snap.price, ReferenceQuality::Exact);
        }

        // 2. Historical lookup — closest entry to window start, preferring Chainlink
        let target = DateTime::from_timestamp(window_ts, 0);
        if let (Some(target_dt), Some(history)) = (target, self.price_history.get(coin)) {
            // Find all entries within 30s of window start
            let mut best: Option<(u64, Decimal, bool)> = None; // (staleness, price, is_chainlink)
            for (ts, price, source) in history {
                let staleness = (*ts - target_dt).num_seconds().unsigned_abs();
                if staleness >= 30 {
                    continue;
                }
                let is_chainlink = source.eq_ignore_ascii_case("chainlink");
                let is_better = match best {
                    None => true,
                    Some((prev_stale, _, prev_cl)) => {
                        // Prefer Chainlink if staleness is similar (within 5s)
                        if is_chainlink && !prev_cl && staleness < prev_stale + 5 {
                            true
                        } else if !is_chainlink && prev_cl && prev_stale < staleness + 5 {
                            false
                        } else {
                            staleness < prev_stale
                        }
                    }
                };
                if is_better {
                    best = Some((staleness, *price, is_chainlink));
                }
            }
            if let Some((staleness, price, _)) = best {
                return (price, ReferenceQuality::Historical(staleness));
            }
        }

        // 3. Current price (existing behavior)
        (current_price, ReferenceQuality::Current)
    }

    /// Remove boundary snapshots older than 4 windows (1 hour) for a given coin.
    fn prune_boundary_snapshots(&mut self, coin: &str) {
        let now_ts = Utc::now().timestamp();
        let cutoff = now_ts - (WINDOW_SECS * 4);
        let prefix = format!("{coin}-");
        self.boundary_prices.retain(|key, _| {
            if !key.starts_with(&prefix) {
                return true;
            }
            // Extract timestamp from key
            key.strip_prefix(&prefix)
                .and_then(|ts_str| ts_str.parse::<i64>().ok())
                .is_none_or(|ts| ts >= cutoff)
        });
    }

    // -- Helpers ------------------------------------------------------------

    /// Extract coin symbol from market question string.
    /// Looks for known coin names as whole words in the question text.
    fn extract_coin(&self, question: &str) -> Option<String> {
        const COIN_NAMES: &[(&str, &str)] = &[
            ("BITCOIN", "BTC"),
            ("ETHEREUM", "ETH"),
            ("SOLANA", "SOL"),
        ];

        let upper = question.to_uppercase();

        // First, check for full coin names (e.g. "Bitcoin" → "BTC")
        for &(name, ticker) in COIN_NAMES {
            if upper.contains(name) {
                return Some(ticker.to_string());
            }
        }

        // Then, check for ticker symbols as whole words (e.g. "XRP")
        for coin in &self.config.coins {
            // Match coin as a whole word to avoid false positives
            // (e.g. "SOL" should not match "SOLVE" or "resolution")
            let mut found = false;
            for (idx, _) in upper.match_indices(coin.as_str()) {
                let before_ok = idx == 0
                    || !upper[..idx]
                        .chars()
                        .next_back()
                        .unwrap()
                        .is_ascii_alphanumeric();
                let after_idx = idx + coin.len();
                let after_ok = after_idx >= upper.len()
                    || !upper[after_idx..]
                        .chars()
                        .next()
                        .unwrap()
                        .is_ascii_alphanumeric();
                if before_ok && after_ok {
                    found = true;
                    break;
                }
            }
            if found {
                return Some(coin.clone());
            }
        }
        None
    }
}

impl DashboardViewProvider for CryptoArbitrageStrategy {
    fn view_name(&self) -> &str {
        "crypto-arb"
    }

    fn render_view(&self) -> polyrust_core::error::Result<String> {
        let mut html = String::with_capacity(4096);

        // --- Reference Prices & Predictions ---
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        html.push_str(r#"<h2 class="text-lg font-bold mb-3">Reference Prices &amp; Predictions</h2>"#);

        if self.active_markets.is_empty() {
            html.push_str(r#"<p class="text-gray-500">No active markets</p>"#);
        } else {
            html.push_str(r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#);
            html.push_str("<th class=\"text-left py-1\">Coin</th>");
            html.push_str("<th class=\"text-right py-1\">Ref Price</th>");
            html.push_str("<th class=\"text-right py-1\">Current</th>");
            html.push_str("<th class=\"text-right py-1\">Change</th>");
            html.push_str("<th class=\"text-right py-1\">Prediction</th>");
            html.push_str("</tr></thead><tbody>");

            // Collect unique coins from active markets
            let mut seen_coins = HashSet::new();
            let mut markets_sorted: Vec<_> = self.active_markets.values().collect();
            markets_sorted.sort_by(|a, b| a.coin.cmp(&b.coin));

            for mwr in &markets_sorted {
                if !seen_coins.insert(&mwr.coin) {
                    continue;
                }
                let current_price = self
                    .price_history
                    .get(&mwr.coin)
                    .and_then(|h| h.back().map(|(_, p, _)| *p));

                let ref_label = match mwr.reference_quality {
                    ReferenceQuality::OnChain => "✓",
                    ReferenceQuality::Exact => "=",
                    ReferenceQuality::Historical(_) => "≈",
                    ReferenceQuality::Current => "~",
                };

                let (change_str, change_class, prediction) = match current_price {
                    Some(cp) => {
                        let change = if mwr.reference_price.is_zero() {
                            Decimal::ZERO
                        } else {
                            ((cp - mwr.reference_price) / mwr.reference_price)
                                * Decimal::new(100, 0)
                        };
                        let cls = if change >= Decimal::ZERO {
                            "pnl-positive"
                        } else {
                            "pnl-negative"
                        };
                        let pred = match mwr.predict_winner(cp) {
                            Some(OutcomeSide::Up) | Some(OutcomeSide::Yes) => "UP",
                            Some(OutcomeSide::Down) | Some(OutcomeSide::No) => "DOWN",
                            None => "-",
                        };
                        (format!("{:+.2}%", change), cls, pred)
                    }
                    None => ("-".to_string(), "", "-"),
                };

                let _ = write!(
                    html,
                    r#"<tr class="border-b border-gray-800"><td class="py-1">{coin}</td><td class="text-right py-1">{ref_label}{ref_price}</td><td class="text-right py-1">{current}</td><td class="text-right py-1 {change_class}">{change}</td><td class="text-right py-1 font-bold">{prediction}</td></tr>"#,
                    coin = escape_html(&mwr.coin),
                    ref_label = ref_label,
                    ref_price = fmt_usd(mwr.reference_price),
                    current = current_price
                        .map(fmt_usd)
                        .unwrap_or_else(|| "-".to_string()),
                    change_class = change_class,
                    change = change_str,
                    prediction = prediction,
                );
            }
            html.push_str("</tbody></table>");
        }
        html.push_str("</div>");

        // --- Active Markets ---
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        let _ = write!(
            html,
            r#"<h2 class="text-lg font-bold mb-3">Active Markets ({})</h2>"#,
            self.active_markets.len()
        );

        if self.active_markets.is_empty() {
            html.push_str(r#"<p class="text-gray-500">No active markets</p>"#);
        } else {
            html.push_str(r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#);
            html.push_str("<th class=\"text-left py-1\">Market</th>");
            html.push_str("<th class=\"text-right py-1\">UP</th>");
            html.push_str("<th class=\"text-right py-1\">DOWN</th>");
            html.push_str("<th class=\"text-right py-1\">Fee</th>");
            html.push_str("<th class=\"text-right py-1\">Net</th>");
            html.push_str("<th class=\"text-right py-1\">Time Left</th>");
            html.push_str("</tr></thead><tbody>");

            let mut markets_by_time: Vec<_> = self.active_markets.values().collect();
            markets_by_time.sort_by_key(|m| m.market.end_date);

            for mwr in &markets_by_time {
                let remaining = mwr.market.seconds_remaining().max(0);
                let time_str = if remaining > 60 {
                    format!("{}m {}s", remaining / 60, remaining % 60)
                } else {
                    format!("{}s", remaining)
                };

                let up_ask = self
                    .cached_asks
                    .get(&mwr.market.token_ids.outcome_a)
                    .copied();
                let down_ask = self
                    .cached_asks
                    .get(&mwr.market.token_ids.outcome_b)
                    .copied();

                let up_price = up_ask
                    .map(fmt_market_price)
                    .unwrap_or_else(|| "-".to_string());
                let down_price = down_ask
                    .map(fmt_market_price)
                    .unwrap_or_else(|| "-".to_string());

                // Show fee/net for the predicted winner side (or lower-priced side)
                let fee_rate = self.config.fee.taker_fee_rate;
                let (fee_str, net_str) = match (up_ask, down_ask) {
                    (Some(ua), Some(da)) => {
                        // Show fee for the lower-priced (more likely to trade) side
                        let price = ua.min(da);
                        let fee = taker_fee(price, fee_rate);
                        let net = net_profit_margin(price, fee_rate, false);
                        (
                            format!("{:.3}", fee.round_dp(3)),
                            format!("{:.3}", net.round_dp(3)),
                        )
                    }
                    (Some(p), None) | (None, Some(p)) => {
                        let fee = taker_fee(p, fee_rate);
                        let net = net_profit_margin(p, fee_rate, false);
                        (
                            format!("{:.3}", fee.round_dp(3)),
                            format!("{:.3}", net.round_dp(3)),
                        )
                    }
                    _ => ("-".to_string(), "-".to_string()),
                };

                let _ = write!(
                    html,
                    r#"<tr class="border-b border-gray-800"><td class="py-1">{coin} Up/Down</td><td class="text-right py-1">{up}</td><td class="text-right py-1">{down}</td><td class="text-right py-1">{fee}</td><td class="text-right py-1">{net}</td><td class="text-right py-1">{time}</td></tr>"#,
                    coin = escape_html(&mwr.coin),
                    up = up_price,
                    down = down_price,
                    fee = fee_str,
                    net = net_str,
                    time = time_str,
                );
            }
            html.push_str("</tbody></table>");
        }
        html.push_str("</div>");

        // --- Open Positions ---
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        let total_positions: usize = self.positions.values().map(|v| v.len()).sum();
        let _ = write!(
            html,
            r#"<h2 class="text-lg font-bold mb-3">Open Positions ({})</h2>"#,
            total_positions
        );

        if self.positions.is_empty() {
            html.push_str(r#"<p class="text-gray-500">No open positions</p>"#);
        } else {
            html.push_str(r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#);
            html.push_str("<th class=\"text-left py-1\">Market</th>");
            html.push_str("<th class=\"text-left py-1\">Side</th>");
            html.push_str("<th class=\"text-right py-1\">Entry</th>");
            html.push_str("<th class=\"text-right py-1\">Current</th>");
            html.push_str("<th class=\"text-right py-1\">PnL</th>");
            html.push_str("<th class=\"text-right py-1\">Size</th>");
            html.push_str("</tr></thead><tbody>");

            for positions in self.positions.values() {
                for pos in positions {
                    let current = self.cached_asks.get(&pos.token_id).copied();
                    let (current_str, pnl_str, pnl_class) = match current {
                        Some(cp) => {
                            let pnl = (cp - pos.entry_price) * pos.size;
                            let cls = if pnl >= Decimal::ZERO {
                                "pnl-positive"
                            } else {
                                "pnl-negative"
                            };
                            (cp.to_string(), format!("${pnl:.2}"), cls)
                        }
                        None => ("-".to_string(), "-".to_string(), ""),
                    };
                    let _ = write!(
                        html,
                        r#"<tr class="border-b border-gray-800"><td class="py-1">{coin}</td><td class="py-1">{side:?}</td><td class="text-right py-1">{entry}</td><td class="text-right py-1">{current}</td><td class="text-right py-1"><span class="{pnl_class}">{pnl}</span></td><td class="text-right py-1">{size}</td></tr>"#,
                        coin = escape_html(&pos.coin),
                        side = pos.side,
                        entry = pos.entry_price,
                        current = current_str,
                        pnl_class = pnl_class,
                        pnl = pnl_str,
                        size = pos.size,
                    );
                }
            }
            html.push_str("</tbody></table>");
        }
        html.push_str("</div>");

        Ok(html)
    }
}

#[async_trait]
impl Strategy for CryptoArbitrageStrategy {
    fn name(&self) -> &str {
        "crypto-arbitrage"
    }

    fn description(&self) -> &str {
        "Exploits mispricing in 15-min Up/Down crypto markets"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        info!(
            coins = ?self.config.coins,
            max_positions = self.config.max_positions,
            position_size = %self.config.sizing.base_size,
            "Crypto arbitrage strategy started"
        );
        self.last_scan = Some(tokio::time::Instant::now());
        Ok(())
    }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        let mut actions = match event {
            Event::MarketData(MarketDataEvent::ExternalPrice {
                symbol,
                price,
                source,
                ..
            }) => self.on_crypto_price(symbol, *price, source, ctx).await?,

            Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot)) => {
                self.on_orderbook_update(snapshot, ctx).await?
            }

            Event::MarketData(MarketDataEvent::MarketDiscovered(market)) => {
                self.on_market_discovered(market, ctx).await?
            }

            Event::MarketData(MarketDataEvent::MarketExpired(id)) => {
                self.on_market_expired(id, ctx).await?
            }

            Event::OrderUpdate(OrderEvent::Placed(result)) => self.on_order_placed(result),

            Event::OrderUpdate(OrderEvent::Rejected { token_id, .. }) => {
                if let Some(token_id) = token_id {
                    // Clear pending buy order
                    if let Some(pending) = self.pending_orders.remove(token_id) {
                        warn!(
                            token_id = %token_id,
                            market = %pending.market_id,
                            "Cleared pending order after rejection"
                        );
                    }
                    // Clear pending stop-loss — position retained for retry
                    if self.pending_stop_loss.remove(token_id) {
                        warn!(
                            token_id = %token_id,
                            "Stop-loss sell rejected, position retained for retry"
                        );
                    }
                }
                vec![]
            }

            _ => vec![],
        };

        // Throttled dashboard update signal for real-time SSE view refresh
        if let Some(dashboard_action) = self.maybe_emit_dashboard_update() {
            actions.push(dashboard_action);
        }

        Ok(actions)
    }

    async fn on_stop(&mut self, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        let total_positions: usize = self.positions.values().map(|v| v.len()).sum();
        info!(
            active_markets = self.active_markets.len(),
            open_positions = total_positions,
            pending_orders = self.pending_orders.len(),
            "Crypto arbitrage strategy stopping"
        );

        let mut actions = Vec::new();

        // Cancel all open orders on shutdown to avoid orphaned orders
        if !self.positions.is_empty() || !self.pending_orders.is_empty() {
            warn!(
                markets_with_positions = self.positions.len(),
                total_positions = total_positions,
                "Cancelling all open orders on shutdown"
            );
            actions.push(Action::CancelAllOrders);
        }

        self.pending_orders.clear();
        self.pending_stop_loss.clear();
        Ok(actions)
    }

    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use rust_decimal_macros::dec;
    use std::collections::VecDeque;

    fn make_market_info(id: &str, end_date: DateTime<Utc>) -> MarketInfo {
        MarketInfo {
            id: id.to_string(),
            slug: "btc-up-down".to_string(),
            question: "Will BTC go up?".to_string(),
            start_date: None,
            end_date,
            token_ids: TokenIds {
                outcome_a: "token_up".to_string(),
                outcome_b: "token_down".to_string(),
            },
            accepting_orders: true,
            neg_risk: false,
        }
    }

    fn make_mwr(reference_price: Decimal, time_remaining_secs: i64) -> MarketWithReference {
        MarketWithReference {
            market: make_market_info(
                "market1",
                Utc::now() + Duration::seconds(time_remaining_secs),
            ),
            reference_price,
            reference_quality: ReferenceQuality::Exact,
            discovery_time: Utc::now(),
            coin: "BTC".to_string(),
        }
    }

    fn make_orderbook(token_id: &str, best_bid: Decimal, best_ask: Decimal) -> OrderbookSnapshot {
        OrderbookSnapshot {
            token_id: token_id.to_string(),
            bids: vec![OrderbookLevel {
                price: best_bid,
                size: dec!(100),
            }],
            asks: vec![OrderbookLevel {
                price: best_ask,
                size: dec!(100),
            }],
            timestamp: Utc::now(),
        }
    }

    // --- predict_winner tests ---

    #[test]
    fn predict_winner_btc_up() {
        let mwr = make_mwr(dec!(50000), 600);
        // Current price above reference => Up
        assert_eq!(mwr.predict_winner(dec!(50100)), Some(OutcomeSide::Up));
    }

    #[test]
    fn predict_winner_btc_down() {
        let mwr = make_mwr(dec!(50000), 600);
        // Current price below reference => Down
        assert_eq!(mwr.predict_winner(dec!(49900)), Some(OutcomeSide::Down));
    }

    #[test]
    fn predict_winner_at_reference_returns_none() {
        let mwr = make_mwr(dec!(50000), 600);
        // Price equals reference => no directional signal
        assert_eq!(mwr.predict_winner(dec!(50000)), None);
    }

    // --- get_confidence tests ---

    #[test]
    fn confidence_tail_end() {
        // < 120s remaining, market >= 0.90 -> confidence 1.0
        let mwr = make_mwr(dec!(50000), 60);
        let confidence = mwr.get_confidence(dec!(51000), dec!(0.95), 60);
        assert_eq!(confidence, dec!(1.0));
    }

    #[test]
    fn confidence_tail_end_low_market_price() {
        // < 120s but market < 0.90 -> NOT tail-end, falls to late window
        // Small move so late window doesn't cap at 1.0
        let mwr = make_mwr(dec!(50000), 60);
        // distance_pct = 50/50000 = 0.001, base = 0.001 * 66 = 0.066
        // market_boost = 1.0 + (0.55 - 0.50) * 0.5 = 1.025
        // raw = 0.066 * 1.025 = 0.0677 < 1.0
        let confidence = mwr.get_confidence(dec!(50050), dec!(0.55), 60);
        assert!(confidence < dec!(1.0));
        assert!(confidence > Decimal::ZERO);
    }

    #[test]
    fn confidence_late_window() {
        // 120-300s remaining
        let mwr = make_mwr(dec!(50000), 200);
        let confidence = mwr.get_confidence(dec!(51000), dec!(0.70), 200);
        // distance_pct = 1000/50000 = 0.02
        // base = 0.02 * 66 = 1.32
        // market_boost = 1.0 + (0.70 - 0.50) * 0.5 = 1.10
        // raw = 1.32 * 1.10 = 1.452 -> capped at 1.0
        assert!(confidence > Decimal::ZERO);
        assert!(confidence <= dec!(1.0));
    }

    #[test]
    fn confidence_early_window() {
        // > 300s remaining
        let mwr = make_mwr(dec!(50000), 600);
        // distance_pct = 500/50000 = 0.01
        // raw = 0.01 * 50 = 0.50
        let confidence = mwr.get_confidence(dec!(50500), dec!(0.50), 600);
        assert_eq!(confidence, dec!(0.50));
    }

    #[test]
    fn confidence_early_window_small_move() {
        // > 300s, small move => lower confidence
        let mwr = make_mwr(dec!(50000), 600);
        // distance_pct = 100/50000 = 0.002
        // raw = 0.002 * 50 = 0.10
        let confidence = mwr.get_confidence(dec!(50100), dec!(0.50), 600);
        assert_eq!(confidence, dec!(0.10));
    }

    // --- evaluate_opportunity tests ---

    #[tokio::test]
    async fn evaluate_tail_end_opportunity() {
        let mwr = make_mwr(dec!(50000), 60);
        let ctx = StrategyContext::new();

        // Set up orderbook with high ask for Up outcome
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.93), dec!(0.95)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.03), dec!(0.05)),
            );
        }

        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        // Current price > reference => Up wins; ask = 0.95 >= 0.90
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(51000), &ctx)
            .await
            .unwrap();
        assert!(!opps.is_empty());
        let opp = &opps[0];
        assert_eq!(opp.mode, ArbitrageMode::TailEnd);
        assert_eq!(opp.outcome_to_buy, OutcomeSide::Up);
        assert_eq!(opp.buy_price, dec!(0.95));
        assert_eq!(opp.confidence, dec!(1.0));
    }

    #[tokio::test]
    async fn evaluate_two_sided_opportunity() {
        let mwr = make_mwr(dec!(50000), 400);
        let ctx = StrategyContext::new();

        // Both asks low: 0.40 + 0.40 = 0.80 < 0.98
        // Gross margin = 0.20, fees = ~0.03, net > 0 → opportunity generated
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.38), dec!(0.40)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.38), dec!(0.40)),
            );
        }

        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(50100), &ctx)
            .await
            .unwrap();
        assert_eq!(opps.len(), 2, "TwoSided should return both outcomes");
        assert_eq!(opps[0].mode, ArbitrageMode::TwoSided);
        assert_eq!(opps[0].outcome_to_buy, OutcomeSide::Up);
        assert_eq!(opps[1].outcome_to_buy, OutcomeSide::Down);
        assert_eq!(opps[0].profit_margin, dec!(0.20)); // 1.0 - 0.80
        assert!(opps[0].net_margin > Decimal::ZERO);
        assert!(opps[0].estimated_fee > Decimal::ZERO);
    }

    #[tokio::test]
    async fn evaluate_confirmed_opportunity() {
        let mwr = make_mwr(dec!(50000), 200);
        let ctx = StrategyContext::new();

        // Late window, reasonable ask, high distance
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.55), dec!(0.60)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.35), dec!(0.40)),
            );
        }

        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        // Large price move: 52000 vs 50000 = 4% distance
        // confidence = min(1.0, 0.04 * 66 * boost) will be > 0.50
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(52000), &ctx)
            .await
            .unwrap();
        assert!(!opps.is_empty());
        let opp = &opps[0];
        assert_eq!(opp.mode, ArbitrageMode::Confirmed);
        assert_eq!(opp.outcome_to_buy, OutcomeSide::Up);
        assert!(opp.confidence >= dec!(0.50));
        assert!(opp.estimated_fee > Decimal::ZERO);
        assert!(opp.net_margin > Decimal::ZERO);
        assert!(opp.net_margin < opp.profit_margin);
    }

    #[tokio::test]
    async fn evaluate_no_opportunity_low_confidence() {
        let mwr = make_mwr(dec!(50000), 600);
        let ctx = StrategyContext::new();

        // Early window, tiny move, high ask => no opportunity
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.88), dec!(0.92)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.06), dec!(0.08)),
            );
        }

        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        // Tiny move: 50010 vs 50000 = 0.02% distance
        // confidence = 0.0002 * 50 = 0.01 < 0.50
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(50010), &ctx)
            .await
            .unwrap();
        assert!(opps.is_empty());
    }

    // --- stop-loss tests ---

    #[test]
    fn stop_loss_triggers() {
        // Reversal > 0.5% AND price drop > 5¢ AND time > 60s
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());

        let mwr = make_mwr(dec!(50000), 300);
        strategy.active_markets.insert("market1".to_string(), mwr);

        // We bet Up at reference 50000 with entry price 0.60
        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
        };

        // Price reversed: BTC dropped from 50000 to 49500 = -1% > 0.5%
        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(49500), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // Market bid dropped from 0.60 to 0.50 = 10¢ > 5¢
        let snapshot = make_orderbook("token_up", dec!(0.50), dec!(0.55));

        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_some());
    }

    #[test]
    fn stop_loss_does_not_trigger_final_60s() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());

        // Only 30 seconds left
        let mwr = make_mwr(dec!(50000), 30);
        strategy.active_markets.insert("market1".to_string(), mwr);

        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
        };

        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(49500), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        let snapshot = make_orderbook("token_up", dec!(0.50), dec!(0.55));
        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_none());
    }

    #[test]
    fn stop_loss_does_not_trigger_small_drop() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());

        let mwr = make_mwr(dec!(50000), 300);
        strategy.active_markets.insert("market1".to_string(), mwr);

        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
        };

        // Crypto reversed, but market price only dropped 3¢ < 5¢
        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(49500), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        let snapshot = make_orderbook("token_up", dec!(0.57), dec!(0.60));
        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_none());
    }

    // --- market discovery/expiry tests ---

    #[tokio::test]
    async fn on_market_discovered_creates_entry() {
        let mut strategy = make_strategy_no_chainlink();
        let ctx = StrategyContext::new();

        // Set BTC price in context
        {
            let mut md = ctx.market_data.write().await;
            md.external_prices.insert("BTC".to_string(), dec!(50000));
        }

        let market = make_market_info("btc-market-1", Utc::now() + Duration::seconds(900));

        let actions = strategy.on_market_discovered(&market, &ctx).await.unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::SubscribeMarket(_)));
        assert!(strategy.active_markets.contains_key("btc-market-1"));
        // Reference price should be the current external price (Current quality)
        let mwr = &strategy.active_markets["btc-market-1"];
        assert_eq!(mwr.reference_price, dec!(50000));
        assert_eq!(mwr.reference_quality, ReferenceQuality::Current);
    }

    #[tokio::test]
    async fn on_market_expired_removes_market() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        let ctx = StrategyContext::new();

        let mwr = make_mwr(dec!(50000), 0);
        strategy.active_markets.insert("market1".to_string(), mwr);

        let actions = strategy.on_market_expired("market1", &ctx).await.unwrap();
        assert!(!strategy.active_markets.contains_key("market1"));
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::UnsubscribeMarket(_)));
    }

    // --- extract_coin tests ---

    #[test]
    fn extract_coin_from_question() {
        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        assert_eq!(
            strategy.extract_coin("Will BTC go up in the next 15 minutes?"),
            Some("BTC".to_string())
        );
        assert_eq!(
            strategy.extract_coin("Will ETH be above $2000?"),
            Some("ETH".to_string())
        );
        assert_eq!(strategy.extract_coin("Random question about stocks"), None);
        // Full coin names (as used by Polymarket)
        assert_eq!(
            strategy.extract_coin("Bitcoin Up or Down - January 27, 4:45PM-5:00PM ET"),
            Some("BTC".to_string())
        );
        assert_eq!(
            strategy.extract_coin("Ethereum Up or Down - January 27, 4:45PM-5:00PM ET"),
            Some("ETH".to_string())
        );
        assert_eq!(
            strategy.extract_coin("Solana Up or Down - January 27, 4:45PM-5:00PM ET"),
            Some("SOL".to_string())
        );
        assert_eq!(
            strategy.extract_coin("XRP Up or Down - January 27, 4:45PM-5:00PM ET"),
            Some("XRP".to_string())
        );
    }

    // --- DashboardViewProvider tests ---

    #[test]
    fn dashboard_view_returns_some() {
        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        let view = strategy.dashboard_view();
        assert!(view.is_some());
        assert_eq!(view.unwrap().view_name(), "crypto-arb");
    }

    #[test]
    fn render_view_empty_state() {
        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        let html = strategy.render_view().unwrap();
        // Should contain all three section headers
        assert!(html.contains("Reference Prices"));
        assert!(html.contains("Active Markets"));
        assert!(html.contains("Open Positions"));
        // Empty state messages
        assert!(html.contains("No active markets"));
        assert!(html.contains("No open positions"));
    }

    #[test]
    fn render_view_with_active_markets_and_prices() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());

        // Add an active market
        let mwr = make_mwr(dec!(50000), 300);
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

        // Add current price history for BTC
        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(50500), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        let html = strategy.render_view().unwrap();

        // Reference price section should show coin data with formatted prices
        assert!(html.contains("BTC"));
        assert!(html.contains("$50,000.00"));
        assert!(html.contains("$50,500.00"));
        assert!(html.contains("UP")); // 50500 > 50000 => UP prediction

        // Active markets section should show the market
        assert!(html.contains("BTC Up/Down"));

        // No open positions
        assert!(html.contains("No open positions"));
    }

    #[test]
    fn render_view_with_positions() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());

        // Add a position
        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
        };
        strategy
            .positions
            .entry("market1".to_string())
            .or_default()
            .push(pos);

        let html = strategy.render_view().unwrap();

        // Should show position data
        assert!(html.contains("Open Positions (1)"));
        assert!(html.contains("BTC"));
        assert!(html.contains("0.60")); // entry price
        assert!(!html.contains("No open positions"));
    }

    // --- dashboard update emission tests ---

    #[test]
    fn maybe_emit_dashboard_update_first_call_emits() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        let action = strategy.maybe_emit_dashboard_update();
        assert!(action.is_some(), "first call should emit");
        if let Some(Action::EmitSignal {
            signal_type,
            payload,
        }) = action
        {
            assert_eq!(signal_type, "dashboard-update");
            assert_eq!(payload["view_name"], "crypto-arb");
        }
    }

    #[test]
    fn maybe_emit_dashboard_update_throttles() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());

        // First call emits
        let action = strategy.maybe_emit_dashboard_update();
        assert!(action.is_some());

        // Immediate second call should be throttled
        let action = strategy.maybe_emit_dashboard_update();
        assert!(action.is_none(), "immediate second call should be throttled");
    }

    #[test]
    fn render_view_current_quality_reference() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());

        let mut mwr = make_mwr(dec!(50000), 300);
        mwr.reference_quality = ReferenceQuality::Current;
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

        let html = strategy.render_view().unwrap();
        // Current quality reference should show ~ prefix with formatted price
        assert!(html.contains("~$50,000.00"));
    }

    #[test]
    fn render_view_historical_quality_reference() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());

        let mut mwr = make_mwr(dec!(50000), 300);
        mwr.reference_quality = ReferenceQuality::Historical(10);
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

        let html = strategy.render_view().unwrap();
        // Historical quality reference should show ≈ prefix
        assert!(html.contains("≈$50,000.00"));
    }

    #[test]
    fn render_view_onchain_quality_reference() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());

        let mut mwr = make_mwr(dec!(50000), 300);
        mwr.reference_quality = ReferenceQuality::OnChain;
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

        let html = strategy.render_view().unwrap();
        // OnChain quality reference should show ✓ prefix
        assert!(html.contains("✓$50,000.00"));
    }

    // --- reference quality tests ---

    #[test]
    fn quality_factor_values() {
        assert_eq!(ReferenceQuality::OnChain.quality_factor(), Decimal::ONE);
        assert_eq!(ReferenceQuality::Exact.quality_factor(), Decimal::ONE);
        assert_eq!(
            ReferenceQuality::Historical(3).quality_factor(),
            dec!(0.95)
        );
        assert_eq!(
            ReferenceQuality::Historical(10).quality_factor(),
            dec!(0.85)
        );
        assert_eq!(ReferenceQuality::Current.quality_factor(), dec!(0.70));
    }

    #[test]
    fn confidence_discounted_by_quality() {
        // Exact quality: raw confidence unchanged
        let mwr_exact = make_mwr(dec!(50000), 600);
        // distance_pct = 500/50000 = 0.01, raw = 0.01 * 50 = 0.50
        let c_exact = mwr_exact.get_confidence(dec!(50500), dec!(0.50), 600);
        assert_eq!(c_exact, dec!(0.50)); // 0.50 * 1.0 = 0.50

        // Current quality: discounted by 0.70
        let mut mwr_current = make_mwr(dec!(50000), 600);
        mwr_current.reference_quality = ReferenceQuality::Current;
        let c_current = mwr_current.get_confidence(dec!(50500), dec!(0.50), 600);
        assert_eq!(c_current, dec!(0.350)); // 0.50 * 0.70 = 0.35
    }

    /// Helper: create a strategy with Chainlink disabled (no RPC calls in tests).
    fn make_strategy_no_chainlink() -> CryptoArbitrageStrategy {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        CryptoArbitrageStrategy::new(config)
    }

    #[tokio::test]
    async fn find_best_reference_exact_boundary() {
        let mut strategy = make_strategy_no_chainlink();

        let ts = 1706000000i64;
        strategy.boundary_prices.insert(
            "BTC-1706000000".to_string(),
            BoundarySnapshot {
                timestamp: DateTime::from_timestamp(ts, 0).unwrap(),
                price: dec!(42500),
                source: "chainlink".to_string(),
            },
        );

        let (price, quality) = strategy.find_best_reference("BTC", ts, dec!(43000)).await;
        assert_eq!(price, dec!(42500));
        assert_eq!(quality, ReferenceQuality::Exact);
    }

    #[tokio::test]
    async fn find_best_reference_historical() {
        let mut strategy = make_strategy_no_chainlink();

        let window_ts = 1706000000i64;
        let target_dt = DateTime::from_timestamp(window_ts, 0).unwrap();

        // Add history entries around the window start
        let mut history = VecDeque::new();
        // 5 seconds after window start
        history.push_back((
            target_dt + Duration::seconds(5),
            dec!(42600),
            "binance".to_string(),
        ));
        // 20 seconds after window start
        history.push_back((
            target_dt + Duration::seconds(20),
            dec!(42700),
            "binance".to_string(),
        ));
        strategy.price_history.insert("BTC".to_string(), history);

        let (price, quality) = strategy.find_best_reference("BTC", window_ts, dec!(43000)).await;
        assert_eq!(price, dec!(42600)); // Closest to window start (5s)
        assert_eq!(quality, ReferenceQuality::Historical(5));
    }

    #[tokio::test]
    async fn find_best_reference_historical_prefers_chainlink() {
        let mut strategy = make_strategy_no_chainlink();

        let window_ts = 1706000000i64;
        let target_dt = DateTime::from_timestamp(window_ts, 0).unwrap();

        let mut history = VecDeque::new();
        // Binance at 3s, Chainlink at 6s — within 5s of each other
        history.push_back((
            target_dt + Duration::seconds(3),
            dec!(42600),
            "binance".to_string(),
        ));
        history.push_back((
            target_dt + Duration::seconds(6),
            dec!(42650),
            "chainlink".to_string(),
        ));
        strategy.price_history.insert("BTC".to_string(), history);

        let (price, quality) = strategy.find_best_reference("BTC", window_ts, dec!(43000)).await;
        // Should prefer Chainlink even though it's slightly further
        assert_eq!(price, dec!(42650));
        assert_eq!(quality, ReferenceQuality::Historical(6));
    }

    #[tokio::test]
    async fn find_best_reference_fallback_to_current() {
        let strategy = make_strategy_no_chainlink();

        // No boundary snapshots, no history
        let (price, quality) = strategy.find_best_reference("BTC", 1706000000, dec!(43000)).await;
        assert_eq!(price, dec!(43000));
        assert_eq!(quality, ReferenceQuality::Current);
    }

    #[tokio::test]
    async fn find_best_reference_stale_history_falls_to_current() {
        let mut strategy = make_strategy_no_chainlink();

        let window_ts = 1706000000i64;
        let target_dt = DateTime::from_timestamp(window_ts, 0).unwrap();

        // History entry 60s after window start — too stale (> 30s threshold)
        let mut history = VecDeque::new();
        history.push_back((
            target_dt + Duration::seconds(60),
            dec!(42800),
            "binance".to_string(),
        ));
        strategy.price_history.insert("BTC".to_string(), history);

        let (price, quality) = strategy.find_best_reference("BTC", window_ts, dec!(43000)).await;
        assert_eq!(price, dec!(43000));
        assert_eq!(quality, ReferenceQuality::Current);
    }

    #[test]
    fn parse_slug_timestamp_valid() {
        assert_eq!(
            parse_slug_timestamp("btc-updown-15m-1706000000"),
            Some(1706000000)
        );
    }

    #[test]
    fn parse_slug_timestamp_no_number() {
        assert_eq!(parse_slug_timestamp("btc-updown-15m"), None);
    }

    #[test]
    fn parse_slug_timestamp_small_number() {
        assert_eq!(parse_slug_timestamp("btc-updown-15m-12345"), None);
    }

    #[test]
    fn prune_boundary_snapshots_removes_old() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());

        let now_ts = Utc::now().timestamp();
        let old_ts = now_ts - (WINDOW_SECS * 5); // 5 windows ago
        let recent_ts = now_ts - WINDOW_SECS; // 1 window ago

        strategy.boundary_prices.insert(
            format!("BTC-{old_ts}"),
            BoundarySnapshot {
                timestamp: DateTime::from_timestamp(old_ts, 0).unwrap(),
                price: dec!(40000),
                source: "chainlink".to_string(),
            },
        );
        strategy.boundary_prices.insert(
            format!("BTC-{recent_ts}"),
            BoundarySnapshot {
                timestamp: DateTime::from_timestamp(recent_ts, 0).unwrap(),
                price: dec!(42000),
                source: "chainlink".to_string(),
            },
        );

        strategy.prune_boundary_snapshots("BTC");

        // Old one should be pruned, recent one kept
        assert!(!strategy.boundary_prices.contains_key(&format!("BTC-{old_ts}")));
        assert!(strategy.boundary_prices.contains_key(&format!("BTC-{recent_ts}")));
    }

    #[tokio::test]
    async fn on_market_discovered_with_boundary_snapshot() {
        let mut strategy = make_strategy_no_chainlink();
        let ctx = StrategyContext::new();

        // Set BTC price in context
        {
            let mut md = ctx.market_data.write().await;
            md.external_prices.insert("BTC".to_string(), dec!(50500));
        }

        // Set up a boundary snapshot for the current window
        let now_ts = Utc::now().timestamp();
        let window_ts = now_ts - (now_ts % WINDOW_SECS);
        strategy.boundary_prices.insert(
            format!("BTC-{window_ts}"),
            BoundarySnapshot {
                timestamp: DateTime::from_timestamp(window_ts, 0).unwrap(),
                price: dec!(50000),
                source: "chainlink".to_string(),
            },
        );

        let mut market = make_market_info("btc-market-1", Utc::now() + Duration::seconds(900));
        market.start_date = DateTime::from_timestamp(window_ts, 0);

        let actions = strategy.on_market_discovered(&market, &ctx).await.unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::SubscribeMarket(_)));

        let mwr = &strategy.active_markets["btc-market-1"];
        // Should use the boundary snapshot, not the current price
        assert_eq!(mwr.reference_price, dec!(50000));
        assert_eq!(mwr.reference_quality, ReferenceQuality::Exact);
    }

    // --- config sub-struct tests ---

    #[test]
    fn config_default_sub_configs() {
        let config = ArbitrageConfig::default();

        // Fee defaults
        assert_eq!(config.fee.taker_fee_rate, dec!(0.0315));

        // Spike defaults
        assert_eq!(config.spike.threshold_pct, dec!(0.005));
        assert_eq!(config.spike.window_secs, 10);
        assert_eq!(config.spike.history_size, 50);

        // Order defaults
        assert!(config.order.hybrid_mode);
        assert_eq!(config.order.limit_offset, dec!(0.01));
        assert_eq!(config.order.max_age_secs, 30);

        // Sizing defaults
        assert_eq!(config.sizing.base_size, dec!(10));
        assert_eq!(config.sizing.kelly_multiplier, dec!(0.25));
        assert_eq!(config.sizing.min_size, dec!(2));
        assert_eq!(config.sizing.max_size, dec!(25));
        assert!(config.sizing.use_kelly);

        // StopLoss defaults
        assert_eq!(config.stop_loss.reversal_pct, dec!(0.005));
        assert_eq!(config.stop_loss.min_drop, dec!(0.05));
        assert!(config.stop_loss.trailing_enabled);
        assert_eq!(config.stop_loss.trailing_distance, dec!(0.03));
        assert!(config.stop_loss.time_decay);

        // Correlation defaults
        assert!(!config.correlation.enabled);
        assert_eq!(config.correlation.min_spike_pct, dec!(0.01));
        assert_eq!(config.correlation.pairs.len(), 2);

        // Performance defaults
        assert_eq!(config.performance.min_trades, 20);
        assert_eq!(config.performance.min_win_rate, dec!(0.40));
        assert_eq!(config.performance.window_size, 50);
        assert!(!config.performance.auto_disable);
    }

    #[test]
    fn config_deserialize_missing_sub_configs() {
        // Minimal TOML with only top-level fields — sub-configs should default.
        let toml_str = r#"
            coins = ["BTC"]
            max_positions = 3
            min_profit_margin = "0.04"
            late_window_margin = "0.03"
            scan_interval_secs = 60
            use_chainlink = false
        "#;
        let config: ArbitrageConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.coins, vec!["BTC"]);
        assert_eq!(config.max_positions, 3);
        assert!(!config.use_chainlink);
        // Sub-configs should have their defaults
        assert_eq!(config.fee.taker_fee_rate, dec!(0.0315));
        assert!(config.order.hybrid_mode);
        assert_eq!(config.sizing.base_size, dec!(10));
        assert_eq!(config.stop_loss.reversal_pct, dec!(0.005));
        assert!(!config.correlation.enabled);
        assert!(!config.performance.auto_disable);
    }

    // --- taker_fee tests ---

    #[test]
    fn taker_fee_at_50_50() {
        // At p=0.50: fee = 2 * 0.50 * 0.50 * 0.0315 = 0.01575
        let fee = taker_fee(dec!(0.50), dec!(0.0315));
        assert_eq!(fee, dec!(0.015750));
    }

    #[test]
    fn taker_fee_at_80() {
        // At p=0.80: fee = 2 * 0.80 * 0.20 * 0.0315 = 0.01008
        let fee = taker_fee(dec!(0.80), dec!(0.0315));
        assert_eq!(fee, dec!(0.010080));
    }

    #[test]
    fn taker_fee_at_95() {
        // At p=0.95: fee = 2 * 0.95 * 0.05 * 0.0315 = 0.0029925
        let fee = taker_fee(dec!(0.95), dec!(0.0315));
        assert_eq!(fee, dec!(0.0029925));
    }

    // --- net_profit_margin tests ---

    #[test]
    fn net_profit_margin_taker() {
        // At p=0.80: gross = 0.20, fee = 0.01008, net = 0.18992
        let net = net_profit_margin(dec!(0.80), dec!(0.0315), false);
        let expected = dec!(0.20) - dec!(0.010080);
        assert_eq!(net, expected);
    }

    #[test]
    fn net_profit_margin_maker() {
        // Maker fee = $0, so net = gross = 1 - price
        let net = net_profit_margin(dec!(0.80), dec!(0.0315), true);
        assert_eq!(net, dec!(0.20));
    }

    // --- fee-aware filtering tests ---

    #[tokio::test]
    async fn confirmed_mode_filtered_at_50_with_small_margin() {
        // At p=0.50 with 3¢ gross margin, net margin < 0 after fee
        // ask = 0.97 → gross = 0.03, fee at 0.97 = 2*0.97*0.03*0.0315 = 0.001837
        // Actually, let's use p=0.50 directly: ask = 0.50, gross = 0.50 but
        // the plan says "Confirmed mode at p=0.50 with 3¢ gross margin is filtered out"
        // This means ask = 0.97 at a 50/50 market. But fee at 0.97 is tiny.
        // More accurately: at mid-range prices where fee is highest.
        // Use ask = 0.55 with min_profit_margin = 0.03.
        // gross = 0.45, fee = 2*0.55*0.45*0.0315 = 0.01559. net = 0.434.
        // That's still > 0.03. Let's find a case where net < min_margin.
        //
        // To filter: net < min_margin(0.02 for late window). Use ask = 0.97.
        // gross = 0.03, fee = 2*0.97*0.03*0.0315 = 0.001837. net = 0.028.
        // Still passes. Need a tighter case.
        //
        // Let's set min_profit_margin = 0.04 and ask = 0.95. gross=0.05, fee=0.003.
        // net=0.047 >= 0.04, passes. Use min_profit_margin = 0.05 instead.
        //
        // Better approach: construct a scenario where fee eats up the margin.
        // ask = 0.50, gross = 0.50, fee = 0.01575, net = 0.484 — still large.
        // The real impact is when gross is tiny. Let's just verify with a custom
        // high fee rate to show the filtering works.
        let mwr = make_mwr(dec!(50000), 200); // late window
        let ctx = StrategyContext::new();

        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.95), dec!(0.97)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.01), dec!(0.03)),
            );
        }

        // Use a high fee rate that makes the 3¢ gross margin negative
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.fee.taker_fee_rate = dec!(0.60); // Extreme fee rate for testing
        config.late_window_margin = dec!(0.02);
        let strategy = CryptoArbitrageStrategy::new(config);

        // Large move for high confidence (52000 vs 50000 = 4%)
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(52000), &ctx)
            .await
            .unwrap();
        // At ask=0.97, gross=0.03, fee=2*0.97*0.03*0.60=0.03492
        // net = 0.03 - 0.03492 = -0.00492 < late_window_margin(0.02) → filtered
        assert!(
            opps.is_empty(),
            "Should filter Confirmed mode when net margin < 0 after fee"
        );
    }

    #[tokio::test]
    async fn tail_end_at_95_still_passes_with_fees() {
        // At p=0.95: fee ≈ 0.003, margin = 0.05, net ≈ 0.047 > 0
        let mwr = make_mwr(dec!(50000), 60); // < 120s
        let ctx = StrategyContext::new();

        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.93), dec!(0.95)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.03), dec!(0.05)),
            );
        }

        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(51000), &ctx)
            .await
            .unwrap();
        assert!(!opps.is_empty(), "Tail-End at 0.95 should still pass");
        let opp = &opps[0];
        assert_eq!(opp.mode, ArbitrageMode::TailEnd);
        // Verify fee is small (~0.3¢)
        assert!(opp.estimated_fee < dec!(0.005));
        // Net margin should be ~4.7¢
        assert!(opp.net_margin > dec!(0.04));
    }

    #[tokio::test]
    async fn two_sided_filtered_when_fees_exceed_margin() {
        // Both asks near 0.49 → combined 0.98 just under threshold
        // but fees on both legs eat up the tiny 2¢ margin
        let mwr = make_mwr(dec!(50000), 400);
        let ctx = StrategyContext::new();

        // 0.48 + 0.49 = 0.97, gross margin = 0.03
        // fee_up = 2*0.48*0.52*0.0315 = 0.01572
        // fee_down = 2*0.49*0.51*0.0315 = 0.01575
        // total_fee = 0.03147, net = 0.03 - 0.03147 = -0.00147 → filtered
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.46), dec!(0.48)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.47), dec!(0.49)),
            );
        }

        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(50100), &ctx)
            .await
            .unwrap();
        assert!(
            opps.is_empty(),
            "Two-Sided should be filtered when fees exceed margin"
        );
    }

    #[test]
    fn config_deserialize_explicit_sub_configs() {
        let toml_str = r#"
            coins = ["BTC", "ETH"]
            max_positions = 10
            min_profit_margin = "0.05"
            late_window_margin = "0.03"
            scan_interval_secs = 15
            use_chainlink = true

            [fee]
            taker_fee_rate = "0.02"

            [spike]
            threshold_pct = "0.01"
            window_secs = 20
            history_size = 100

            [order]
            hybrid_mode = false
            limit_offset = "0.005"
            max_age_secs = 60

            [sizing]
            base_size = "20"
            kelly_multiplier = "0.50"
            min_size = "5"
            max_size = "50"
            use_kelly = false

            [stop_loss]
            reversal_pct = "0.01"
            min_drop = "0.10"
            trailing_enabled = false
            trailing_distance = "0.05"
            time_decay = false

            [correlation]
            enabled = true
            min_spike_pct = "0.02"
            pairs = [["BTC", ["ETH"]]]

            [performance]
            min_trades = 50
            min_win_rate = "0.55"
            window_size = 100
            auto_disable = true
        "#;
        let config: ArbitrageConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.fee.taker_fee_rate, dec!(0.02));
        assert_eq!(config.spike.threshold_pct, dec!(0.01));
        assert_eq!(config.spike.window_secs, 20);
        assert!(!config.order.hybrid_mode);
        assert_eq!(config.order.limit_offset, dec!(0.005));
        assert_eq!(config.sizing.base_size, dec!(20));
        assert!(!config.sizing.use_kelly);
        assert_eq!(config.stop_loss.reversal_pct, dec!(0.01));
        assert!(!config.stop_loss.trailing_enabled);
        assert!(config.correlation.enabled);
        assert_eq!(config.performance.min_trades, 50);
        assert!(config.performance.auto_disable);
    }
}
