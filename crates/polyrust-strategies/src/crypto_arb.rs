use std::collections::{HashMap, VecDeque};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use polyrust_core::prelude::*;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the crypto arbitrage strategy.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArbitrageConfig {
    /// Coins to track (e.g. ["BTC", "ETH", "SOL", "XRP"])
    pub coins: Vec<String>,
    /// USDC amount per trade
    pub position_size: Decimal,
    /// Maximum concurrent positions
    pub max_positions: usize,
    /// Minimum profit margin for confirmed mode
    pub min_profit_margin: Decimal,
    /// Minimum profit margin in late window (120-300s)
    pub late_window_margin: Decimal,
    /// Reversal percentage to trigger stop-loss (e.g. 0.005 = 0.5%)
    pub stop_loss_reversal_pct: Decimal,
    /// Minimum market price drop to confirm stop-loss (e.g. 0.05 = 5¢)
    pub stop_loss_min_drop: Decimal,
    /// Interval in seconds between market discovery scans
    pub scan_interval_secs: u64,
    /// Whether to use Chainlink prices for resolution reference
    pub use_chainlink: bool,
}

impl Default for ArbitrageConfig {
    fn default() -> Self {
        Self {
            coins: vec![
                "BTC".into(),
                "ETH".into(),
                "SOL".into(),
                "XRP".into(),
            ],
            position_size: Decimal::new(5, 0),
            max_positions: 5,
            min_profit_margin: Decimal::new(3, 2),       // 0.03
            late_window_margin: Decimal::new(2, 2),       // 0.02
            stop_loss_reversal_pct: Decimal::new(5, 3),   // 0.005
            stop_loss_min_drop: Decimal::new(5, 2),       // 0.05
            scan_interval_secs: 30,
            use_chainlink: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Market enriched with the reference crypto price at discovery time.
#[derive(Debug, Clone)]
pub struct MarketWithReference {
    pub market: MarketInfo,
    /// Crypto price at the moment the market was discovered
    pub reference_price: Decimal,
    /// True if reference was approximate (mid-window discovery)
    pub reference_approximate: bool,
    pub discovery_time: DateTime<Utc>,
    /// Coin symbol (e.g. "BTC")
    pub coin: String,
}

impl MarketWithReference {
    /// Predict winner based on current crypto price vs reference.
    /// If the current price exceeds reference, "Up" wins; otherwise "Down".
    pub fn predict_winner(&self, current_price: Decimal) -> OutcomeSide {
        if current_price >= self.reference_price {
            OutcomeSide::Up
        } else {
            OutcomeSide::Down
        }
    }

    /// Multi-signal confidence score in [0, 1].
    ///
    /// Three regimes based on time remaining:
    /// - Tail-end (< 120s, market >= 0.90): confidence 1.0
    /// - Late window (120-300s): distance-weighted with market boost
    /// - Early window (> 300s): distance-weighted, lower base
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

        if time_remaining_secs < 120 && market_price >= Decimal::new(90, 2) {
            // Tail-end: highest confidence
            Decimal::ONE
        } else if time_remaining_secs < 300 {
            // Late window
            let base = distance_pct * Decimal::new(66, 0);
            let market_boost =
                Decimal::ONE + (market_price - Decimal::new(50, 2)) * Decimal::new(5, 1);
            let raw = base * market_boost;
            raw.min(Decimal::ONE)
        } else {
            // Early window
            let raw = distance_pct * Decimal::new(50, 0);
            raw.min(Decimal::ONE)
        }
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
pub struct CryptoArbitrageStrategy {
    config: ArbitrageConfig,
    active_markets: HashMap<MarketId, MarketWithReference>,
    price_history: HashMap<String, VecDeque<(DateTime<Utc>, Decimal)>>,
    positions: HashMap<MarketId, Vec<ArbitragePosition>>,
    /// Orders submitted but not yet confirmed — keyed by token_id.
    /// Prevents re-entry while orders are in flight.
    pending_orders: HashMap<TokenId, PendingOrder>,
    last_scan: Option<tokio::time::Instant>,
}

impl CryptoArbitrageStrategy {
    pub fn new(config: ArbitrageConfig) -> Self {
        Self {
            config,
            active_markets: HashMap::new(),
            price_history: HashMap::new(),
            positions: HashMap::new(),
            pending_orders: HashMap::new(),
            last_scan: None,
        }
    }

    // -- Event handlers -----------------------------------------------------

    async fn on_crypto_price(
        &mut self,
        symbol: &str,
        price: Decimal,
        ctx: &StrategyContext,
    ) -> Result<Vec<Action>> {
        // Record price history (keep last 12 entries ≈ 60s at 5s intervals)
        let history = self.price_history.entry(symbol.to_string()).or_default();
        history.push_back((Utc::now(), price));
        if history.len() > 12 {
            history.pop_front();
        }

        let mut actions = Vec::new();

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
            if !opps.is_empty() && (total_positions + total_pending) < self.config.max_positions {
                // For TwoSided mode, compute equal share count across both outcomes
                // so total cost = position_size and each side gets N shares.
                let two_sided_size = if opps.len() == 2
                    && opps[0].mode == ArbitrageMode::TwoSided
                {
                    let combined_price = opps[0].buy_price + opps[1].buy_price;
                    if combined_price > Decimal::ZERO {
                        Some(self.config.position_size / combined_price)
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
                    let size = two_sided_size
                        .unwrap_or_else(|| self.config.position_size / opp.buy_price);
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
        if self.pending_orders.values().any(|p| p.market_id == market.market.id) {
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
        if time_remaining < 120 {
            let predicted = market.predict_winner(current_price);
            let (token_id, ask) = match predicted {
                OutcomeSide::Up | OutcomeSide::Yes => {
                    (&market.market.token_ids.outcome_a, up_ask)
                }
                OutcomeSide::Down | OutcomeSide::No => {
                    (&market.market.token_ids.outcome_b, down_ask)
                }
            };
            if let Some(ask_price) = ask
                && ask_price >= Decimal::new(90, 2)
            {
                let profit_margin = Decimal::ONE - ask_price;
                return Ok(vec![ArbitrageOpportunity {
                    mode: ArbitrageMode::TailEnd,
                    market_id: market.market.id.clone(),
                    outcome_to_buy: predicted,
                    token_id: token_id.clone(),
                    buy_price: ask_price,
                    confidence: Decimal::ONE,
                    profit_margin,
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
                return Ok(vec![
                    ArbitrageOpportunity {
                        mode: ArbitrageMode::TwoSided,
                        market_id: market.market.id.clone(),
                        outcome_to_buy: OutcomeSide::Up,
                        token_id: market.market.token_ids.outcome_a.clone(),
                        buy_price: ua,
                        confidence: Decimal::ONE,
                        profit_margin,
                    },
                    ArbitrageOpportunity {
                        mode: ArbitrageMode::TwoSided,
                        market_id: market.market.id.clone(),
                        outcome_to_buy: OutcomeSide::Down,
                        token_id: market.market.token_ids.outcome_b.clone(),
                        buy_price: da,
                        confidence: Decimal::ONE,
                        profit_margin,
                    },
                ]);
            }
        }

        // 3. Confirmed mode: confidence >= threshold + sufficient margin
        let predicted = market.predict_winner(current_price);
        let (token_id, ask) = match predicted {
            OutcomeSide::Up | OutcomeSide::Yes => {
                (&market.market.token_ids.outcome_a, up_ask)
            }
            OutcomeSide::Down | OutcomeSide::No => {
                (&market.market.token_ids.outcome_b, down_ask)
            }
        };

        if let Some(ask_price) = ask {
            let confidence =
                market.get_confidence(current_price, ask_price, time_remaining);
            let profit_margin = Decimal::ONE - ask_price;
            let min_margin = if time_remaining < 300 {
                self.config.late_window_margin
            } else {
                self.config.min_profit_margin
            };

            if confidence >= Decimal::new(50, 2) && profit_margin >= min_margin {
                return Ok(vec![ArbitrageOpportunity {
                    mode: ArbitrageMode::Confirmed,
                    market_id: market.market.id.clone(),
                    outcome_to_buy: predicted,
                    token_id: token_id.clone(),
                    buy_price: ask_price,
                    confidence,
                    profit_margin,
                }]);
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

        // Check stop-losses on open positions
        let mut actions = Vec::new();
        let position_ids: Vec<MarketId> = self.positions.keys().cloned().collect();

        for market_id in position_ids {
            let positions = match self.positions.get(&market_id) {
                Some(p) => p.clone(),
                None => continue,
            };

            let mut triggered_indices = Vec::new();
            for (i, pos) in positions.iter().enumerate() {
                // Only check if this snapshot is for the position's token
                if pos.token_id != snapshot.token_id {
                    continue;
                }

                if let Some(action) = self.check_stop_loss(pos, snapshot)? {
                    info!(
                        market = %market_id,
                        entry = %pos.entry_price,
                        side = ?pos.side,
                        "Stop-loss triggered, selling position"
                    );
                    triggered_indices.push(i);
                    actions.push(action);
                }
            }

            if !triggered_indices.is_empty()
                && let Some(remaining) = self.positions.get_mut(&market_id)
            {
                for i in triggered_indices.into_iter().rev() {
                    remaining.remove(i);
                }
                if remaining.is_empty() {
                    self.positions.remove(&market_id);
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
            .and_then(|h| h.back().map(|(_, p)| *p));

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
            reversal >= self.config.stop_loss_reversal_pct
        } else {
            false
        };

        // Check market price drop from entry
        let current_bid = match snapshot.best_bid() {
            Some(bid) => bid,
            None => return Ok(None), // No bids — cannot sell, skip stop-loss
        };
        let price_drop = pos.entry_price - current_bid;
        let market_dropped = price_drop >= self.config.stop_loss_min_drop;

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

        // Get the current crypto price as the reference
        let md = ctx.market_data.read().await;
        let reference_price = match md.external_prices.get(&coin) {
            Some(&p) => p,
            None => {
                debug!(coin = %coin, market = %market.id, "No price available for coin, skipping market");
                return Ok(vec![]);
            }
        };

        let mwr = MarketWithReference {
            market: market.clone(),
            reference_price,
            reference_approximate: false,
            discovery_time: Utc::now(),
            coin: coin.clone(),
        };

        info!(
            coin = %coin,
            market = %market.id,
            reference = %reference_price,
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

    // -- Helpers ------------------------------------------------------------

    /// Extract coin symbol from market question string.
    /// Looks for known coin names as whole words in the question text.
    fn extract_coin(&self, question: &str) -> Option<String> {
        let upper = question.to_uppercase();
        for coin in &self.config.coins {
            // Match coin as a whole word to avoid false positives
            // (e.g. "SOL" should not match "SOLVE" or "resolution")
            let mut found = false;
            for (idx, _) in upper.match_indices(coin.as_str()) {
                let before_ok = idx == 0
                    || !upper.as_bytes()[idx - 1].is_ascii_alphanumeric();
                let after_idx = idx + coin.len();
                let after_ok = after_idx >= upper.len()
                    || !upper.as_bytes()[after_idx].is_ascii_alphanumeric();
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
            position_size = %self.config.position_size,
            "Crypto arbitrage strategy started"
        );
        self.last_scan = Some(tokio::time::Instant::now());
        Ok(())
    }

    async fn on_event(
        &mut self,
        event: &Event,
        ctx: &StrategyContext,
    ) -> Result<Vec<Action>> {
        match event {
            Event::MarketData(MarketDataEvent::ExternalPrice {
                symbol, price, ..
            }) => self.on_crypto_price(symbol, *price, ctx).await,

            Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot)) => {
                self.on_orderbook_update(snapshot, ctx).await
            }

            Event::MarketData(MarketDataEvent::MarketDiscovered(market)) => {
                self.on_market_discovered(market, ctx).await
            }

            Event::MarketData(MarketDataEvent::MarketExpired(id)) => {
                self.on_market_expired(id, ctx).await
            }

            Event::OrderUpdate(OrderEvent::Placed(result)) => {
                Ok(self.on_order_placed(result))
            }

            _ => Ok(vec![]),
        }
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
        Ok(actions)
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
            end_date,
            token_ids: TokenIds {
                outcome_a: "token_up".to_string(),
                outcome_b: "token_down".to_string(),
            },
            accepting_orders: true,
            neg_risk: false,
        }
    }

    fn make_mwr(
        reference_price: Decimal,
        time_remaining_secs: i64,
    ) -> MarketWithReference {
        MarketWithReference {
            market: make_market_info(
                "market1",
                Utc::now() + Duration::seconds(time_remaining_secs),
            ),
            reference_price,
            reference_approximate: false,
            discovery_time: Utc::now(),
            coin: "BTC".to_string(),
        }
    }

    fn make_orderbook(
        token_id: &str,
        best_bid: Decimal,
        best_ask: Decimal,
    ) -> OrderbookSnapshot {
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
        assert_eq!(mwr.predict_winner(dec!(50100)), OutcomeSide::Up);
    }

    #[test]
    fn predict_winner_btc_down() {
        let mwr = make_mwr(dec!(50000), 600);
        // Current price below reference => Down
        assert_eq!(mwr.predict_winner(dec!(49900)), OutcomeSide::Down);
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

        // Both asks low: 0.48 + 0.49 = 0.97 < 0.98
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
        assert_eq!(opps.len(), 2, "TwoSided should return both outcomes");
        assert_eq!(opps[0].mode, ArbitrageMode::TwoSided);
        assert_eq!(opps[0].outcome_to_buy, OutcomeSide::Up);
        assert_eq!(opps[1].outcome_to_buy, OutcomeSide::Down);
        assert_eq!(opps[0].profit_margin, dec!(0.03)); // 1.0 - 0.97
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
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

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
        history.push_back((Utc::now(), dec!(49500)));
        strategy
            .price_history
            .insert("BTC".to_string(), history);

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
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

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
        history.push_back((Utc::now(), dec!(49500)));
        strategy
            .price_history
            .insert("BTC".to_string(), history);

        let snapshot = make_orderbook("token_up", dec!(0.50), dec!(0.55));
        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_none());
    }

    #[test]
    fn stop_loss_does_not_trigger_small_drop() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());

        let mwr = make_mwr(dec!(50000), 300);
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

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
        history.push_back((Utc::now(), dec!(49500)));
        strategy
            .price_history
            .insert("BTC".to_string(), history);

        let snapshot = make_orderbook("token_up", dec!(0.57), dec!(0.60));
        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_none());
    }

    // --- market discovery/expiry tests ---

    #[tokio::test]
    async fn on_market_discovered_creates_entry() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        let ctx = StrategyContext::new();

        // Set BTC price in context
        {
            let mut md = ctx.market_data.write().await;
            md.external_prices
                .insert("BTC".to_string(), dec!(50000));
        }

        let market = make_market_info(
            "btc-market-1",
            Utc::now() + Duration::seconds(900),
        );

        let actions = strategy
            .on_market_discovered(&market, &ctx)
            .await
            .unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::SubscribeMarket(_)));
        assert!(strategy.active_markets.contains_key("btc-market-1"));
        assert_eq!(
            strategy.active_markets["btc-market-1"].reference_price,
            dec!(50000)
        );
    }

    #[tokio::test]
    async fn on_market_expired_removes_market() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default());
        let ctx = StrategyContext::new();

        let mwr = make_mwr(dec!(50000), 0);
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

        let actions = strategy
            .on_market_expired("market1", &ctx)
            .await
            .unwrap();
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
        assert_eq!(
            strategy.extract_coin("Random question about stocks"),
            None
        );
    }
}
