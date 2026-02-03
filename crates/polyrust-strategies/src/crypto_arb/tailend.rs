//! TailEnd strategy: High-confidence trades near market expiration.
//!
//! Entry conditions:
//! - Time remaining < 120 seconds
//! - Predicted winner's ask >= 0.90
//! - Confidence: 1.0 (fixed, highest priority)
//!
//! Uses FOK orders for speed (taker fee ~0% at extreme prices).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use polyrust_core::prelude::*;

use crate::crypto_arb::base::{taker_fee, CryptoArbBase};
use crate::crypto_arb::dashboard::try_emit_dashboard_updates;
use crate::crypto_arb::types::{
    ArbitrageMode, ArbitrageOpportunity, ArbitragePosition, PendingOrder,
};

/// TailEnd strategy: trades near expiration with high market prices.
pub struct TailEndStrategy {
    base: Arc<CryptoArbBase>,
}

impl TailEndStrategy {
    pub fn new(base: Arc<CryptoArbBase>) -> Self {
        Self { base }
    }

    /// Get the dynamic ask threshold based on time remaining.
    /// Uses the tightest (highest) threshold where time_remaining <= bucket threshold.
    /// Falls back to legacy ask_threshold if no dynamic thresholds match.
    #[cfg(test)]
    pub(crate) fn get_ask_threshold(&self, time_remaining_secs: i64) -> Decimal {
        self.get_ask_threshold_impl(time_remaining_secs)
    }

    fn get_ask_threshold_impl(&self, time_remaining_secs: i64) -> Decimal {
        let thresholds = &self.base.config.tailend.dynamic_thresholds;

        // Sort by time bucket ascending to find the tightest applicable threshold first
        let mut sorted = thresholds.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        for (bucket_secs, threshold) in sorted {
            if time_remaining_secs <= bucket_secs as i64 {
                return threshold;
            }
        }

        // Fallback to legacy threshold
        self.base.config.tailend.ask_threshold
    }

    /// Evaluate tail-end opportunity for a market.
    async fn evaluate_opportunity(
        &self,
        market_id: &MarketId,
        current_price: Decimal,
        ctx: &StrategyContext,
    ) -> Option<ArbitrageOpportunity> {
        let markets = self.base.active_markets.read().await;
        let market = match markets.get(market_id) {
            Some(m) => m,
            None => {
                debug!(
                    market = %market_id,
                    "TailEnd skip: market not in active_markets"
                );
                return None;
            }
        };

        let now = ctx.now().await;
        let time_remaining = market.market.seconds_remaining_at(now);

        // Must be within the tail-end window
        if time_remaining >= self.base.config.tailend.time_threshold_secs as i64
            || time_remaining <= 0
        {
            debug!(
                market = %market_id,
                time_remaining = time_remaining,
                "TailEnd skip: time outside (0, 120) window"
            );
            return None;
        }

        // Check if mode is disabled
        if self.base.is_mode_disabled(&ArbitrageMode::TailEnd).await {
            debug!(
                market = %market_id,
                "TailEnd skip: mode auto-disabled by performance tracker"
            );
            return None;
        }

        // Check reference quality against configured threshold
        let min_quality = self.base.config.tailend.min_reference_quality;
        if !market.reference_quality.meets_threshold(min_quality) {
            debug!(
                market = %market_id,
                quality = ?market.reference_quality,
                min_quality = ?min_quality,
                "TailEnd skip: reference quality below threshold"
            );
            return None;
        }

        // Predict winner based on crypto price
        let predicted = match market.predict_winner(current_price) {
            Some(side) => side,
            None => {
                debug!(
                    market = %market_id,
                    current_price = %current_price,
                    reference_price = %market.reference_price,
                    "TailEnd skip: no prediction (price == reference)"
                );
                return None;
            }
        };

        let md = ctx.market_data.read().await;
        let (token_id, ask) = match predicted {
            OutcomeSide::Up | OutcomeSide::Yes => (
                &market.market.token_ids.outcome_a,
                md.orderbooks
                    .get(&market.market.token_ids.outcome_a)
                    .and_then(|ob| ob.best_ask()),
            ),
            OutcomeSide::Down | OutcomeSide::No => (
                &market.market.token_ids.outcome_b,
                md.orderbooks
                    .get(&market.market.token_ids.outcome_b)
                    .and_then(|ob| ob.best_ask()),
            ),
        };

        let ask_price = match ask {
            Some(p) => p,
            None => {
                debug!(
                    market = %market_id,
                    predicted_side = ?predicted,
                    "TailEnd skip: no ask in orderbook for predicted side"
                );
                return None;
            }
        };

        // Get best bid for spread calculation
        let bid = match predicted {
            OutcomeSide::Up | OutcomeSide::Yes => {
                md.orderbooks
                    .get(&market.market.token_ids.outcome_a)
                    .and_then(|ob| ob.best_bid())
            }
            OutcomeSide::Down | OutcomeSide::No => {
                md.orderbooks
                    .get(&market.market.token_ids.outcome_b)
                    .and_then(|ob| ob.best_bid())
            }
        };
        drop(md);

        // Ask must be >= dynamic threshold for tail-end (based on time remaining)
        let ask_threshold = self.get_ask_threshold_impl(time_remaining);
        if ask_price < ask_threshold {
            debug!(
                market = %market_id,
                ask = %ask_price,
                threshold = %ask_threshold,
                time_remaining = time_remaining,
                "TailEnd skip: ask below dynamic threshold"
            );
            return None;
        }

        // Check spread to filter out illiquid markets
        if let Some(bid_price) = bid
            && bid_price > Decimal::ZERO
            && ask_price > Decimal::ZERO
        {
            let spread = ask_price - bid_price;
            let mid_price = (ask_price + bid_price) / Decimal::new(2, 0);
            let spread_pct = spread / mid_price;
            let max_spread = self.base.config.tailend.max_spread_bps / Decimal::new(10000, 0);

            if spread_pct > max_spread {
                debug!(
                    market = %market_id,
                    spread_pct = %spread_pct,
                    max_spread = %max_spread,
                    bid = %bid_price,
                    ask = %ask_price,
                    "TailEnd skip: spread too wide (illiquidity filter)"
                );
                return None;
            }
        }

        // Check sustained price direction (momentum filter)
        let min_sustained = self.base.config.tailend.min_sustained_secs;
        let sustained = self
            .base
            .check_sustained_direction(&market.coin, market.reference_price, predicted, min_sustained)
            .await;

        if !sustained {
            debug!(
                market = %market_id,
                coin = %market.coin,
                min_sustained_secs = min_sustained,
                "TailEnd skip: price direction not sustained long enough"
            );
            return None;
        }

        // Check recent volatility (wick filter)
        let max_volatility = self.base.config.tailend.max_recent_volatility;
        if let Some(volatility) = self
            .base
            .max_recent_volatility(&market.coin, market.reference_price, 10)
            .await
            && volatility > max_volatility
        {
            debug!(
                market = %market_id,
                volatility = %volatility,
                max_volatility = %max_volatility,
                "TailEnd skip: recent volatility too high (choppy market)"
            );
            return None;
        }

        let profit_margin = Decimal::ONE - ask_price;
        let estimated_fee = taker_fee(ask_price, self.base.config.fee.taker_fee_rate);
        let net_margin = profit_margin - estimated_fee;

        // Apply quality factor to confidence (reduces position size via Kelly sizing)
        let quality_factor = market.reference_quality.quality_factor();
        let confidence = Decimal::ONE * quality_factor;

        Some(ArbitrageOpportunity {
            mode: ArbitrageMode::TailEnd,
            market_id: market_id.clone(),
            outcome_to_buy: predicted,
            token_id: token_id.clone(),
            buy_price: ask_price,
            confidence,
            profit_margin,
            estimated_fee,
            net_margin,
        })
    }

    /// Handle order placement result.
    async fn on_order_placed(&self, result: &OrderResult) -> Vec<Action> {
        // Check if this is a stop-loss sell confirmation
        {
            let mut pending_sl = self.base.pending_stop_loss.write().await;
            if let Some(exit_price) = pending_sl.remove(&result.token_id) {
                if result.success {
                    if let Some(pos) = self.base.remove_position_by_token(&result.token_id).await {
                        if pos.mode == ArbitrageMode::TailEnd {
                            let exit_fee =
                                taker_fee(exit_price, self.base.config.fee.taker_fee_rate);
                            let pnl = (exit_price - pos.entry_price) * pos.size
                                - (pos.estimated_fee * pos.size)
                                - (exit_fee * pos.size);
                            self.base.record_trade_pnl(&pos.mode, pnl).await;
                            info!(
                                token_id = %result.token_id,
                                mode = %pos.mode,
                                pnl = %pnl,
                                "TailEnd stop-loss sell confirmed"
                            );
                        } else {
                            // Not our mode, put position back
                            self.base.record_position(pos).await;
                        }
                    }
                } else {
                    warn!(
                        token_id = %result.token_id,
                        message = %result.message,
                        "TailEnd stop-loss sell failed"
                    );
                }
                return vec![];
            }
        }

        let pending = {
            let mut orders = self.base.pending_orders.write().await;
            match orders.remove(&result.token_id) {
                Some(p) if p.mode == ArbitrageMode::TailEnd => p,
                Some(p) => {
                    // Not our mode, put it back
                    orders.insert(result.token_id.clone(), p);
                    return vec![];
                }
                None => return vec![],
            }
        };

        if !result.success {
            warn!(
                token_id = %result.token_id,
                market = %pending.market_id,
                message = %result.message,
                "TailEnd order rejected"
            );
            return vec![];
        }

        // FOK orders fill immediately — create position now
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
            kelly_fraction: pending.kelly_fraction,
            peak_bid: pending.price,
            mode: pending.mode.clone(),
            estimated_fee: pending.estimated_fee,
            entry_market_price: pending.price, // Entry price is the market price at entry
            tick_size: pending.tick_size,
            fee_rate_bps: pending.fee_rate_bps,
        };

        info!(
            market = %pending.market_id,
            side = ?position.side,
            price = %position.entry_price,
            size = %position.size,
            mode = %pending.mode,
            "TailEnd position confirmed after order fill"
        );

        self.base.record_position(position).await;
        vec![]
    }
}

#[async_trait]
impl Strategy for TailEndStrategy {
    fn name(&self) -> &str {
        "crypto-arb-tailend"
    }

    fn description(&self) -> &str {
        "Tail-end arbitrage: trades near expiration with high market prices"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        info!(
            coins = ?self.base.config.coins,
            "TailEnd strategy started"
        );
        Ok(())
    }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        let mut actions = match event {
            Event::MarketData(MarketDataEvent::MarketDiscovered(market)) => {
                self.base.on_market_discovered(market, ctx).await
            }

            Event::MarketData(MarketDataEvent::MarketExpired(id)) => {
                self.base.on_market_expired(id).await
            }

            Event::MarketData(MarketDataEvent::ExternalPrice {
                symbol,
                price,
                source,
                ..
            }) => {
                // Record price and promote any pending markets
                let (_, promote_actions) =
                    self.base.record_price(symbol, *price, source).await;
                let mut result = promote_actions;

                // Find active markets for this coin
                let market_ids: Vec<MarketId> = {
                    let markets = self.base.active_markets.read().await;
                    markets
                        .iter()
                        .filter(|(_, m)| m.coin == *symbol)
                        .map(|(id, _)| id.clone())
                        .collect()
                };

                for market_id in market_ids {
                    // Skip if in FOK rejection cooldown
                    if self.base.is_fok_cooled_down(&market_id).await {
                        debug!(
                            market = %market_id,
                            "TailEnd skip: FOK rejection cooldown active"
                        );
                        continue;
                    }

                    // Skip if we already have exposure
                    if self.base.has_market_exposure(&market_id).await {
                        debug!(
                            market = %market_id,
                            "TailEnd skip: already have exposure to market"
                        );
                        continue;
                    }

                    // Check position limits
                    if !self.base.can_open_position().await {
                        debug!(
                            market = %market_id,
                            "TailEnd skip: max positions reached"
                        );
                        break;
                    }

                    // Get market info for order construction
                    let market_info = {
                        let markets = self.base.active_markets.read().await;
                        markets.get(&market_id).cloned()
                    };

                    if let Some(opp) = self.evaluate_opportunity(&market_id, *price, ctx).await {
                        if opp.buy_price.is_zero() {
                            warn!(market = %market_id, "skipping TailEnd opportunity with zero buy_price");
                            continue;
                        }

                        // TailEnd uses fixed sizing (no Kelly - confidence is always 1.0)
                        // Round to 2dp immediately — raw division produces 28+ decimals
                        // which confuses logs and depth comparisons
                        let mut size = (self.base.config.sizing.base_size / opp.buy_price)
                            .round_dp_with_strategy(
                                2,
                                rust_decimal::RoundingStrategy::ToZero,
                            );

                        // Cap to available orderbook depth to avoid guaranteed FOK rejection
                        let available = {
                            let md = ctx.market_data.read().await;
                            match md.orderbooks.get(&opp.token_id) {
                                Some(ob) => {
                                    // Skip if orderbook snapshot is stale (>2s old)
                                    // In competitive tail-end windows, 5s is an eternity
                                    let age = Utc::now()
                                        .signed_duration_since(ob.timestamp)
                                        .num_seconds();
                                    if age > 2 {
                                        warn!(
                                            market = %market_id,
                                            age_secs = age,
                                            "TailEnd skip: stale orderbook"
                                        );
                                        continue;
                                    }
                                    // Cumulative depth at all ask levels up to our buy price
                                    ob.ask_depth_up_to(opp.buy_price)
                                }
                                None => Decimal::ZERO,
                            }
                        };
                        // 50% safety margin — 80% was still too aggressive for competitive tail-end windows
                        // where multiple bots race for the same asks during ~1-5s total latency
                        let safe_available = available * Decimal::new(50, 2);
                        if safe_available < size {
                            warn!(
                                market = %market_id,
                                wanted = %size,
                                available = %available,
                                safe_available = %safe_available,
                                "TailEnd: capping order size to safe available ask depth"
                            );
                            size = safe_available;
                        }

                        // Validate minimum order size
                        if !self.base.validate_min_order_size(&market_id, size).await {
                            continue;
                        }

                        // TailEnd always uses FOK orders (speed matters)
                        let order = if let Some(ref market) = market_info {
                            OrderRequest::new(
                                opp.token_id.clone(),
                                opp.buy_price,
                                size,
                                OrderSide::Buy,
                                OrderType::Fok,
                                market.market.neg_risk,
                            )
                            .with_tick_size(market.market.tick_size)
                            .with_fee_rate_bps(market.market.fee_rate_bps)
                        } else {
                            OrderRequest::new(
                                opp.token_id.clone(),
                                opp.buy_price,
                                size,
                                OrderSide::Buy,
                                OrderType::Fok,
                                false,
                            )
                        };

                        info!(
                            mode = ?opp.mode,
                            market = %market_id,
                            confidence = %opp.confidence,
                            price = %opp.buy_price,
                            size = %size,
                            available_depth = %available,
                            safe_depth = %safe_available,
                            side = ?opp.outcome_to_buy,
                            "Submitting TailEnd order"
                        );

                        // Track pending order
                        if let Some(market) = market_info {
                            let mut pending = self.base.pending_orders.write().await;
                            pending.insert(
                                opp.token_id.clone(),
                                PendingOrder {
                                    market_id: market_id.clone(),
                                    token_id: opp.token_id.clone(),
                                    side: opp.outcome_to_buy,
                                    price: opp.buy_price,
                                    size,
                                    reference_price: market.reference_price,
                                    coin: market.coin.clone(),
                                    order_type: OrderType::Fok,
                                    mode: ArbitrageMode::TailEnd,
                                    kelly_fraction: None,
                                    estimated_fee: opp.estimated_fee,
                                    tick_size: market.market.tick_size,
                                    fee_rate_bps: market.market.fee_rate_bps,
                                    },
                                );
                        }

                        result.push(Action::PlaceOrder(order));
                    }
                }

                result
            }

            Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot)) => {
                // Update cached asks
                if let Some(best_ask) = snapshot.asks.first() {
                    let mut cached = self.base.cached_asks.write().await;
                    cached.insert(snapshot.token_id.clone(), best_ask.price);
                }

                // Update peak_bid for trailing stop
                if let Some(current_bid) = snapshot.best_bid() {
                    self.base
                        .update_peak_bid(&snapshot.token_id, current_bid)
                        .await;
                }

                // Check stop-losses on our positions
                let mut actions = Vec::new();
                let position_ids: Vec<(MarketId, ArbitragePosition)> = {
                    let positions = self.base.positions.read().await;
                    positions
                        .iter()
                        .flat_map(|(mid, plist)| plist.iter().map(|p| (mid.clone(), p.clone())))
                        .filter(|(_, p)| p.mode == ArbitrageMode::TailEnd)
                        .collect()
                };

                for (_, pos) in position_ids {
                    if pos.token_id != snapshot.token_id {
                        continue;
                    }

                    // Skip if stop-loss already in flight
                    {
                        let pending_sl = self.base.pending_stop_loss.read().await;
                        if pending_sl.contains_key(&pos.token_id) {
                            continue;
                        }
                    }

                    if let Some((action, exit_price)) =
                        self.base.check_stop_loss(&pos, snapshot).await
                    {
                        info!(
                            market = %pos.market_id,
                            entry = %pos.entry_price,
                            exit = %exit_price,
                            side = ?pos.side,
                            "TailEnd stop-loss triggered"
                        );
                        let mut pending_sl = self.base.pending_stop_loss.write().await;
                        pending_sl.insert(pos.token_id.clone(), exit_price);
                        actions.push(action);
                        continue;
                    }

                    // Post-entry confirmation: exit if price drops significantly
                    // within 10 seconds of entry (catches false signals immediately)
                    let seconds_since_entry = Utc::now()
                        .signed_duration_since(pos.entry_time)
                        .num_seconds();
                    if seconds_since_entry <= 10
                        && let Some(current_bid) = snapshot.best_bid()
                    {
                        // Exit if market price drops below 0.85 (85%)
                        let post_entry_exit_threshold = Decimal::new(85, 2);
                        if current_bid < post_entry_exit_threshold {
                            info!(
                                market = %pos.market_id,
                                entry_market_price = %pos.entry_market_price,
                                current_bid = %current_bid,
                                seconds_since_entry = seconds_since_entry,
                                "TailEnd post-entry exit triggered: price dropped below 0.85"
                            );
                            let order = OrderRequest::new(
                                pos.token_id.clone(),
                                current_bid,
                                pos.size,
                                OrderSide::Sell,
                                OrderType::Fok,
                                false, // neg_risk - will be set from market info if needed
                            )
                            .with_tick_size(pos.tick_size)
                            .with_fee_rate_bps(pos.fee_rate_bps);
                            let mut pending_sl = self.base.pending_stop_loss.write().await;
                            pending_sl.insert(pos.token_id.clone(), current_bid);
                            actions.push(Action::PlaceOrder(order));
                        }
                    }
                }

                actions
            }

            Event::OrderUpdate(OrderEvent::Placed(result)) => self.on_order_placed(result).await,

            Event::OrderUpdate(OrderEvent::Rejected { token_id, .. }) => {
                if let Some(token_id) = token_id {
                    // Clear pending buy order if it's ours and record cooldown
                    let mut pending = self.base.pending_orders.write().await;
                    if let Some(p) = pending.get(token_id)
                        && p.mode == ArbitrageMode::TailEnd
                    {
                        let market_id = p.market_id.clone();
                        pending.remove(token_id);
                        drop(pending);

                        let cooldown = self.base.config.tailend.fok_cooldown_secs;
                        self.base.record_fok_cooldown(&market_id, cooldown).await;
                        warn!(
                            token_id = %token_id,
                            market = %market_id,
                            cooldown_secs = cooldown,
                            "TailEnd FOK order rejected, cooldown applied"
                        );
                    }

                    // Clear pending stop-loss
                    let mut pending_sl = self.base.pending_stop_loss.write().await;
                    if pending_sl.remove(token_id).is_some() {
                        warn!(
                            token_id = %token_id,
                            "TailEnd stop-loss sell rejected"
                        );
                    }
                }
                vec![]
            }

            _ => vec![],
        };

        // Check stale limit orders (TailEnd doesn't use GTC, but check anyway for shared state)
        actions.extend(self.base.check_stale_limit_orders().await);

        // Emit SSE dashboard updates (throttled to ~5s across all strategies)
        actions.extend(try_emit_dashboard_updates(&self.base).await);

        // Periodic pipeline status summary (every 60s)
        self.base.maybe_log_status_summary().await;

        Ok(actions)
    }

    async fn on_stop(&mut self, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        info!("TailEnd strategy stopping");
        Ok(vec![])
    }

    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        None // Uses shared dashboard
    }
}
