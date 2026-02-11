//! TailEnd strategy: High-confidence trades near market expiration.
//!
//! Entry conditions:
//! - Time remaining < 120 seconds
//! - Predicted winner's ask >= 0.90
//! - Confidence: 1.0 (fixed, highest priority)
//!
//! Uses GTC orders with aggressive pricing (at/above ask) for immediate fills.
//! GTC avoids the FOK USDC clamping issue at extreme prices (>0.99) and gets
//! 0% maker fee instead of ~0.06% taker fee.

use std::sync::Arc;

use async_trait::async_trait;
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use polyrust_core::prelude::*;

use crate::crypto_arb::base::{taker_fee, CryptoArbBase};
use crate::crypto_arb::dashboard::try_emit_dashboard_updates;
use crate::crypto_arb::types::{
    ArbitrageMode, ArbitrageOpportunity, ArbitragePosition, OpenLimitOrder, PendingOrder,
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
            self.base.record_tailend_skip("time_window").await;
            return None;
        }

        // Check if mode is disabled
        if self.base.is_mode_disabled(&ArbitrageMode::TailEnd).await {
            debug!(
                market = %market_id,
                "TailEnd skip: mode auto-disabled by performance tracker"
            );
            self.base.record_tailend_skip("auto_disabled").await;
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
            self.base.record_tailend_skip("ref_quality").await;
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
                self.base.record_tailend_skip("no_prediction").await;
                return None;
            }
        };

        let md = ctx.market_data.read().await;
        let token_id = match predicted {
            OutcomeSide::Up | OutcomeSide::Yes => &market.market.token_ids.outcome_a,
            OutcomeSide::Down | OutcomeSide::No => &market.market.token_ids.outcome_b,
        };
        let ob = md.orderbooks.get(token_id);
        let ask = ob.and_then(|ob| ob.best_ask());
        let bid = ob.and_then(|ob| ob.best_bid());
        drop(md);

        let ask_price = match ask {
            Some(p) => p,
            None => {
                debug!(
                    market = %market_id,
                    predicted_side = ?predicted,
                    "TailEnd skip: no ask in orderbook for predicted side"
                );
                self.base.record_tailend_skip("no_ask").await;
                return None;
            }
        };

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
            self.base.record_tailend_skip("threshold").await;
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
                self.base.record_tailend_skip("spread").await;
                return None;
            }
        }

        // Check sustained price direction (momentum filter)
        let min_sustained = self.base.config.tailend.min_sustained_secs;
        let now = ctx.now().await;
        let sustained = self
            .base
            .check_sustained_direction(&market.coin, market.reference_price, predicted, min_sustained, now)
            .await;

        if !sustained {
            debug!(
                market = %market_id,
                coin = %market.coin,
                min_sustained_secs = min_sustained,
                "TailEnd skip: price direction not sustained long enough"
            );
            self.base.record_tailend_skip("sustained").await;
            return None;
        }

        // Check recent volatility (wick filter)
        let max_volatility = self.base.config.tailend.max_recent_volatility;
        if let Some(volatility) = self
            .base
            .max_recent_volatility(&market.coin, market.reference_price, 10, now)
            .await
            && volatility > max_volatility
        {
            debug!(
                market = %market_id,
                volatility = %volatility,
                max_volatility = %max_volatility,
                "TailEnd skip: recent volatility too high (choppy market)"
            );
            self.base.record_tailend_skip("volatility").await;
            return None;
        }

        let profit_margin = Decimal::ONE - ask_price;
        let estimated_fee = Decimal::ZERO; // GTC maker fee = 0%
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

        // GTC orders: track as open limit order; position created on fill event
        if pending.order_type == OrderType::Gtc {
            if let Some(order_id) = &result.order_id {
                info!(
                    order_id = %order_id,
                    market = %pending.market_id,
                    price = %pending.price,
                    "TailEnd GTC limit order placed"
                );
                let mut limits = self.base.open_limit_orders.write().await;
                limits.insert(
                    order_id.clone(),
                    OpenLimitOrder {
                        order_id: order_id.clone(),
                        market_id: pending.market_id,
                        token_id: pending.token_id,
                        side: pending.side,
                        price: pending.price,
                        size: pending.size,
                        reference_price: pending.reference_price,
                        coin: pending.coin,
                        placed_at: self.base.event_time().await,
                        mode: pending.mode,
                        kelly_fraction: pending.kelly_fraction,
                        estimated_fee: pending.estimated_fee,
                        tick_size: pending.tick_size,
                        fee_rate_bps: pending.fee_rate_bps,
                        cancel_pending: false,
                    },
                );
            }
            return vec![];
        }

        // FOK fallback path (stop-loss sells still use FOK)
        let now = self.base.event_time().await;
        let position = ArbitragePosition {
            market_id: pending.market_id.clone(),
            token_id: pending.token_id,
            side: pending.side,
            entry_price: pending.price,
            size: pending.size,
            reference_price: pending.reference_price,
            coin: pending.coin,
            order_id: result.order_id.clone(),
            entry_time: now,
            kelly_fraction: pending.kelly_fraction,
            peak_bid: pending.price,
            mode: pending.mode.clone(),
            estimated_fee: pending.estimated_fee,
            entry_market_price: pending.price,
            tick_size: pending.tick_size,
            fee_rate_bps: pending.fee_rate_bps,
        };

        info!(
            market = %pending.market_id,
            side = ?position.side,
            price = %position.entry_price,
            size = %position.size,
            mode = %pending.mode,
            "TailEnd FOK position confirmed"
        );

        self.base.record_position(position).await;
        vec![]
    }

    /// Handle an external price update: record price, scan near-expiry markets,
    /// evaluate tail-end opportunities, and submit GTC orders.
    async fn handle_external_price(
        &self,
        symbol: &str,
        price: Decimal,
        source: &str,
        ctx: &StrategyContext,
    ) -> Vec<Action> {
        // Record price and promote any pending markets
        let now = ctx.now().await;
        let (_, promote_actions) = self.base.record_price(symbol, price, source, now).await;
        let mut result = promote_actions;

        // Fast pre-filter: skip coins where no market is near expiration.
        // This avoids acquiring active_markets lock + iterating for 99%+ of events.
        {
            let nearest = self.base.coin_nearest_expiry.read().await;
            if let Some(expiry) = nearest.get(symbol) {
                let now = ctx.now().await;
                let secs_remaining = (*expiry - now).num_seconds();
                if secs_remaining > self.base.config.tailend.time_threshold_secs as i64 {
                    self.base.record_tailend_skip("coin_not_near_expiry").await;
                    return result;
                }
            }
        }

        // Find active markets for this coin
        let market_ids: Vec<MarketId> = {
            let markets = self.base.active_markets.read().await;
            markets
                .iter()
                .filter(|(_, m)| m.coin == symbol)
                .map(|(id, _)| id.clone())
                .collect()
        };

        for market_id in market_ids {
            // Skip if market is in stale-removal cooldown
            if self.base.is_stale_market_cooled_down(&market_id).await {
                debug!(
                    market = %market_id,
                    "TailEnd skip: stale market cooldown active"
                );
                self.base.record_tailend_skip("stale_cooldown").await;
                continue;
            }

            // Skip if in FOK rejection cooldown
            if self.base.is_fok_cooled_down(&market_id).await {
                debug!(
                    market = %market_id,
                    "TailEnd skip: FOK rejection cooldown active"
                );
                self.base.record_tailend_skip("fok_cooldown").await;
                continue;
            }

            // Skip if we already have exposure
            if self.base.has_market_exposure(&market_id).await {
                debug!(
                    market = %market_id,
                    "TailEnd skip: already have exposure to market"
                );
                self.base.record_tailend_skip("exposure").await;
                continue;
            }

            // Check position limits
            if !self.base.can_open_position().await {
                debug!(
                    market = %market_id,
                    "TailEnd skip: max positions reached"
                );
                self.base.record_tailend_skip("max_positions").await;
                break;
            }

            // Get market info for order construction
            let market_info = {
                let markets = self.base.active_markets.read().await;
                markets.get(&market_id).cloned()
            };

            if let Some(opp) = self.evaluate_opportunity(&market_id, price, ctx).await {
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
                            let max_age = self.base.config.tailend.stale_ob_secs;
                            let age = ctx
                                .now()
                                .await
                                .signed_duration_since(ob.timestamp)
                                .num_seconds();
                            if age > max_age {
                                warn!(
                                    market = %market_id,
                                    age_secs = age,
                                    max_age_secs = max_age,
                                    "TailEnd skip: stale orderbook"
                                );
                                self.base.record_tailend_skip("stale_ob").await;
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

                // TailEnd uses GTC orders with aggressive pricing (at/above ask).
                // GTC avoids FOK USDC clamping at extreme prices and gets 0% maker fee.
                let limit_offset = self.base.config.order.limit_offset;
                let aggressive_price = (opp.buy_price + limit_offset).min(Decimal::new(99, 2));
                let (neg_risk, tick_size, fee_rate_bps) = match &market_info {
                    Some(m) => (m.market.neg_risk, Some(m.market.tick_size), Some(m.market.fee_rate_bps)),
                    None => (false, None, None),
                };
                let mut order = OrderRequest::new(
                    opp.token_id.clone(),
                    aggressive_price,
                    size,
                    OrderSide::Buy,
                    OrderType::Gtc,
                    neg_risk,
                );
                if let Some(ts) = tick_size { order = order.with_tick_size(ts); }
                if let Some(fr) = fee_rate_bps { order = order.with_fee_rate_bps(fr); }

                info!(
                    mode = ?opp.mode,
                    market = %market_id,
                    confidence = %opp.confidence,
                    ask_price = %opp.buy_price,
                    limit_price = %aggressive_price,
                    size = %size,
                    available_depth = %available,
                    safe_depth = %safe_available,
                    side = ?opp.outcome_to_buy,
                    "Submitting TailEnd GTC order"
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
                            order_type: OrderType::Gtc,
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

    /// Handle an orderbook update: update cached asks, peak bids, check
    /// stop-losses, and trigger post-entry exits on our positions.
    async fn handle_orderbook_update(&self, snapshot: &OrderbookSnapshot) -> Vec<Action> {
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

            // Skip if stop-loss already in flight or in cooldown
            {
                let pending_sl = self.base.pending_stop_loss.read().await;
                if pending_sl.contains_key(&pos.token_id) {
                    continue;
                }
            }
            if self.base.is_stop_loss_cooled_down(&pos.token_id).await {
                continue;
            }

            if let Some((action, exit_price, trigger)) =
                self.base.check_stop_loss(&pos, snapshot, self.base.event_time().await).await
            {
                info!(
                    market = %pos.market_id,
                    entry = %pos.entry_price,
                    exit = %exit_price,
                    side = ?pos.side,
                    reason = trigger.reason,
                    peak_bid = %trigger.peak_bid,
                    effective_distance = %trigger.effective_distance,
                    time_remaining = trigger.time_remaining,
                    "TailEnd stop-loss triggered"
                );
                let mut pending_sl = self.base.pending_stop_loss.write().await;
                pending_sl.insert(pos.token_id.clone(), exit_price);
                actions.push(action);
                continue;
            }

            // Post-entry confirmation: exit if price drops significantly
            // within post_entry_window_secs of entry (catches false signals immediately)
            let seconds_since_entry = self.base.event_time().await
                .signed_duration_since(pos.entry_time)
                .num_seconds();
            let window = self.base.config.tailend.post_entry_window_secs;
            let max_drop = self.base.config.tailend.post_entry_exit_drop;
            if seconds_since_entry <= window
                && let Some(current_bid) = snapshot.best_bid()
            {
                // Exit if bid dropped more than post_entry_exit_drop below entry price
                let exit_threshold = pos.entry_price - max_drop;
                if current_bid < exit_threshold {
                    info!(
                        market = %pos.market_id,
                        entry_price = %pos.entry_price,
                        exit_threshold = %exit_threshold,
                        current_bid = %current_bid,
                        seconds_since_entry = seconds_since_entry,
                        "TailEnd post-entry exit triggered: bid dropped {max_drop} below entry"
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

    /// Handle a fully filled GTC order event.
    async fn on_order_filled(
        &self,
        order_id: &str,
        _token_id: &str,
        price: Decimal,
        size: Decimal,
    ) -> Vec<Action> {
        let lo = {
            let mut limits = self.base.open_limit_orders.write().await;
            match limits.remove(order_id) {
                Some(lo) if lo.mode == ArbitrageMode::TailEnd => lo,
                Some(lo) => {
                    // Not our mode, put it back
                    limits.insert(order_id.to_string(), lo);
                    return vec![];
                }
                None => return vec![],
            }
        };

        info!(
            order_id = %order_id,
            market = %lo.market_id,
            price = %price,
            size = %size,
            "TailEnd GTC order filled"
        );

        let now = self.base.event_time().await;
        let position = ArbitragePosition::from_limit_order(
            &lo,
            price,
            size,
            Some(order_id.to_string()),
            now,
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
        self.base.update_event_time(ctx).await;

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
            }) => self.handle_external_price(symbol, *price, source, ctx).await,

            Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot)) => {
                self.handle_orderbook_update(snapshot).await
            }

            Event::OrderUpdate(OrderEvent::Placed(result)) => self.on_order_placed(result).await,

            Event::OrderUpdate(OrderEvent::Filled {
                order_id,
                token_id,
                price,
                size,
                ..
            }) => {
                self.on_order_filled(order_id, token_id, *price, *size)
                    .await
            }

            Event::OrderUpdate(OrderEvent::Rejected { token_id, reason, .. }) => {
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

                    // Handle stop-loss rejection with balance-aware cleanup
                    if self.base.pending_stop_loss.read().await.contains_key(token_id) {
                        self.base.handle_stop_loss_rejection(token_id, reason, "TailEnd").await;
                    }
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::Cancelled(order_id)) => {
                let mut limits = self.base.open_limit_orders.write().await;
                if let Some(lo) = limits.remove(order_id)
                    && lo.mode == ArbitrageMode::TailEnd
                {
                    info!(
                        order_id = %order_id,
                        market = %lo.market_id,
                        "TailEnd GTC order cancelled"
                    );
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::CancelFailed { order_id, reason }) => {
                let (_found, fill_actions) =
                    self.base.handle_cancel_failed(order_id, reason).await;
                fill_actions
            }

            _ => vec![],
        };

        // Check stale limit orders (TailEnd uses GTC for entries)
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use rust_decimal_macros::dec;

    use crate::crypto_arb::config::ArbitrageConfig;
    use crate::crypto_arb::types::{MarketWithReference, ReferenceQuality};

    fn make_market_info(id: &str, end_date: chrono::DateTime<Utc>) -> polyrust_core::types::MarketInfo {
        polyrust_core::types::MarketInfo {
            id: id.to_string(),
            slug: "btc-up-down".to_string(),
            question: "Will BTC go up?".to_string(),
            start_date: None,
            end_date,
            token_ids: polyrust_core::types::TokenIds {
                outcome_a: "token_up".to_string(),
                outcome_b: "token_down".to_string(),
            },
            accepting_orders: true,
            neg_risk: false,
            min_order_size: dec!(5.0),
            tick_size: dec!(0.01),
            fee_rate_bps: 0,
        }
    }

    async fn make_tailend_strategy(time_remaining: i64) -> (TailEndStrategy, StrategyContext) {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.tailend.enabled = true;
        config.tailend.min_sustained_secs = 5; // Small window to keep test simple
        config.tailend.max_recent_volatility = dec!(1.0); // Disable volatility filter
        let base = Arc::new(CryptoArbBase::new(config, vec![]));

        let market = MarketWithReference {
            market: make_market_info("market1", Utc::now() + Duration::seconds(time_remaining)),
            reference_price: dec!(50000),
            reference_quality: ReferenceQuality::Exact,
            discovery_time: Utc::now(),
            coin: "BTC".to_string(),
        };
        base.active_markets
            .write()
            .await
            .insert("market1".to_string(), market);

        // Populate price history so sustained direction check passes.
        // Use timestamps spread over last 5s to establish direction.
        {
            use std::collections::VecDeque;
            let mut history = base.price_history.write().await;
            let mut entries = VecDeque::new();
            let now = Utc::now();
            // BTC above reference (51000 > 50000) — favors Up direction
            entries.push_back((now - Duration::seconds(3), dec!(51000), "test".to_string()));
            entries.push_back((now - Duration::seconds(1), dec!(51000), "test".to_string()));
            history.insert("BTC".to_string(), entries);
        }

        let ctx = StrategyContext::new();
        let strategy = TailEndStrategy::new(base);
        (strategy, ctx)
    }

    #[tokio::test]
    async fn tailend_generates_order_within_window() {
        let (strategy, ctx) = make_tailend_strategy(60).await;

        // Set up orderbook with ask >= threshold (0.93 at 60s), tight spread
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_up".to_string(),
                    bids: vec![polyrust_core::types::OrderbookLevel { price: dec!(0.935), size: dec!(100) }],
                    asks: vec![polyrust_core::types::OrderbookLevel { price: dec!(0.94), size: dec!(100) }],
                    timestamp: Utc::now(),
                },
            );
        }

        // BTC price above reference → predicts Up → token_up
        let opp = strategy
            .evaluate_opportunity(&"market1".to_string(), dec!(51000), &ctx)
            .await;
        assert!(opp.is_some());
        let opp = opp.unwrap();
        assert_eq!(opp.mode, ArbitrageMode::TailEnd);
        assert_eq!(opp.token_id, "token_up");
        assert_eq!(opp.buy_price, dec!(0.94));
    }

    #[tokio::test]
    async fn tailend_skips_outside_window() {
        let (strategy, ctx) = make_tailend_strategy(200).await; // > 120s threshold

        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_up".to_string(),
                    bids: vec![polyrust_core::types::OrderbookLevel { price: dec!(0.92), size: dec!(100) }],
                    asks: vec![polyrust_core::types::OrderbookLevel { price: dec!(0.95), size: dec!(100) }],
                    timestamp: Utc::now(),
                },
            );
        }

        let opp = strategy
            .evaluate_opportunity(&"market1".to_string(), dec!(51000), &ctx)
            .await;
        assert!(opp.is_none());
    }

    #[tokio::test]
    async fn tailend_skips_below_threshold() {
        let (strategy, ctx) = make_tailend_strategy(60).await;

        // At 60s, dynamic threshold is 0.93. Set ask to 0.89 (below threshold).
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_up".to_string(),
                    bids: vec![polyrust_core::types::OrderbookLevel { price: dec!(0.87), size: dec!(100) }],
                    asks: vec![polyrust_core::types::OrderbookLevel { price: dec!(0.89), size: dec!(100) }],
                    timestamp: Utc::now(),
                },
            );
        }

        let opp = strategy
            .evaluate_opportunity(&"market1".to_string(), dec!(51000), &ctx)
            .await;
        assert!(opp.is_none());
    }

    #[tokio::test]
    async fn tailend_dynamic_threshold_tightens() {
        let strategy_constructor = |time: i64| async move {
            let (s, _) = make_tailend_strategy(time).await;
            s
        };

        let s120 = strategy_constructor(120).await;
        let s30 = strategy_constructor(30).await;

        let t120 = s120.get_ask_threshold(120);
        let t30 = s30.get_ask_threshold(30);

        // At 120s → 0.90, at 30s → 0.95
        assert_eq!(t120, dec!(0.90));
        assert_eq!(t30, dec!(0.95));
        assert!(t30 > t120);
    }

    #[tokio::test]
    async fn tailend_respects_max_spread() {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.tailend.enabled = true;
        config.tailend.min_sustained_secs = 0;
        config.tailend.max_recent_volatility = dec!(1.0);
        config.tailend.max_spread_bps = dec!(50); // 50 bps = 0.5%
        let base = Arc::new(CryptoArbBase::new(config, vec![]));

        let market = MarketWithReference {
            market: make_market_info("market1", Utc::now() + Duration::seconds(60)),
            reference_price: dec!(50000),
            reference_quality: ReferenceQuality::Exact,
            discovery_time: Utc::now(),
            coin: "BTC".to_string(),
        };
        base.active_markets
            .write()
            .await
            .insert("market1".to_string(), market);

        let ctx = StrategyContext::new();
        let strategy = TailEndStrategy::new(base);

        // Wide spread: bid=0.90, ask=0.95 → spread=5.4% >> 0.5%
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_up".to_string(),
                    bids: vec![polyrust_core::types::OrderbookLevel { price: dec!(0.90), size: dec!(100) }],
                    asks: vec![polyrust_core::types::OrderbookLevel { price: dec!(0.95), size: dec!(100) }],
                    timestamp: Utc::now(),
                },
            );
        }

        let opp = strategy
            .evaluate_opportunity(&"market1".to_string(), dec!(51000), &ctx)
            .await;
        assert!(opp.is_none());
    }
}
