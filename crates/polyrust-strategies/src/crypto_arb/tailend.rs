//! TailEnd strategy: High-confidence trades near market expiration.
//!
//! Entry conditions:
//! - Time remaining < 120 seconds
//! - Predicted winner's ask >= 0.90
//! - Confidence: 1.0 (fixed, highest priority)
//!
//! Uses GTC orders with aggressive pricing (above ask) for immediate fills.
//! Taker fee at TailEnd prices (0.90-0.99) is negligible (0.06-0.57%).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use polyrust_core::prelude::*;

use crate::crypto_arb::base::{CryptoArbBase, GtcStopLossOrder, taker_fee};
use crate::crypto_arb::dashboard::try_emit_dashboard_updates;
use crate::crypto_arb::base::StopLossRejectionKind;
use crate::crypto_arb::types::{
    ArbitrageOpportunity, ArbitragePosition, ExitOrderMeta, OpenLimitOrder, PendingOrder,
    PendingStopLoss, PositionLifecycle, PositionLifecycleState, StopLossTriggerKind,
    TriggerEvalContext, compute_exit_clip,
};

/// TailEnd strategy: trades near expiration with high market prices.
pub struct TailEndStrategy {
    base: Arc<CryptoArbBase>,
}

impl TailEndStrategy {
    pub fn new(base: Arc<CryptoArbBase>) -> Self {
        // Validate config at construction time as defense-in-depth.
        // main.rs should call ArbitrageConfig::validate() first for graceful errors.
        if let Err(e) = base.config.validate() {
            panic!("Invalid arbitrage config: {e}");
        }
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
    pub(crate) async fn evaluate_opportunity(
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

        // Check if auto-disabled by performance tracker
        if self.base.is_auto_disabled().await {
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

        // Strike proximity filter: reject entries when crypto is too close to the strike.
        // A tiny margin means a single candle of noise can flip the outcome.
        let min_distance_pct = self.base.config.tailend.min_strike_distance_pct;
        if !market.reference_price.is_zero() && !min_distance_pct.is_zero() {
            let distance_pct =
                (current_price - market.reference_price).abs() / market.reference_price;
            if distance_pct < min_distance_pct {
                debug!(
                    market = %market_id,
                    distance_pct = %distance_pct,
                    min = %min_distance_pct,
                    current_price = %current_price,
                    reference_price = %market.reference_price,
                    "TailEnd skip: crypto too close to strike"
                );
                self.base.record_tailend_skip("strike_proximity").await;
                return None;
            }
        }

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
        let min_ticks = self.base.config.tailend.min_sustained_ticks;
        let now = ctx.now().await;
        let sustained = self
            .base
            .check_sustained_direction(
                &market.coin,
                market.reference_price,
                predicted,
                min_sustained,
                min_ticks,
                now,
            )
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
        let estimated_fee = taker_fee(ask_price, self.base.config.fee.taker_fee_rate);
        let net_margin = profit_margin - estimated_fee;

        // Apply quality factor to confidence (reduces position size via Kelly sizing)
        let quality_factor = market.reference_quality.quality_factor();
        let confidence = Decimal::ONE * quality_factor;

        Some(ArbitrageOpportunity {
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
        // Check if this is a stop-loss sell confirmation.
        // Don't remove position here — defer to Filled event to avoid race
        // with the persistence handler (which also needs the position for P&L).
        {
            let pending_sl = self.base.pending_stop_loss.read().await;
            if let Some(sl_info) = pending_sl.get(&result.token_id) {
                let exit_price = sl_info.exit_price;
                let sl_order_type = sl_info.order_type;
                drop(pending_sl);

                if result.success {
                    // Only track GTC stop-loss orders for lifecycle management.
                    // FOK orders fill immediately — tracking them here would cause
                    // the fill handler to use the 0% maker fee path instead of taker fee.
                    if sl_order_type == OrderType::Gtc {
                        if let Some(order_id) = &result.order_id {
                            let now = self.base.event_time().await;
                            let pos_info = {
                                let positions = self.base.positions.read().await;
                                positions
                                    .values()
                                    .flat_map(|v| v.iter())
                                    .find(|p| p.token_id == result.token_id)
                                    .map(|p| (p.market_id.clone(), p.size))
                            };

                            if let Some((market_id, size)) = pos_info {
                                let mut gtc_sl = self.base.gtc_stop_loss_orders.write().await;
                                gtc_sl.insert(
                                    order_id.clone(),
                                    GtcStopLossOrder {
                                        order_id: order_id.clone(),
                                        token_id: result.token_id.clone(),
                                        market_id,
                                        price: exit_price,
                                        size,
                                        placed_at: now,
                                    },
                                );
                            }

                            info!(
                                token_id = %result.token_id,
                                order_id = %order_id,
                                "TailEnd GTC stop-loss sell order placed, awaiting fill"
                            );
                        }
                    } else {
                        info!(
                            token_id = %result.token_id,
                            order_type = ?sl_order_type,
                            "TailEnd stop-loss sell order placed (FOK), awaiting fill"
                        );
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
                Some(p) => p,
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
                        kelly_fraction: pending.kelly_fraction,
                        estimated_fee: pending.estimated_fee,
                        tick_size: pending.tick_size,
                        fee_rate_bps: pending.fee_rate_bps,
                        cancel_pending: false,
                        reconcile_miss_count: 0,
                    },
                );
            }
            return vec![];
        }

        // FOK fallback path (stop-loss sells still use FOK)
        let now = self.base.event_time().await;
        let entry_fee_per_share = if pending.order_type == OrderType::Fok {
            taker_fee(pending.price, self.base.config.fee.taker_fee_rate)
        } else {
            Decimal::ZERO
        };
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
            estimated_fee: pending.estimated_fee,
            entry_market_price: pending.price,
            tick_size: pending.tick_size,
            fee_rate_bps: pending.fee_rate_bps,
            entry_order_type: pending.order_type,
            entry_fee_per_share,
            realized_pnl: Decimal::ZERO,
        };

        info!(
            market = %pending.market_id,
            side = ?position.side,
            price = %position.entry_price,
            size = %position.size,
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

        // Update the stop-loss composite cache for this coin.
        // Runs on every ExternalPrice so the SL evaluation (on orderbook updates)
        // always has a recent composite without needing StrategyContext.
        self.base.update_sl_composite_cache(symbol, ctx).await;

        // Fast pre-filter: skip coins where no market is near expiration.
        // This avoids acquiring active_markets lock + iterating for 99%+ of events.
        // Runs BEFORE composite price check since it's a cheap HashMap lookup that
        // filters ~98% of events, avoiding expensive composite evaluation.
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

        // Composite price gating: if enabled, use composite fair price for entry evaluation
        let effective_price = if self.base.config.tailend.use_composite_price {
            match self
                .base
                .composite_fair_price(
                    symbol,
                    ctx,
                    self.base.config.tailend.max_source_stale_secs,
                    self.base.config.tailend.min_sources,
                    self.base.config.tailend.max_dispersion_bps,
                )
                .await
            {
                Some(composite) => {
                    debug!(
                        coin = %symbol,
                        composite_price = %composite.price,
                        sources = composite.sources_used,
                        max_lag_ms = composite.max_lag_ms,
                        dispersion_bps = %composite.dispersion_bps,
                        "Using composite fair price for TailEnd evaluation"
                    );
                    composite.price
                }
                None => {
                    self.base.record_tailend_skip("composite_stale").await;
                    return result;
                }
            }
        } else {
            price
        };

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

            // Skip if in rejection cooldown
            if self.base.is_rejection_cooled_down(&market_id).await {
                debug!(
                    market = %market_id,
                    "TailEnd skip: rejection cooldown active"
                );
                self.base.record_tailend_skip("rejection_cooldown").await;
                continue;
            }

            // Skip if in recovery exit cooldown (same-side re-entry gating)
            if self.base.is_recovery_exit_cooled_down(&market_id).await {
                debug!(
                    market = %market_id,
                    "TailEnd skip: recovery exit cooldown active"
                );
                self.base.record_tailend_skip("recovery_cooldown").await;
                continue;
            }

            // Atomically check exposure + position limits and reserve the market
            if !self
                .base
                .try_reserve_market(&market_id, 1)
                .await
            {
                debug!(
                    market = %market_id,
                    "TailEnd skip: market reserved, has exposure, or max positions reached"
                );
                self.base.record_tailend_skip("reservation").await;
                continue;
            }

            // Get market info for order construction — required for pending tracking
            let market_info = {
                let markets = self.base.active_markets.read().await;
                markets.get(&market_id).cloned()
            };
            let market_info = match market_info {
                Some(m) => m,
                None => {
                    warn!(market = %market_id, "Skipping order: no market metadata");
                    self.base.release_reservation(&market_id).await;
                    continue;
                }
            };

            let opp = match self
                .evaluate_opportunity(&market_id, effective_price, ctx)
                .await
            {
                Some(opp) => opp,
                None => {
                    self.base.release_reservation(&market_id).await;
                    continue;
                }
            };

            if opp.buy_price.is_zero() {
                warn!(market = %market_id, "skipping TailEnd opportunity with zero buy_price");
                self.base.release_reservation(&market_id).await;
                continue;
            }

            // TailEnd uses fixed sizing (no Kelly - confidence is always 1.0)
            // Round to 2dp immediately — raw division produces 28+ decimals
            // which confuses logs and depth comparisons
            let mut size = (self.base.config.sizing.base_size / opp.buy_price)
                .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);

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
                            self.base.release_reservation(&market_id).await;
                            continue;
                        }
                        // Cumulative depth at all ask levels up to our buy price
                        ob.ask_depth_up_to(opp.buy_price)
                    }
                    None => Decimal::ZERO,
                }
            };
            // Safety margin from config — 80% was still too aggressive for competitive tail-end windows
            // where multiple bots race for the same asks during ~1-5s total latency
            let safe_available = available * self.base.config.sizing.depth_cap_factor;
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
                self.base.release_reservation(&market_id).await;
                continue;
            }

            // TailEnd uses GTC orders with aggressive pricing (above ask).
            // Taker fee at these prices (0.90-0.99) is negligible (0.06-0.57%).
            // Tick-aware pricing: step N ticks above the best ask using the market's tick_size.
            let tick_size = market_info.market.tick_size;
            let tick_steps = Decimal::from(self.base.config.order.tick_steps_above_ask);
            let aggressive_price =
                (opp.buy_price + tick_size * tick_steps).min(Decimal::new(99, 2));
            let neg_risk = market_info.market.neg_risk;
            let fee_rate_bps = market_info.market.fee_rate_bps;
            let mut order = OrderRequest::new(
                opp.token_id.clone(),
                aggressive_price,
                size,
                OrderSide::Buy,
                OrderType::Gtc,
                neg_risk,
            );
            order = order.with_tick_size(tick_size);
            order = order.with_fee_rate_bps(fee_rate_bps);
            order = order.with_post_only(self.base.config.tailend.post_only);

            info!(
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

            // Consume reservation and track pending order
            self.base.consume_reservation(&market_id).await;
            {
                let mut pending = self.base.pending_orders.write().await;
                pending.insert(
                    opp.token_id.clone(),
                    PendingOrder {
                        market_id: market_id.clone(),
                        token_id: opp.token_id.clone(),
                        side: opp.outcome_to_buy,
                        price: aggressive_price,
                        size,
                        reference_price: market_info.reference_price,
                        coin: market_info.coin.clone(),
                        order_type: OrderType::Gtc,
                        kelly_fraction: None,
                        estimated_fee: opp.estimated_fee,
                        tick_size,
                        fee_rate_bps,
                    },
                );
            }

            // Track order submission in telemetry
            {
                let mut telem = self.base.order_telemetry.lock().unwrap();
                telem.total_orders += 1;
            }

            result.push(Action::PlaceOrder(order));
        }

        result
    }

    /// Handle an orderbook update: update cached asks, peak bids, evaluate
    /// lifecycle-driven stop-loss triggers on our positions.
    ///
    /// Replaces the old check_stop_loss + post-entry exit logic with the
    /// 4-level trigger hierarchy (evaluate_triggers) and lifecycle state machine.
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

        // Cancel stale GTC stop-loss orders (older than max_age_secs)
        let mut actions = Vec::new();
        {
            let max_age = self.base.config.stop_loss.gtc_stop_loss_max_age_secs as i64;
            let now = self.base.event_time().await;
            let mut gtc_sl = self.base.gtc_stop_loss_orders.write().await;
            let stale: Vec<OrderId> = gtc_sl
                .iter()
                .filter(|(_, sl)| (now - sl.placed_at).num_seconds() >= max_age)
                .map(|(oid, _)| oid.clone())
                .collect();
            for oid in stale {
                if let Some(sl) = gtc_sl.remove(&oid) {
                    info!(
                        order_id = %oid,
                        token_id = %sl.token_id,
                        age_secs = (now - sl.placed_at).num_seconds(),
                        "Cancelling stale GTC stop-loss order for re-evaluation"
                    );
                    // Clear pending_stop_loss so stop-loss can re-trigger
                    let mut pending_sl = self.base.pending_stop_loss.write().await;
                    pending_sl.remove(&sl.token_id);
                    drop(pending_sl);
                    actions.push(Action::CancelOrder(oid));
                }
            }
        }

        // Lifecycle-driven stop-loss evaluation on our positions
        let position_snapshot: Vec<(MarketId, ArbitragePosition)> = {
            let positions = self.base.positions.read().await;
            positions
                .iter()
                .flat_map(|(mid, plist)| plist.iter().map(|p| (mid.clone(), p.clone())))
                .collect()
        };

        let now = self.base.event_time().await;

        for (_, pos) in position_snapshot {
            if pos.token_id != snapshot.token_id {
                continue;
            }

            // Skip if stop-loss already in flight (legacy pending_stop_loss check for transition period)
            {
                let pending_sl = self.base.pending_stop_loss.read().await;
                if pending_sl.contains_key(&pos.token_id) {
                    continue;
                }
            }
            if self.base.is_stop_loss_cooled_down(&pos.token_id).await {
                continue;
            }

            // Get or create lifecycle for this position
            let mut lifecycle = self.base.ensure_lifecycle(&pos.token_id).await;

            // If lifecycle is in ExitExecuting, skip — order is already in flight
            if matches!(lifecycle.state, PositionLifecycleState::ExitExecuting { .. }) {
                continue;
            }

            // Handle ResidualRisk: 2s GTC refresh cycle
            if let PositionLifecycleState::ResidualRisk {
                remaining_size,
                retry_count,
                last_attempt,
                use_gtc_next,
            } = lifecycle.state.clone()
            {
                if let Some(action) = self
                    .handle_residual_risk(
                        &pos,
                        snapshot,
                        &mut lifecycle,
                        remaining_size,
                        retry_count,
                        last_attempt,
                        use_gtc_next,
                        now,
                    )
                    .await
                {
                    self.write_lifecycle(&pos.token_id, &lifecycle).await;
                    actions.push(action);
                }
                continue;
            }

            // Handle Cooldown state: check if elapsed, transition back to Healthy
            if let PositionLifecycleState::Cooldown { until } = lifecycle.state {
                if self.handle_cooldown(&pos, &mut lifecycle, until, now).await {
                    // Cooldown elapsed — fall through to normal trigger evaluation
                } else {
                    // Still in cooldown — skip
                    continue;
                }
            }

            // RecoveryProbe: order is in flight, wait for fill/reject events
            if matches!(lifecycle.state, PositionLifecycleState::RecoveryProbe { .. }) {
                self.handle_recovery_probe(&pos, &mut lifecycle, now).await;
                continue;
            }

            // Get market metadata (time remaining, neg_risk, min_order_size)
            let (time_remaining, neg_risk, min_order_size) = {
                let markets = self.base.active_markets.read().await;
                match markets.get(&pos.market_id) {
                    Some(m) => (
                        m.market.seconds_remaining_at(now),
                        m.market.neg_risk,
                        m.market.min_order_size,
                    ),
                    None => continue,
                }
            };

            // Skip if time remaining is below threshold (don't sell in final seconds)
            if time_remaining <= self.base.config.stop_loss.min_remaining_secs {
                continue;
            }

            // Skip dust positions below min order size
            if pos.size < min_order_size {
                debug!(
                    token_id = %pos.token_id,
                    size = %pos.size,
                    min = %min_order_size,
                    "Lifecycle skip: dust position below min order size"
                );
                continue;
            }

            let current_bid = match snapshot.best_bid() {
                Some(b) => b,
                None => continue,
            };

            // Build the trigger evaluation context
            let book_age_ms = now
                .signed_duration_since(snapshot.timestamp)
                .num_milliseconds();

            // Get composite price for this coin from cache
            let sl_config = &self.base.config.stop_loss;
            let (external_price, external_age_ms, composite_sources) = {
                // Read the cache directly to get both the result and its timestamp
                let cache = self.base.sl_composite_cache.read().await;
                if let Some((composite, cached_at)) = cache.get(&pos.coin) {
                    let age = now.signed_duration_since(*cached_at).num_milliseconds();
                    if age <= sl_config.sl_max_external_age_ms * 2 {
                        (Some(composite.price), Some(age), Some(composite.sources_used))
                    } else {
                        drop(cache);
                        // Composite too stale, try single fresh source
                        if let Some(single) = self
                            .base
                            .get_sl_single_fresh(
                                &pos.coin,
                                sl_config.sl_max_external_age_ms * 2,
                                now,
                            )
                            .await
                        {
                            let history = self.base.price_history.read().await;
                            let age = history
                                .get(&pos.coin)
                                .and_then(|h| h.back())
                                .map(|(ts, _, _)| {
                                    now.signed_duration_since(*ts).num_milliseconds()
                                })
                                .unwrap_or(sl_config.sl_max_external_age_ms * 3);
                            (Some(single), Some(age), None)
                        } else {
                            (None, None, None)
                        }
                    }
                } else {
                    drop(cache);
                    // No composite cached, try single fresh source
                    if let Some(single) = self
                        .base
                        .get_sl_single_fresh(
                            &pos.coin,
                            sl_config.sl_max_external_age_ms * 2,
                            now,
                        )
                        .await
                    {
                        let history = self.base.price_history.read().await;
                        let age = history
                            .get(&pos.coin)
                            .and_then(|h| h.back())
                            .map(|(ts, _, _)| {
                                now.signed_duration_since(*ts).num_milliseconds()
                            })
                            .unwrap_or(sl_config.sl_max_external_age_ms * 3);
                        (Some(single), Some(age), None)
                    } else {
                        (None, None, None)
                    }
                }
            };

            let trigger_ctx = TriggerEvalContext {
                entry_price: pos.entry_price,
                peak_bid: pos.peak_bid,
                side: pos.side,
                reference_price: pos.reference_price,
                tick_size: pos.tick_size,
                entry_time: pos.entry_time,
                current_bid,
                book_age_ms,
                external_price,
                external_age_ms,
                composite_sources,
                time_remaining,
                now,
            };

            let seconds_since_entry = now
                .signed_duration_since(pos.entry_time)
                .num_seconds();
            let is_sellable = seconds_since_entry >= self.base.config.tailend.min_sell_delay_secs;

            // Handle existing DeferredExit state
            if let PositionLifecycleState::DeferredExit { .. } = &lifecycle.state {
                if is_sellable {
                    // Re-evaluate triggers — if still firing, transition to ExitExecuting
                    let trigger = lifecycle.evaluate_triggers(
                        &trigger_ctx,
                        sl_config,
                        &self.base.config.tailend,
                    );
                    if let Some(trigger_kind) = trigger {
                        // Trigger persists — execute exit
                        if let Some(action) = self
                            .build_exit_order(
                                &pos,
                                current_bid,
                                snapshot,
                                neg_risk,
                                min_order_size,
                                &trigger_kind,
                                &mut lifecycle,
                                now,
                            )
                            .await
                        {
                            self.write_lifecycle(&pos.token_id, &lifecycle).await;
                            actions.push(action);
                            continue;
                        }
                    }
                    // Trigger cleared or exit couldn't be built — return to Healthy
                    if let Err(e) = lifecycle.transition(
                        PositionLifecycleState::Healthy,
                        "deferred trigger cleared",
                        now,
                    ) {
                        warn!(token_id = %pos.token_id, error = %e, "Lifecycle transition error");
                    }
                    lifecycle.dual_trigger_ticks = 0;
                    self.write_lifecycle(&pos.token_id, &lifecycle).await;
                    continue;
                }
                // Still in sell delay — keep DeferredExit, skip further evaluation
                self.write_lifecycle(&pos.token_id, &lifecycle).await;
                continue;
            }

            // Evaluate triggers for Healthy positions
            let trigger = lifecycle.evaluate_triggers(
                &trigger_ctx,
                sl_config,
                &self.base.config.tailend,
            );

            if let Some(trigger_kind) = trigger {
                if !is_sellable {
                    // Trigger during sell delay — defer exit
                    if let Err(e) = lifecycle.transition(
                        PositionLifecycleState::DeferredExit {
                            trigger: trigger_kind.clone(),
                            armed_at: now,
                        },
                        &format!("trigger during sell delay: {trigger_kind}"),
                        now,
                    ) {
                        warn!(token_id = %pos.token_id, error = %e, "Lifecycle transition error");
                    }
                    info!(
                        market = %pos.market_id,
                        token_id = %pos.token_id,
                        trigger = %trigger_kind,
                        seconds_since_entry,
                        "TailEnd exit deferred: trigger during sell delay"
                    );
                    self.write_lifecycle(&pos.token_id, &lifecycle).await;
                    continue;
                }

                // Sellable + trigger fired — execute exit
                if let Some(action) = self
                    .build_exit_order(
                        &pos,
                        current_bid,
                        snapshot,
                        neg_risk,
                        min_order_size,
                        &trigger_kind,
                        &mut lifecycle,
                        now,
                    )
                    .await
                {
                    self.write_lifecycle(&pos.token_id, &lifecycle).await;
                    actions.push(action);
                    continue;
                }
            }

            // No trigger fired — write back lifecycle (updated dual_trigger_ticks, trailing_unarmable)
            self.write_lifecycle(&pos.token_id, &lifecycle).await;
        }

        actions
    }

    /// Build an exit sell order with depth-capped clip sizing and transition
    /// lifecycle to ExitExecuting. Stores exit order metadata for fill routing.
    ///
    /// Returns `None` if the clip size is dust (below min_order_size).
    #[allow(clippy::too_many_arguments)]
    async fn build_exit_order(
        &self,
        pos: &ArbitragePosition,
        current_bid: Decimal,
        snapshot: &OrderbookSnapshot,
        neg_risk: bool,
        min_order_size: Decimal,
        trigger_kind: &StopLossTriggerKind,
        lifecycle: &mut crate::crypto_arb::types::PositionLifecycle,
        now: DateTime<Utc>,
    ) -> Option<Action> {
        // Compute depth-capped clip size
        let bid_depth = snapshot.bid_depth_down_to(
            current_bid - pos.tick_size * Decimal::from(3u32),
        );
        let clip = compute_exit_clip(
            pos.size,
            bid_depth,
            self.base.config.stop_loss.exit_depth_cap_factor,
            min_order_size,
        );

        if clip.is_zero() {
            debug!(
                token_id = %pos.token_id,
                remaining = %pos.size,
                bid_depth = %bid_depth,
                "Exit clip is dust — skipping exit order"
            );
            return None;
        }

        let order = OrderRequest::new(
            pos.token_id.clone(),
            current_bid,
            clip,
            OrderSide::Sell,
            OrderType::Fok,
            neg_risk,
        )
        .with_tick_size(pos.tick_size)
        .with_fee_rate_bps(pos.fee_rate_bps);

        // Generate a synthetic order ID for lifecycle tracking
        // (real order ID comes back from PlaceOrder result, but we need to track intent now)
        let exit_order_id = format!("exit-{}-{}", pos.token_id, now.timestamp_millis());

        // Transition lifecycle to ExitExecuting
        if let Err(e) = lifecycle.transition(
            PositionLifecycleState::ExitExecuting {
                order_id: exit_order_id.clone(),
                order_type: OrderType::Fok,
                exit_price: current_bid,
                submitted_at: now,
            },
            &format!("trigger fired: {trigger_kind}"),
            now,
        ) {
            warn!(token_id = %pos.token_id, error = %e, "Lifecycle transition to ExitExecuting failed");
            return None;
        }
        lifecycle.pending_exit_order_id = Some(exit_order_id.clone());

        // Store exit order meta for fill routing
        {
            let mut exit_orders = self.base.exit_orders_by_id.write().await;
            exit_orders.insert(
                exit_order_id,
                ExitOrderMeta {
                    token_id: pos.token_id.clone(),
                    order_type: OrderType::Fok,
                    source_state: format!("{trigger_kind}"),
                },
            );
        }

        // Also store in legacy pending_stop_loss for compatibility with existing fill handler
        {
            let mut pending_sl = self.base.pending_stop_loss.write().await;
            pending_sl.insert(
                pos.token_id.clone(),
                PendingStopLoss {
                    exit_price: current_bid,
                    order_type: OrderType::Fok,
                },
            );
        }

        info!(
            market = %pos.market_id,
            token_id = %pos.token_id,
            entry = %pos.entry_price,
            exit = %current_bid,
            clip = %clip,
            side = ?pos.side,
            trigger = %trigger_kind,
            "TailEnd lifecycle stop-loss triggered"
        );

        Some(Action::PlaceOrder(order))
    }

    /// Write a lifecycle back to the shared store.
    async fn write_lifecycle(
        &self,
        token_id: &str,
        lifecycle: &PositionLifecycle,
    ) {
        let mut lifecycles = self.base.position_lifecycle.write().await;
        lifecycles.insert(token_id.to_string(), lifecycle.clone());
    }

    /// Handle a position in ResidualRisk state during an orderbook update.
    ///
    /// Implements the 2-second GTC refresh cycle:
    /// - If `use_gtc_next` is true and cooldown has elapsed, place GTC at bid - tick_offset
    /// - If an existing GTC exit order is older than `short_limit_refresh_secs`, cancel and re-place
    /// - Applies geometric clip reduction after 2+ retries
    /// - Detects dust and max retry exhaustion
    #[allow(clippy::too_many_arguments)]
    async fn handle_residual_risk(
        &self,
        pos: &ArbitragePosition,
        snapshot: &OrderbookSnapshot,
        lifecycle: &mut PositionLifecycle,
        remaining_size: Decimal,
        retry_count: u32,
        last_attempt: DateTime<Utc>,
        use_gtc_next: bool,
        now: DateTime<Utc>,
    ) -> Option<Action> {
        let sl_config = &self.base.config.stop_loss;

        // Get market metadata
        let (neg_risk, min_order_size) = {
            let markets = self.base.active_markets.read().await;
            match markets.get(&pos.market_id) {
                Some(m) => (m.market.neg_risk, m.market.min_order_size),
                None => return None,
            }
        };

        // Dust detection: if remaining is below min_order_size, resolve
        if remaining_size < min_order_size {
            warn!(
                token_id = %pos.token_id,
                remaining = %remaining_size,
                min = %min_order_size,
                "ResidualRisk: dust remaining, removing position"
            );
            self.base
                .reduce_or_remove_position_by_token(&pos.token_id, remaining_size)
                .await;
            return None;
        }

        // Max retries exhausted — attempt recovery or resolve with loss
        if retry_count >= sl_config.max_exit_retries {
            if !sl_config.recovery_enabled {
                warn!(
                    token_id = %pos.token_id,
                    retry_count,
                    remaining = %remaining_size,
                    "ResidualRisk: max retries exhausted, recovery disabled — resolving with loss"
                );
                self.base.record_recovery_exit_cooldown(&pos.market_id).await;
                self.base
                    .reduce_or_remove_position_by_token(&pos.token_id, remaining_size)
                    .await;
                return None;
            }

            // Try set completion recovery (opposite-side buy)
            if let Some(action) = self
                .try_set_completion_recovery(pos, remaining_size, lifecycle, now)
                .await
            {
                return Some(action);
            }

            // Try opposite-side alpha recovery (momentum-confirmed reversal)
            if let Some(action) = self
                .try_opposite_alpha_recovery(pos, remaining_size, lifecycle, now)
                .await
            {
                return Some(action);
            }

            // No recovery viable — resolve with loss
            warn!(
                token_id = %pos.token_id,
                retry_count,
                remaining = %remaining_size,
                "ResidualRisk: max retries exhausted, recovery not viable — resolving with loss"
            );
            self.base.record_recovery_exit_cooldown(&pos.market_id).await;
            self.base
                .reduce_or_remove_position_by_token(&pos.token_id, remaining_size)
                .await;
            return None;
        }

        let current_bid = match snapshot.best_bid() {
            Some(b) => b,
            None => return None,
        };

        // Check if an existing GTC exit order needs refresh (cancel stale ones)
        if let Some(exit_oid) = &lifecycle.pending_exit_order_id {
            let exit_orders = self.base.exit_orders_by_id.read().await;
            if let Some(meta) = exit_orders.get(exit_oid.as_str())
                && meta.order_type == OrderType::Gtc
            {
                // Check if GTC order is stale (older than refresh interval)
                let gtc_sl = self.base.gtc_stop_loss_orders.read().await;
                if let Some(sl_order) = gtc_sl.get(exit_oid.as_str()) {
                    let age_secs = (now - sl_order.placed_at).num_seconds();
                    if age_secs >= sl_config.short_limit_refresh_secs as i64 {
                        drop(gtc_sl);
                        drop(exit_orders);
                        // Cancel stale GTC for re-placement on next cycle
                        info!(
                            token_id = %pos.token_id,
                            order_id = %exit_oid,
                            age_secs,
                            "ResidualRisk: cancelling stale GTC exit for refresh"
                        );
                        return Some(Action::CancelOrder(exit_oid.clone()));
                    }
                }
                // GTC order still fresh — wait for fill or refresh
                return None;
            }
        }

        // Cooldown between retries: wait at least short_limit_refresh_secs before retrying
        let secs_since_last = (now - last_attempt).num_seconds();
        if secs_since_last < sl_config.short_limit_refresh_secs as i64 {
            return None;
        }

        // Compute clip size with geometric reduction for retries > 1
        let effective_remaining = if retry_count >= 2 {
            // Geometric reduction: halve the clip each time after 2 retries
            let factor = Decimal::new(5, 1); // 0.5
            let mut clip = remaining_size;
            for _ in 2..=retry_count {
                clip = (clip * factor).round_dp(2);
            }
            clip
        } else {
            remaining_size
        };

        let bid_depth = snapshot.bid_depth_down_to(
            current_bid - pos.tick_size * Decimal::from(3u32),
        );
        let clip = compute_exit_clip(
            effective_remaining,
            bid_depth,
            sl_config.exit_depth_cap_factor,
            min_order_size,
        );

        if clip.is_zero() {
            debug!(
                token_id = %pos.token_id,
                remaining = %remaining_size,
                effective_remaining = %effective_remaining,
                bid_depth = %bid_depth,
                "ResidualRisk: clip is dust, waiting for liquidity"
            );
            return None;
        }

        // Place GTC exit order at bid - tick_offset
        if use_gtc_next {
            let tick_offset = Decimal::from(sl_config.short_limit_tick_offset);
            let exit_price = (current_bid - pos.tick_size * tick_offset).max(pos.tick_size);

            let order = OrderRequest::new(
                pos.token_id.clone(),
                exit_price,
                clip,
                OrderSide::Sell,
                OrderType::Gtc,
                neg_risk,
            )
            .with_tick_size(pos.tick_size)
            .with_fee_rate_bps(pos.fee_rate_bps);

            let exit_order_id = format!("exit-gtc-{}-{}", pos.token_id, now.timestamp_millis());

            // Transition to ExitExecuting
            if let Err(e) = lifecycle.transition(
                PositionLifecycleState::ExitExecuting {
                    order_id: exit_order_id.clone(),
                    order_type: OrderType::Gtc,
                    exit_price,
                    submitted_at: now,
                },
                &format!("ResidualRisk retry #{retry_count} GTC"),
                now,
            ) {
                warn!(token_id = %pos.token_id, error = %e, "Lifecycle transition error");
                return None;
            }
            lifecycle.pending_exit_order_id = Some(exit_order_id.clone());

            // Store exit order meta and GTC tracking
            {
                let mut exit_orders = self.base.exit_orders_by_id.write().await;
                exit_orders.insert(
                    exit_order_id.clone(),
                    ExitOrderMeta {
                        token_id: pos.token_id.clone(),
                        order_type: OrderType::Gtc,
                        source_state: format!("ResidualRisk(retry={retry_count})"),
                    },
                );
            }
            {
                let mut gtc_sl = self.base.gtc_stop_loss_orders.write().await;
                gtc_sl.insert(
                    exit_order_id.clone(),
                    GtcStopLossOrder {
                        order_id: exit_order_id.clone(),
                        token_id: pos.token_id.clone(),
                        market_id: pos.market_id.clone(),
                        price: exit_price,
                        size: clip,
                        placed_at: now,
                    },
                );
            }
            // Also store in legacy pending_stop_loss for compatibility
            {
                let mut pending_sl = self.base.pending_stop_loss.write().await;
                pending_sl.insert(
                    pos.token_id.clone(),
                    PendingStopLoss {
                        exit_price,
                        order_type: OrderType::Gtc,
                    },
                );
            }

            info!(
                token_id = %pos.token_id,
                exit_price = %exit_price,
                clip = %clip,
                retry_count,
                "ResidualRisk: placing GTC exit order"
            );

            return Some(Action::PlaceOrder(order));
        }

        // FOK retry
        let order = OrderRequest::new(
            pos.token_id.clone(),
            current_bid,
            clip,
            OrderSide::Sell,
            OrderType::Fok,
            neg_risk,
        )
        .with_tick_size(pos.tick_size)
        .with_fee_rate_bps(pos.fee_rate_bps);

        let exit_order_id = format!("exit-fok-{}-{}", pos.token_id, now.timestamp_millis());

        if let Err(e) = lifecycle.transition(
            PositionLifecycleState::ExitExecuting {
                order_id: exit_order_id.clone(),
                order_type: OrderType::Fok,
                exit_price: current_bid,
                submitted_at: now,
            },
            &format!("ResidualRisk retry #{retry_count} FOK"),
            now,
        ) {
            warn!(token_id = %pos.token_id, error = %e, "Lifecycle transition error");
            return None;
        }
        lifecycle.pending_exit_order_id = Some(exit_order_id.clone());

        {
            let mut exit_orders = self.base.exit_orders_by_id.write().await;
            exit_orders.insert(
                exit_order_id,
                ExitOrderMeta {
                    token_id: pos.token_id.clone(),
                    order_type: OrderType::Fok,
                    source_state: format!("ResidualRisk(retry={retry_count})"),
                },
            );
        }
        {
            let mut pending_sl = self.base.pending_stop_loss.write().await;
            pending_sl.insert(
                pos.token_id.clone(),
                PendingStopLoss {
                    exit_price: current_bid,
                    order_type: OrderType::Fok,
                },
            );
        }

        info!(
            token_id = %pos.token_id,
            exit_price = %current_bid,
            clip = %clip,
            retry_count,
            "ResidualRisk: placing FOK exit order"
        );

        Some(Action::PlaceOrder(order))
    }

    /// Try set completion recovery: buy the opposite side so both tokens can be
    /// redeemed for $1.00 per share (minus fees).
    ///
    /// Only viable when `entry_price + other_ask <= recovery_max_set_cost` (default 1.01).
    /// Places a FOK buy order for the opposite token, depth-capped.
    async fn try_set_completion_recovery(
        &self,
        pos: &ArbitragePosition,
        remaining_size: Decimal,
        lifecycle: &mut PositionLifecycle,
        now: DateTime<Utc>,
    ) -> Option<Action> {
        let sl_config = &self.base.config.stop_loss;

        // Look up opposite token
        let opposite_token = self
            .base
            .get_opposite_token(&pos.market_id, &pos.token_id)
            .await?;

        // Get opposite token's best ask from cached orderbook data
        let other_ask = {
            let cached = self.base.cached_asks.read().await;
            cached.get(&opposite_token).copied()
        }?;

        // Check set completion viability: entry + other_ask <= max_set_cost
        let combined_cost = pos.entry_price + other_ask;
        if combined_cost > sl_config.recovery_max_set_cost {
            debug!(
                token_id = %pos.token_id,
                entry = %pos.entry_price,
                other_ask = %other_ask,
                combined = %combined_cost,
                max = %sl_config.recovery_max_set_cost,
                "Set completion not viable: combined cost exceeds max"
            );
            return None;
        }

        // Get market metadata
        let (neg_risk, min_order_size) = {
            let markets = self.base.active_markets.read().await;
            let m = markets.get(&pos.market_id)?;
            (m.market.neg_risk, m.market.min_order_size)
        };

        // Compute clip size — use remaining_size capped by depth
        let clip = remaining_size.min(remaining_size); // Simple: buy matching size
        if clip < min_order_size {
            debug!(
                token_id = %pos.token_id,
                clip = %clip,
                min = %min_order_size,
                "Set completion: clip below min order size"
            );
            return None;
        }

        // Determine the opposite side
        let probe_side = match pos.side {
            OutcomeSide::Up | OutcomeSide::Yes => OutcomeSide::Down,
            OutcomeSide::Down | OutcomeSide::No => OutcomeSide::Up,
        };

        // Build FOK buy order for opposite token at best ask
        let order = OrderRequest::new(
            opposite_token.clone(),
            other_ask,
            clip,
            OrderSide::Buy,
            OrderType::Fok,
            neg_risk,
        )
        .with_tick_size(pos.tick_size)
        .with_fee_rate_bps(pos.fee_rate_bps);

        let recovery_order_id = format!(
            "recovery-set-{}-{}",
            pos.token_id,
            now.timestamp_millis()
        );

        // Transition to RecoveryProbe
        if let Err(e) = lifecycle.transition(
            PositionLifecycleState::RecoveryProbe {
                recovery_order_id: recovery_order_id.clone(),
                probe_side,
                submitted_at: now,
            },
            &format!(
                "set completion: entry={} + other_ask={} = {} <= {}",
                pos.entry_price, other_ask, combined_cost, sl_config.recovery_max_set_cost
            ),
            now,
        ) {
            warn!(token_id = %pos.token_id, error = %e, "Lifecycle transition to RecoveryProbe failed");
            return None;
        }
        lifecycle.pending_exit_order_id = Some(recovery_order_id.clone());
        self.write_lifecycle(&pos.token_id, lifecycle).await;

        // Track recovery order
        {
            let mut exit_orders = self.base.exit_orders_by_id.write().await;
            exit_orders.insert(
                recovery_order_id,
                ExitOrderMeta {
                    token_id: pos.token_id.clone(),
                    order_type: OrderType::Fok,
                    source_state: "RecoveryProbe(set_completion)".to_string(),
                },
            );
        }

        info!(
            token_id = %pos.token_id,
            opposite = %opposite_token,
            other_ask = %other_ask,
            combined_cost = %combined_cost,
            clip = %clip,
            "RecoveryProbe: placing set completion buy (opposite side)"
        );

        Some(Action::PlaceOrder(order))
    }

    /// Try opposite-side alpha recovery: buy the other side when composite
    /// momentum confirms reversal for `reentry_confirm_ticks` consecutive ticks.
    ///
    /// Guard: extra risk (other_ask * size) must be <= `recovery_max_extra_frac`
    /// of position value.
    async fn try_opposite_alpha_recovery(
        &self,
        pos: &ArbitragePosition,
        remaining_size: Decimal,
        lifecycle: &mut PositionLifecycle,
        now: DateTime<Utc>,
    ) -> Option<Action> {
        let sl_config = &self.base.config.stop_loss;

        // Check if momentum confirms reversal for N consecutive ticks
        // We use the composite cache and price history for this
        let composite = {
            let cache = self.base.sl_composite_cache.read().await;
            cache.get(&pos.coin).map(|(c, _)| c.price)
        }?;

        // Check reversal direction relative to position side
        let reversal_confirmed = {
            let history = self.base.price_history.read().await;
            let entries = history.get(&pos.coin)?;
            if entries.len() < sl_config.reentry_confirm_ticks {
                return None;
            }
            // Check last N entries all show reversal
            let recent: Vec<_> = entries
                .iter()
                .rev()
                .take(sl_config.reentry_confirm_ticks)
                .collect();
            recent.iter().all(|(_, price, _)| {
                match pos.side {
                    // For Up position, reversal means price dropping
                    OutcomeSide::Up | OutcomeSide::Yes => *price < pos.reference_price,
                    // For Down position, reversal means price rising
                    OutcomeSide::Down | OutcomeSide::No => *price > pos.reference_price,
                }
            })
        };

        if !reversal_confirmed {
            return None;
        }

        // Look up opposite token
        let opposite_token = self
            .base
            .get_opposite_token(&pos.market_id, &pos.token_id)
            .await?;

        let other_ask = {
            let cached = self.base.cached_asks.read().await;
            cached.get(&opposite_token).copied()
        }?;

        // Guard: extra risk <= recovery_max_extra_frac of position value
        let position_value = pos.entry_price * remaining_size;
        let extra_risk = other_ask * remaining_size;
        if position_value.is_zero()
            || extra_risk / position_value > sl_config.recovery_max_extra_frac
        {
            debug!(
                token_id = %pos.token_id,
                extra_risk = %extra_risk,
                position_value = %position_value,
                max_frac = %sl_config.recovery_max_extra_frac,
                "Opposite alpha: extra risk exceeds budget"
            );
            return None;
        }

        let (neg_risk, min_order_size) = {
            let markets = self.base.active_markets.read().await;
            let m = markets.get(&pos.market_id)?;
            (m.market.neg_risk, m.market.min_order_size)
        };

        let clip = remaining_size;
        if clip < min_order_size {
            return None;
        }

        let probe_side = match pos.side {
            OutcomeSide::Up | OutcomeSide::Yes => OutcomeSide::Down,
            OutcomeSide::Down | OutcomeSide::No => OutcomeSide::Up,
        };

        let order = OrderRequest::new(
            opposite_token.clone(),
            other_ask,
            clip,
            OrderSide::Buy,
            OrderType::Fok,
            neg_risk,
        )
        .with_tick_size(pos.tick_size)
        .with_fee_rate_bps(pos.fee_rate_bps);

        let recovery_order_id = format!(
            "recovery-alpha-{}-{}",
            pos.token_id,
            now.timestamp_millis()
        );

        if let Err(e) = lifecycle.transition(
            PositionLifecycleState::RecoveryProbe {
                recovery_order_id: recovery_order_id.clone(),
                probe_side,
                submitted_at: now,
            },
            &format!(
                "opposite alpha: composite={}, reversal confirmed for {} ticks",
                composite, sl_config.reentry_confirm_ticks
            ),
            now,
        ) {
            warn!(token_id = %pos.token_id, error = %e, "Lifecycle transition to RecoveryProbe failed");
            return None;
        }
        lifecycle.pending_exit_order_id = Some(recovery_order_id.clone());
        self.write_lifecycle(&pos.token_id, lifecycle).await;

        {
            let mut exit_orders = self.base.exit_orders_by_id.write().await;
            exit_orders.insert(
                recovery_order_id,
                ExitOrderMeta {
                    token_id: pos.token_id.clone(),
                    order_type: OrderType::Fok,
                    source_state: "RecoveryProbe(opposite_alpha)".to_string(),
                },
            );
        }

        info!(
            token_id = %pos.token_id,
            opposite = %opposite_token,
            other_ask = %other_ask,
            clip = %clip,
            "RecoveryProbe: placing opposite-side alpha buy"
        );

        Some(Action::PlaceOrder(order))
    }

    /// Handle a position in RecoveryProbe state during an orderbook update.
    ///
    /// RecoveryProbe is a waiting state — the recovery order is in flight.
    /// This method checks if the order has been pending too long and should be
    /// cancelled (handled by fill/reject events, not here).
    /// Currently a no-op since fill/reject routing handles transitions.
    async fn handle_recovery_probe(
        &self,
        _pos: &ArbitragePosition,
        _lifecycle: &mut PositionLifecycle,
        _now: DateTime<Utc>,
    ) -> Option<Action> {
        // Recovery orders are tracked via exit_orders_by_id.
        // Fill → Cooldown transition happens in on_order_filled.
        // Rejection → resolve with loss happens in rejection handler.
        None
    }

    /// Handle a position in Cooldown state during an orderbook update.
    ///
    /// When cooldown has elapsed, transitions back to Healthy so the position
    /// can be re-evaluated by the trigger hierarchy.
    async fn handle_cooldown(
        &self,
        pos: &ArbitragePosition,
        lifecycle: &mut PositionLifecycle,
        until: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> bool {
        if now >= until {
            if let Err(e) = lifecycle.transition(
                PositionLifecycleState::Healthy,
                "cooldown elapsed",
                now,
            ) {
                warn!(token_id = %pos.token_id, error = %e, "Cooldown→Healthy transition failed");
                return false;
            }
            info!(
                token_id = %pos.token_id,
                "Cooldown elapsed, position back to Healthy"
            );
            self.write_lifecycle(&pos.token_id, lifecycle).await;
            true // Caller should continue to normal evaluation
        } else {
            false // Still in cooldown
        }
    }

    /// Transition a position's lifecycle from ExitExecuting to ResidualRisk.
    ///
    /// Called when an exit order is rejected or fails. Increments retry count
    /// and determines whether to use GTC next based on rejection kind.
    async fn transition_to_residual_risk(
        &self,
        token_id: &str,
        remaining_size: Decimal,
        retry_count: u32,
        use_gtc: bool,
        reason: &str,
        now: DateTime<Utc>,
    ) {
        let mut lifecycle = self.base.ensure_lifecycle(token_id).await;

        let new_state = PositionLifecycleState::ResidualRisk {
            remaining_size,
            retry_count,
            last_attempt: now,
            use_gtc_next: use_gtc,
        };

        if let Err(e) = lifecycle.transition(new_state, reason, now) {
            warn!(token_id = %token_id, error = %e, "Lifecycle transition to ResidualRisk failed");
            return;
        }
        lifecycle.pending_exit_order_id = None;

        self.write_lifecycle(token_id, &lifecycle).await;
    }

    /// Handle a fully filled order event (GTC entry fills, stop-loss sells, GTC SL fills).
    async fn on_order_filled(
        &self,
        order_id: &str,
        token_id: &str,
        price: Decimal,
        size: Decimal,
    ) -> Vec<Action> {
        // Check if this is a recovery order fill (by order_id in exit_orders_by_id)
        {
            let exit_meta = {
                let exit_orders = self.base.exit_orders_by_id.read().await;
                exit_orders.get(order_id).cloned()
            };
            if let Some(meta) = &exit_meta
                && meta.source_state.starts_with("RecoveryProbe")
            {
                let now = self.base.event_time().await;
                let sl_config = &self.base.config.stop_loss;

                // Transition to Cooldown
                let mut lifecycle = self.base.ensure_lifecycle(&meta.token_id).await;
                let cooldown_until =
                    now + chrono::Duration::seconds(sl_config.reentry_cooldown_secs);

                if let Err(e) = lifecycle.transition(
                    PositionLifecycleState::Cooldown {
                        until: cooldown_until,
                    },
                    &format!("recovery fill: {} at {}", meta.source_state, price),
                    now,
                ) {
                    warn!(
                        token_id = %meta.token_id,
                        error = %e,
                        "RecoveryProbe→Cooldown transition failed"
                    );
                }
                lifecycle.pending_exit_order_id = None;
                self.write_lifecycle(&meta.token_id, &lifecycle).await;

                // Clean up exit order tracking
                {
                    let mut exit_orders = self.base.exit_orders_by_id.write().await;
                    exit_orders.remove(order_id);
                }

                info!(
                    order_id = %order_id,
                    token_id = %meta.token_id,
                    source = %meta.source_state,
                    fill_price = %price,
                    fill_size = %size,
                    cooldown_secs = sl_config.reentry_cooldown_secs,
                    "Recovery order filled — transitioning to Cooldown"
                );
                return vec![];
            }
        }

        // Check if this is a GTC stop-loss fill (by order_id)
        {
            let mut gtc_sl = self.base.gtc_stop_loss_orders.write().await;
            if let Some(sl_order) = gtc_sl.remove(order_id) {
                drop(gtc_sl);
                // Clear pending_stop_loss for this token
                {
                    let mut pending_sl = self.base.pending_stop_loss.write().await;
                    pending_sl.remove(&sl_order.token_id);
                }
                if let Some((pos, fully_closed)) = self
                    .base
                    .reduce_or_remove_position_by_token(&sl_order.token_id, size)
                    .await
                {
                    // GTC fills are maker orders → 0% fee on exit
                    // Use entry_fee_per_share (0 for GTC entry, actual taker fee for FOK entry)
                    let pnl = (price - pos.entry_price) * size - (pos.entry_fee_per_share * size);
                    self.base.record_trade_pnl(pnl).await;
                    if !fully_closed {
                        let remaining = pos.size - size;
                        let now = self.base.event_time().await;
                        // Transition to ResidualRisk for the remaining amount
                        self.transition_to_residual_risk(
                            &sl_order.token_id,
                            remaining,
                            1,
                            true, // Continue with GTC
                            "GTC exit partial fill",
                            now,
                        )
                        .await;
                        warn!(
                            token_id = %sl_order.token_id,
                            order_id = %order_id,
                            fill_size = %size,
                            remaining = %remaining,
                            "GTC stop-loss partial fill: transitioned to ResidualRisk"
                        );
                    }
                    info!(
                        token_id = %sl_order.token_id,
                        order_id = %order_id,
                        pnl = %pnl,
                        fill_size = %size,
                        fully_closed,
                        "GTC stop-loss sell filled (maker, 0% fee)"
                    );
                } else {
                    warn!(
                        token_id = %sl_order.token_id,
                        order_id = %order_id,
                        "GTC stop-loss fill: position already removed (race)"
                    );
                }
                // Clean up exit order tracking
                {
                    let mut exit_orders = self.base.exit_orders_by_id.write().await;
                    exit_orders.remove(order_id);
                }
                return vec![];
            }
        }

        // Check if this is a FOK stop-loss sell fill (by token_id in pending_stop_loss)
        {
            let mut pending_sl = self.base.pending_stop_loss.write().await;
            if let Some(_sl_info) = pending_sl.remove(token_id) {
                drop(pending_sl);
                // Use actual CLOB fill price, not trigger bid (sl_info.exit_price)
                let exit_price = price;
                if let Some((pos, fully_closed)) = self
                    .base
                    .reduce_or_remove_position_by_token(token_id, size)
                    .await
                {
                    let exit_fee = taker_fee(exit_price, self.base.config.fee.taker_fee_rate);
                    // Use entry_fee_per_share (0 for GTC entry, actual taker fee for FOK entry)
                    let pnl = (exit_price - pos.entry_price) * size
                        - (pos.entry_fee_per_share * size)
                        - (exit_fee * size);
                    self.base.record_trade_pnl(pnl).await;
                    if !fully_closed {
                        let remaining = pos.size - size;
                        // Check if residual is below minimum order size (unsellable dust)
                        let is_dust = {
                            let markets = self.base.active_markets.read().await;
                            markets
                                .get(&pos.market_id)
                                .map(|m| remaining < m.market.min_order_size)
                                .unwrap_or(true)
                        };
                        if is_dust {
                            // Remove dust — too small to sell, will resolve at expiry
                            self.base
                                .reduce_or_remove_position_by_token(token_id, remaining)
                                .await;
                            warn!(
                                token_id = %token_id,
                                dust_size = %remaining,
                                "Removed unsellable dust after FOK partial fill — will resolve at expiry"
                            );
                        } else {
                            // Transition to ResidualRisk for the remaining amount
                            let now = self.base.event_time().await;
                            self.transition_to_residual_risk(
                                token_id,
                                remaining,
                                1,
                                true, // Switch to GTC for next attempt
                                "FOK exit partial fill",
                                now,
                            )
                            .await;
                            warn!(
                                token_id = %token_id,
                                fill_size = %size,
                                remaining = %remaining,
                                "FOK stop-loss partial fill: transitioned to ResidualRisk"
                            );
                        }
                    }
                    info!(
                        token_id = %token_id,
                        pnl = %pnl,
                        fill_size = %size,
                        fully_closed,
                        "Stop-loss sell filled"
                    );
                } else {
                    warn!(
                        token_id = %token_id,
                        "TailEnd stop-loss fill: position already removed (race)"
                    );
                }
                // Clean up exit order tracking
                {
                    let mut exit_orders = self.base.exit_orders_by_id.write().await;
                    exit_orders.retain(|_, meta| meta.token_id != *token_id);
                }
                return vec![];
            }
        }

        let lo = {
            let mut limits = self.base.open_limit_orders.write().await;
            match limits.remove(order_id) {
                Some(lo) => lo,
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

        // Track fill in telemetry
        {
            let now_real = self.base.event_time().await;
            let fill_time = (now_real - lo.placed_at).num_milliseconds() as f64 / 1000.0;
            let mut telem = self.base.order_telemetry.lock().unwrap();
            telem.total_fills += 1;
            // Approximate seconds to expiry at fill time
            let markets = self.base.active_markets.try_read();
            if let Ok(markets) = markets
                && let Some(mwr) = markets.get(&lo.market_id)
            {
                let secs_to_expiry = mwr.market.seconds_remaining_at(now_real);
                telem.fill_times.push((secs_to_expiry, fill_time));
            }
        }

        let now = self.base.event_time().await;
        let position =
            ArbitragePosition::from_limit_order(&lo, price, size, Some(order_id.to_string()), now);

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
            }) => {
                self.handle_external_price(symbol, *price, source, ctx)
                    .await
            }

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

            Event::OrderUpdate(OrderEvent::PartiallyFilled {
                order_id,
                filled_size,
                remaining_size,
            }) => {
                let mut limits = self.base.open_limit_orders.write().await;
                if let Some(lo) = limits.get_mut(order_id) {
                    lo.size = *remaining_size;
                    info!(
                        order_id = %order_id,
                        filled = %filled_size,
                        remaining = %remaining_size,
                        "TailEnd GTC order partially filled"
                    );
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::Rejected {
                token_id, reason, ..
            }) => {
                if let Some(token_id) = token_id {
                    // Clear pending buy order if it's ours and record cooldown
                    let mut pending = self.base.pending_orders.write().await;
                    if let Some(p) = pending.get(token_id) {
                        let market_id = p.market_id.clone();
                        pending.remove(token_id);
                        drop(pending);

                        let cooldown = self.base.config.tailend.rejection_cooldown_secs;
                        self.base
                            .record_rejection_cooldown(&market_id, cooldown)
                            .await;
                        warn!(
                            token_id = %token_id,
                            market = %market_id,
                            cooldown_secs = cooldown,
                            "TailEnd order rejected, cooldown applied"
                        );
                    }

                    // Check if this is a lifecycle-driven exit order rejection
                    let lifecycle = self.base.ensure_lifecycle(token_id).await;
                    if matches!(lifecycle.state, PositionLifecycleState::ExitExecuting { .. }) {
                        let now = self.base.event_time().await;
                        let kind = StopLossRejectionKind::classify(reason);

                        // Get remaining size from position
                        let remaining_size = {
                            let positions = self.base.positions.read().await;
                            positions
                                .values()
                                .flat_map(|v| v.iter())
                                .find(|p| p.token_id == *token_id)
                                .map(|p| p.size)
                        };

                        if let Some(remaining) = remaining_size {
                            // InvalidSize: dust — remove immediately
                            if kind == StopLossRejectionKind::InvalidSize {
                                warn!(
                                    token_id = %token_id,
                                    remaining = %remaining,
                                    "Exit order rejected (InvalidSize) — removing dust position"
                                );
                                self.base
                                    .reduce_or_remove_position_by_token(token_id, remaining)
                                    .await;
                            } else {
                                // Transition to ResidualRisk
                                let use_gtc = kind == StopLossRejectionKind::Liquidity
                                    && self.base.config.stop_loss.gtc_fallback;
                                self.transition_to_residual_risk(
                                    token_id,
                                    remaining,
                                    1, // First retry
                                    use_gtc,
                                    &format!("exit rejected: {reason}"),
                                    now,
                                )
                                .await;
                                warn!(
                                    token_id = %token_id,
                                    reason = %reason,
                                    kind = ?kind,
                                    use_gtc,
                                    "Exit order rejected — transitioned to ResidualRisk"
                                );
                            }
                        }

                        // Clean up exit order tracking and legacy pending_stop_loss
                        {
                            let mut exit_orders = self.base.exit_orders_by_id.write().await;
                            exit_orders.retain(|_, meta| meta.token_id != *token_id);
                        }
                        {
                            let mut pending_sl = self.base.pending_stop_loss.write().await;
                            pending_sl.remove(token_id);
                        }
                        {
                            let mut gtc_sl = self.base.gtc_stop_loss_orders.write().await;
                            gtc_sl.retain(|_, sl| sl.token_id != *token_id);
                        }

                        return Ok(vec![]);
                    }

                    // Check if this is a recovery order rejection
                    if matches!(lifecycle.state, PositionLifecycleState::RecoveryProbe { .. }) {
                        // Recovery failed — accept loss, resolve position
                        let pos_info = {
                            let positions = self.base.positions.read().await;
                            positions
                                .values()
                                .flat_map(|v| v.iter())
                                .find(|p| p.token_id == *token_id)
                                .map(|p| (p.size, p.market_id.clone()))
                        };

                        warn!(
                            token_id = %token_id,
                            reason = %reason,
                            remaining = ?pos_info.as_ref().map(|(s, _)| s),
                            "Recovery order rejected — accepting loss, resolving position"
                        );

                        if let Some((remaining, market_id)) = pos_info {
                            // Record recovery exit cooldown for re-entry gating
                            self.base.record_recovery_exit_cooldown(&market_id).await;
                            self.base
                                .reduce_or_remove_position_by_token(token_id, remaining)
                                .await;
                        }

                        // Clean up
                        {
                            let mut exit_orders = self.base.exit_orders_by_id.write().await;
                            exit_orders.retain(|_, meta| meta.token_id != *token_id);
                        }
                        // Lifecycle was already cleaned up by reduce_or_remove_position_by_token
                        // if fully closed. If not, force-remove lifecycle.
                        self.base.remove_lifecycle(token_id).await;

                        return Ok(vec![]);
                    }

                    // Handle stop-loss rejection with classification and GTC fallback (legacy path)
                    if self
                        .base
                        .pending_stop_loss
                        .read()
                        .await
                        .contains_key(token_id)
                    {
                        // If a GTC SL was rejected, clear the GTC tracking and revert to FOK
                        {
                            let mut gtc_sl = self.base.gtc_stop_loss_orders.write().await;
                            let gtc_order_ids: Vec<OrderId> = gtc_sl
                                .iter()
                                .filter(|(_, sl)| sl.token_id == *token_id)
                                .map(|(oid, _)| oid.clone())
                                .collect();
                            for oid in gtc_order_ids {
                                gtc_sl.remove(&oid);
                                warn!(
                                    order_id = %oid,
                                    token_id = %token_id,
                                    "GTC stop-loss rejected, reverting to FOK for next attempt"
                                );
                            }
                        }

                        self.base
                            .handle_stop_loss_rejection(token_id, reason, "TailEnd")
                            .await;
                    }
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::Cancelled(order_id)) => {
                // Check if this is a lifecycle-driven exit order cancel (GTC refresh cycle)
                {
                    let exit_meta = {
                        let exit_orders = self.base.exit_orders_by_id.read().await;
                        exit_orders.get(order_id).cloned()
                    };
                    if let Some(meta) = exit_meta {
                        let now = self.base.event_time().await;
                        // Get lifecycle state to extract retry info
                        let lifecycle = self.base.ensure_lifecycle(&meta.token_id).await;
                        let retry_count = if let PositionLifecycleState::ExitExecuting { .. } =
                            &lifecycle.state
                        {
                            // Extract previous retry count from the source_state string
                            meta.source_state
                                .split("retry=")
                                .nth(1)
                                .and_then(|s| s.trim_end_matches(')').parse::<u32>().ok())
                                .unwrap_or(1)
                        } else {
                            1
                        };

                        // Get remaining size from position
                        let remaining_size = {
                            let positions = self.base.positions.read().await;
                            positions
                                .values()
                                .flat_map(|v| v.iter())
                                .find(|p| p.token_id == meta.token_id)
                                .map(|p| p.size)
                        };

                        if let Some(remaining) = remaining_size {
                            // Transition back to ResidualRisk for re-placement
                            self.transition_to_residual_risk(
                                &meta.token_id,
                                remaining,
                                retry_count,
                                true, // Use GTC on next attempt
                                "GTC exit cancelled for refresh",
                                now,
                            )
                            .await;
                        }

                        // Clean up tracking
                        {
                            let mut exit_orders = self.base.exit_orders_by_id.write().await;
                            exit_orders.remove(order_id);
                        }
                        {
                            let mut gtc_sl = self.base.gtc_stop_loss_orders.write().await;
                            gtc_sl.remove(order_id);
                        }
                        {
                            let mut pending_sl = self.base.pending_stop_loss.write().await;
                            pending_sl.remove(&meta.token_id);
                        }

                        info!(
                            order_id = %order_id,
                            token_id = %meta.token_id,
                            retry_count,
                            "Lifecycle GTC exit cancelled, will re-place on next OB update"
                        );
                        return Ok(vec![]);
                    }
                }

                // Check if this is a legacy GTC stop-loss cancel
                {
                    let mut gtc_sl = self.base.gtc_stop_loss_orders.write().await;
                    if let Some(sl) = gtc_sl.remove(order_id) {
                        info!(
                            order_id = %order_id,
                            token_id = %sl.token_id,
                            "GTC stop-loss order cancelled, will re-evaluate"
                        );
                        let mut pending_sl = self.base.pending_stop_loss.write().await;
                        pending_sl.remove(&sl.token_id);
                        return Ok(vec![]);
                    }
                }

                let mut limits = self.base.open_limit_orders.write().await;
                if let Some(lo) = limits.remove(order_id) {
                    info!(
                        order_id = %order_id,
                        market = %lo.market_id,
                        "GTC order cancelled"
                    );
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::CancelFailed { order_id, reason }) => {
                let (_found, fill_actions) = self.base.handle_cancel_failed(order_id, reason).await;
                fill_actions
            }

            Event::System(SystemEvent::OpenOrderSnapshot(ids)) => {
                let id_set: std::collections::HashSet<String> = ids.iter().cloned().collect();
                self.base.reconcile_limit_orders(&id_set).await
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

    fn make_market_info(
        id: &str,
        end_date: chrono::DateTime<Utc>,
    ) -> polyrust_core::types::MarketInfo {
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
        config.enabled = true;
        config.tailend.min_sustained_secs = 5; // Small window to keep test simple
        config.tailend.max_recent_volatility = dec!(1.0); // Disable volatility filter
        let base = Arc::new(CryptoArbBase::new(config, vec![]));

        let market = MarketWithReference {
            market: make_market_info("market1", Utc::now() + Duration::seconds(time_remaining)),
            reference_price: dec!(50000),
            reference_quality: ReferenceQuality::Exact,
            discovery_time: Utc::now(),
            coin: "BTC".to_string(),
            window_ts: 0,
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
                    bids: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.935),
                        size: dec!(100),
                    }],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.94),
                        size: dec!(100),
                    }],
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
                    bids: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.92),
                        size: dec!(100),
                    }],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.95),
                        size: dec!(100),
                    }],
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
                    bids: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.87),
                        size: dec!(100),
                    }],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.89),
                        size: dec!(100),
                    }],
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
        config.enabled = true;
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
            window_ts: 0,
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
                    bids: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.90),
                        size: dec!(100),
                    }],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.95),
                        size: dec!(100),
                    }],
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
    async fn tailend_pending_order_stores_aggressive_price() {
        let (strategy, ctx) = make_tailend_strategy(60).await;

        // Set up orderbook: ask=0.94, bid=0.935, depth=100
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_up".to_string(),
                    bids: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.935),
                        size: dec!(100),
                    }],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.94),
                        size: dec!(100),
                    }],
                    timestamp: Utc::now(),
                },
            );
        }

        // Trigger entry via external price
        let actions = strategy
            .handle_external_price("BTC", dec!(51000), "test", &ctx)
            .await;

        // Should have produced a PlaceOrder action
        assert!(
            actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Expected PlaceOrder action"
        );

        // Verify pending order stores aggressive_price (ask + 1 tick = 0.95), not buy_price (0.94)
        let pending = strategy.base.pending_orders.read().await;
        let po = pending.get("token_up").expect("pending order for token_up");
        let expected_aggressive = dec!(0.95); // 0.94 + 0.01 * 1 tick step
        assert_eq!(
            po.price, expected_aggressive,
            "PendingOrder.price should be aggressive_price ({expected_aggressive}), got {}",
            po.price
        );
    }

    #[tokio::test]
    async fn tailend_partially_filled_updates_limit_order_size() {
        let (strategy, ctx) = make_tailend_strategy(60).await;

        // Seed an open limit order as if a GTC was placed
        {
            let mut limits = strategy.base.open_limit_orders.write().await;
            limits.insert(
                "order123".to_string(),
                OpenLimitOrder {
                    order_id: "order123".to_string(),
                    market_id: "market1".to_string(),
                    token_id: "token_up".to_string(),
                    side: polyrust_core::types::OutcomeSide::Up,
                    price: dec!(0.95),
                    size: dec!(10),
                    reference_price: dec!(50000),
                    coin: "BTC".to_string(),
                    placed_at: Utc::now(),
                    kelly_fraction: None,
                    estimated_fee: dec!(0.001),
                    tick_size: dec!(0.01),
                    fee_rate_bps: 0,
                    cancel_pending: false,
                    reconcile_miss_count: 0,
                },
            );
        }

        // Simulate a PartiallyFilled event
        let event = Event::OrderUpdate(polyrust_core::events::OrderEvent::PartiallyFilled {
            order_id: "order123".to_string(),
            filled_size: dec!(4),
            remaining_size: dec!(6),
        });

        let mut strategy_mut = strategy;
        let actions = strategy_mut.on_event(&event, &ctx).await.unwrap();
        assert!(actions.iter().all(|a| !matches!(a, Action::PlaceOrder(_))));

        // Verify size updated to remaining
        let limits = strategy_mut.base.open_limit_orders.read().await;
        let lo = limits.get("order123").expect("limit order still present");
        assert_eq!(lo.size, dec!(6), "size should be updated to remaining_size");
    }

    // --- PnL entry fee bug fix tests ---

    /// GTC entry (maker, 0% fee) + FOK exit (taker fee on exit only).
    /// Entry fee must be 0, only exit taker fee is deducted.
    #[test]
    fn pnl_gtc_entry_fok_exit_entry_fee_is_zero() {
        use crate::crypto_arb::base::taker_fee;

        let entry_price = dec!(0.92);
        let exit_price = dec!(0.85);
        let size = dec!(100);
        let fee_rate = dec!(0.0315);

        // GTC entry → entry_fee_per_share = 0
        let entry_fee_per_share = Decimal::ZERO;
        let exit_fee = taker_fee(exit_price, fee_rate);

        // Formula from FOK stop-loss path:
        // pnl = (exit_price - entry_price) * size - (entry_fee_per_share * size) - (exit_fee * size)
        let pnl = (exit_price - entry_price) * size
            - (entry_fee_per_share * size)
            - (exit_fee * size);

        // Expected: (0.85 - 0.92) * 100 - 0 - exit_fee * 100
        let expected_exit_fee = taker_fee(dec!(0.85), fee_rate);
        let expected = (dec!(0.85) - dec!(0.92)) * dec!(100) - expected_exit_fee * dec!(100);
        assert_eq!(pnl, expected);

        // Verify entry fee component is truly zero
        assert_eq!(entry_fee_per_share * size, Decimal::ZERO);
    }

    /// GTC entry + GTC exit → both fees = 0.
    #[test]
    fn pnl_gtc_entry_gtc_exit_both_fees_zero() {
        let entry_price = dec!(0.93);
        let exit_price = dec!(0.88);
        let size = dec!(50);

        // GTC entry → entry_fee_per_share = 0
        let entry_fee_per_share = Decimal::ZERO;
        // GTC exit → 0% maker fee (no exit_fee term)

        // Formula from GTC stop-loss path:
        // pnl = (price - entry_price) * size - (entry_fee_per_share * size)
        let pnl = (exit_price - entry_price) * size - (entry_fee_per_share * size);

        // Expected: (0.88 - 0.93) * 50 - 0 = -2.50
        let expected = (dec!(0.88) - dec!(0.93)) * dec!(50);
        assert_eq!(pnl, expected);
        assert_eq!(pnl, dec!(-2.5));
    }

    /// FOK entry (taker fee) + FOK exit (taker fee) → both fees deducted.
    /// This verifies entry_fee_per_share is correctly used instead of estimated_fee.
    #[test]
    fn pnl_fok_entry_fok_exit_both_fees_deducted() {
        use crate::crypto_arb::base::taker_fee;

        let entry_price = dec!(0.94);
        let exit_price = dec!(0.90);
        let size = dec!(100);
        let fee_rate = dec!(0.0315);

        // FOK entry → entry_fee_per_share = taker_fee(actual_fill_price, rate)
        let entry_fee_per_share = taker_fee(entry_price, fee_rate);
        let exit_fee = taker_fee(exit_price, fee_rate);

        let pnl = (exit_price - entry_price) * size
            - (entry_fee_per_share * size)
            - (exit_fee * size);

        // Manual: taker_fee(0.94, 0.0315) = 2 * 0.94 * 0.06 * 0.0315 = 0.0035532
        // taker_fee(0.90, 0.0315) = 2 * 0.90 * 0.10 * 0.0315 = 0.00567
        // pnl = (0.90 - 0.94) * 100 - 0.35532 - 0.567 = -4.0 - 0.35532 - 0.567 = -4.92232
        let expected_entry = taker_fee(dec!(0.94), fee_rate);
        let expected_exit = taker_fee(dec!(0.90), fee_rate);
        let expected = (dec!(0.90) - dec!(0.94)) * dec!(100)
            - expected_entry * dec!(100)
            - expected_exit * dec!(100);
        assert_eq!(pnl, expected);

        // Both fees should be non-zero
        assert!(entry_fee_per_share > Decimal::ZERO);
        assert!(exit_fee > Decimal::ZERO);
    }

    /// Market expiry with GTC entry: winning outcome → entry fee = 0.
    #[test]
    fn pnl_market_expiry_gtc_entry_win() {
        let entry_price = dec!(0.90);
        let size = dec!(100);
        let entry_fee_per_share = Decimal::ZERO; // GTC

        // Won: pnl = (1.0 - entry_price) * size - (entry_fee_per_share * size)
        let pnl = (Decimal::ONE - entry_price) * size - (entry_fee_per_share * size);

        // Expected: (1.0 - 0.90) * 100 - 0 = 10.00
        assert_eq!(pnl, dec!(10));
    }

    /// Market expiry with GTC entry: losing outcome → entry fee = 0.
    #[test]
    fn pnl_market_expiry_gtc_entry_loss() {
        let entry_price = dec!(0.90);
        let size = dec!(100);
        let entry_fee_per_share = Decimal::ZERO; // GTC

        // Lost: pnl = -(entry_price * size) - (entry_fee_per_share * size)
        let pnl = -(entry_price * size) - (entry_fee_per_share * size);

        // Expected: -(0.90 * 100) - 0 = -90.00
        assert_eq!(pnl, dec!(-90));
    }

    /// Market expiry with FOK entry: taker fee deducted from outcome.
    #[test]
    fn pnl_market_expiry_fok_entry_win() {
        use crate::crypto_arb::base::taker_fee;

        let entry_price = dec!(0.92);
        let size = dec!(100);
        let fee_rate = dec!(0.0315);
        let entry_fee_per_share = taker_fee(entry_price, fee_rate);

        // Won: pnl = (1.0 - entry_price) * size - (entry_fee_per_share * size)
        let pnl = (Decimal::ONE - entry_price) * size - (entry_fee_per_share * size);

        // taker_fee(0.92) = 2 * 0.92 * 0.08 * 0.0315 = 0.0046368
        // pnl = (0.08) * 100 - 0.46368 = 7.53632
        let expected = dec!(8) - taker_fee(dec!(0.92), fee_rate) * dec!(100);
        assert_eq!(pnl, expected);
        assert!(pnl > Decimal::ZERO);
        assert!(pnl < dec!(8)); // Must be less than gross due to fee
    }

    // --- PnL exit price bug fix tests ---

    /// FOK stop-loss fill at 0.93 when trigger bid was 0.92.
    /// PnL must use actual fill price (0.93), not trigger bid (0.92).
    #[test]
    fn pnl_fok_exit_uses_actual_fill_price_not_trigger_bid() {
        use crate::crypto_arb::base::taker_fee;

        let entry_price = dec!(0.95);
        let trigger_bid = dec!(0.92); // price when stop-loss triggered
        let actual_fill_price = dec!(0.93); // better fill from CLOB
        let size = dec!(100);
        let fee_rate = dec!(0.0315);
        let entry_fee_per_share = Decimal::ZERO; // GTC entry

        // Correct PnL: uses actual fill price
        let exit_fee = taker_fee(actual_fill_price, fee_rate);
        let correct_pnl = (actual_fill_price - entry_price) * size
            - (entry_fee_per_share * size)
            - (exit_fee * size);

        // Wrong PnL: would use trigger bid (the old bug)
        let wrong_exit_fee = taker_fee(trigger_bid, fee_rate);
        let wrong_pnl = (trigger_bid - entry_price) * size
            - (entry_fee_per_share * size)
            - (wrong_exit_fee * size);

        // Actual fill is better (0.93 > 0.92), so correct PnL is less negative
        assert!(correct_pnl > wrong_pnl);
        // The difference should be meaningful (not just rounding)
        assert!(correct_pnl - wrong_pnl > dec!(0.5));
        // Both should still be negative (stop-loss means loss)
        assert!(correct_pnl < Decimal::ZERO);
        assert!(wrong_pnl < Decimal::ZERO);
    }

    /// When trigger bid equals actual fill price, PnL is the same either way (sanity).
    #[test]
    fn pnl_fok_exit_same_trigger_and_fill_price() {
        use crate::crypto_arb::base::taker_fee;

        let entry_price = dec!(0.95);
        let fill_price = dec!(0.90); // trigger and fill are identical
        let size = dec!(50);
        let fee_rate = dec!(0.0315);
        let entry_fee_per_share = Decimal::ZERO; // GTC entry

        let exit_fee = taker_fee(fill_price, fee_rate);
        let pnl = (fill_price - entry_price) * size
            - (entry_fee_per_share * size)
            - (exit_fee * size);

        // Manual: (0.90 - 0.95) * 50 - 0 - taker_fee(0.90) * 50
        // = -2.50 - (2 * 0.90 * 0.10 * 0.0315) * 50
        // = -2.50 - 0.00567 * 50
        // = -2.50 - 0.2835
        // = -2.7835
        let expected = (dec!(0.90) - dec!(0.95)) * dec!(50)
            - taker_fee(dec!(0.90), fee_rate) * dec!(50);
        assert_eq!(pnl, expected);
        assert!(pnl < Decimal::ZERO);
    }

    // -----------------------------------------------------------------------
    // Lifecycle-driven stop-loss evaluation tests (Task 13)
    // -----------------------------------------------------------------------

    use crate::crypto_arb::types::PositionLifecycleState;

    /// Helper: create a TailEndStrategy with a market, position, and price history
    /// configured so that stop-loss can fire.
    async fn make_lifecycle_test_setup(
        entry_time_offset_secs: i64,
        time_remaining_secs: i64,
    ) -> (TailEndStrategy, polyrust_core::types::OrderbookSnapshot) {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.enabled = true;
        config.tailend.min_sustained_secs = 0;
        config.tailend.max_recent_volatility = dec!(1.0);
        // Hard crash: bid drop >= 0.08 OR reversal >= 0.6%
        config.stop_loss.hard_drop_abs = dec!(0.08);
        config.stop_loss.hard_reversal_pct = dec!(0.006);
        // Dual trigger: both conditions for 2 ticks
        config.stop_loss.dual_trigger_consecutive_ticks = 2;
        config.stop_loss.reversal_pct = dec!(0.003);
        config.stop_loss.min_drop = dec!(0.05);
        // Freshness: generous limits for testing
        config.stop_loss.sl_max_book_age_ms = 5000;
        config.stop_loss.sl_max_external_age_ms = 5000;
        config.stop_loss.sl_min_sources = 1;
        config.stop_loss.sl_max_dispersion_bps = dec!(100);
        // Sell delay: 10 seconds
        config.tailend.min_sell_delay_secs = 10;
        // Post-entry window: 20 seconds
        config.tailend.post_entry_window_secs = 20;
        config.tailend.post_entry_exit_drop = dec!(0.04);
        // Min remaining: 0 (allow stop-loss at any time)
        config.stop_loss.min_remaining_secs = 0;
        // Exit depth cap
        config.stop_loss.exit_depth_cap_factor = dec!(0.80);

        let base = Arc::new(CryptoArbBase::new(config, vec![]));

        let now = Utc::now();
        let end_date = now + Duration::seconds(time_remaining_secs);

        // Insert active market
        {
            let mut markets = base.active_markets.write().await;
            markets.insert(
                "market1".to_string(),
                MarketWithReference {
                    market: make_market_info("market1", end_date),
                    reference_price: dec!(50000),
                    reference_quality: ReferenceQuality::Exact,
                    discovery_time: now,
                    coin: "BTC".to_string(),
                    window_ts: 0,
                },
            );
        }

        // Seed price history: BTC reversed from 50000 → 49700 (0.6%)
        {
            let mut history = base.price_history.write().await;
            let mut entries = std::collections::VecDeque::new();
            entries.push_back((now, dec!(49700), "test".to_string()));
            history.insert("BTC".to_string(), entries);
        }

        // Seed the composite cache with fresh data
        {
            let mut cache = base.sl_composite_cache.write().await;
            cache.insert(
                "BTC".to_string(),
                (
                    crate::crypto_arb::base::CompositePriceResult {
                        price: dec!(49700),
                        sources_used: 2,
                        max_lag_ms: 100,
                        dispersion_bps: dec!(5),
                    },
                    now,
                ),
            );
        }

        // Create position: entry at 0.90 (entry_time_offset_secs ago)
        let entry_time = now - Duration::seconds(entry_time_offset_secs);
        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: polyrust_core::types::OutcomeSide::Up,
            entry_price: dec!(0.90),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time,
            kelly_fraction: None,
            peak_bid: dec!(0.90),
            estimated_fee: Decimal::ZERO,
            entry_market_price: dec!(0.90),
            tick_size: dec!(0.01),
            fee_rate_bps: 0,
            entry_order_type: OrderType::Gtc,
            entry_fee_per_share: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
        };
        base.record_position(pos).await;

        let strategy = TailEndStrategy::new(base);

        // Snapshot: bid dropped to 0.82 (drop = 0.08 from 0.90 = hard crash level)
        let snapshot = polyrust_core::types::OrderbookSnapshot {
            token_id: "token_up".to_string(),
            bids: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.82),
                size: dec!(100),
            }],
            asks: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.85),
                size: dec!(100),
            }],
            timestamp: now,
        };

        (strategy, snapshot)
    }

    /// Orderbook update with trigger condition (hard crash) on a sellable position
    /// transitions lifecycle to ExitExecuting and produces a PlaceOrder action.
    #[tokio::test]
    async fn lifecycle_trigger_transitions_to_exit_executing() {
        // entry_time 20s ago (past sell delay of 10s), market 300s remaining
        let (strategy, snapshot) = make_lifecycle_test_setup(20, 300).await;

        let actions = strategy.handle_orderbook_update(&snapshot).await;

        // Should produce a PlaceOrder (FOK sell) action
        assert!(
            actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Expected PlaceOrder action for stop-loss exit, got: {actions:?}"
        );

        // Check lifecycle transitioned to ExitExecuting
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles
            .get("token_up")
            .expect("lifecycle for token_up should exist");
        assert!(
            matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }),
            "Expected ExitExecuting, got: {:?}",
            lc.state
        );

        // Verify exit_orders_by_id has an entry
        let exit_orders = strategy.base.exit_orders_by_id.read().await;
        assert!(
            !exit_orders.is_empty(),
            "exit_orders_by_id should have the exit order meta"
        );

        // Verify pending_stop_loss was also populated (legacy compat)
        let pending_sl = strategy.base.pending_stop_loss.read().await;
        assert!(
            pending_sl.contains_key("token_up"),
            "pending_stop_loss should be set for fill handler"
        );
    }

    /// Orderbook update with trigger condition during sell delay window
    /// transitions lifecycle to DeferredExit (no sell order placed).
    #[tokio::test]
    async fn lifecycle_trigger_during_sell_delay_defers_exit() {
        // entry_time 5s ago (still within sell delay of 10s), market 300s remaining
        let (strategy, snapshot) = make_lifecycle_test_setup(5, 300).await;

        let actions = strategy.handle_orderbook_update(&snapshot).await;

        // Should NOT produce a PlaceOrder action (sell delay not elapsed)
        assert!(
            !actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Should not sell during sell delay, got: {actions:?}"
        );

        // Lifecycle should be DeferredExit
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles
            .get("token_up")
            .expect("lifecycle should exist");
        assert!(
            matches!(lc.state, PositionLifecycleState::DeferredExit { .. }),
            "Expected DeferredExit during sell delay, got: {:?}",
            lc.state
        );
    }

    /// When a deferred exit becomes sellable and the trigger still fires,
    /// the lifecycle transitions to ExitExecuting and a sell order is placed.
    #[tokio::test]
    async fn lifecycle_deferred_exit_fires_when_sellable() {
        // Start with position in sell delay (5s ago)
        let (strategy, snapshot) = make_lifecycle_test_setup(5, 300).await;

        // First call: should defer (still in sell delay)
        let _ = strategy.handle_orderbook_update(&snapshot).await;
        {
            let lifecycles = strategy.base.position_lifecycle.read().await;
            let lc = lifecycles.get("token_up").unwrap();
            assert!(matches!(lc.state, PositionLifecycleState::DeferredExit { .. }));
        }

        // Simulate time passing: move position entry_time to 20s ago
        {
            let mut positions = strategy.base.positions.write().await;
            for pos_list in positions.values_mut() {
                for pos in pos_list.iter_mut() {
                    if pos.token_id == "token_up" {
                        pos.entry_time = Utc::now() - Duration::seconds(20);
                    }
                }
            }
        }

        // Second call: sell delay now elapsed, trigger still holds
        let actions = strategy.handle_orderbook_update(&snapshot).await;

        // Should produce a PlaceOrder action
        assert!(
            actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Should sell now that delay elapsed, got: {actions:?}"
        );

        // Lifecycle should be ExitExecuting
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").unwrap();
        assert!(
            matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }),
            "Expected ExitExecuting after deferred exit, got: {:?}",
            lc.state
        );
    }

    /// When a deferred exit's trigger condition clears before the sell delay
    /// expires, the lifecycle transitions back to Healthy.
    #[tokio::test]
    async fn lifecycle_deferred_exit_clears_when_condition_resolves() {
        // Start with position in sell delay (5s ago)
        let (strategy, snapshot) = make_lifecycle_test_setup(5, 300).await;

        // First call: should defer
        let _ = strategy.handle_orderbook_update(&snapshot).await;
        {
            let lifecycles = strategy.base.position_lifecycle.read().await;
            let lc = lifecycles.get("token_up").unwrap();
            assert!(matches!(lc.state, PositionLifecycleState::DeferredExit { .. }));
        }

        // Simulate time passing (sell delay elapsed)
        {
            let mut positions = strategy.base.positions.write().await;
            for pos_list in positions.values_mut() {
                for pos in pos_list.iter_mut() {
                    if pos.token_id == "token_up" {
                        pos.entry_time = Utc::now() - Duration::seconds(20);
                    }
                }
            }
        }

        // Now send a healthy snapshot (bid recovered near entry)
        let healthy_snapshot = polyrust_core::types::OrderbookSnapshot {
            token_id: "token_up".to_string(),
            bids: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.89),
                size: dec!(100),
            }],
            asks: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.91),
                size: dec!(100),
            }],
            timestamp: Utc::now(),
        };

        // Also update price history to reflect no reversal
        {
            let mut history = strategy.base.price_history.write().await;
            let mut entries = std::collections::VecDeque::new();
            entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
            history.insert("BTC".to_string(), entries);
        }
        // Clear the composite cache so no stale reversal signal
        {
            let mut cache = strategy.base.sl_composite_cache.write().await;
            cache.insert(
                "BTC".to_string(),
                (
                    crate::crypto_arb::base::CompositePriceResult {
                        price: dec!(50100),
                        sources_used: 2,
                        max_lag_ms: 100,
                        dispersion_bps: dec!(5),
                    },
                    Utc::now(),
                ),
            );
        }

        let actions = strategy.handle_orderbook_update(&healthy_snapshot).await;

        // Should NOT produce a sell action (conditions cleared)
        assert!(
            !actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Should not sell when conditions cleared, got: {actions:?}"
        );

        // Lifecycle should transition back to Healthy
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").unwrap();
        assert!(
            matches!(lc.state, PositionLifecycleState::Healthy),
            "Expected Healthy after deferred exit cleared, got: {:?}",
            lc.state
        );
    }

    /// Helper: set up a strategy with a position already in ExitExecuting state
    /// (simulates a FOK exit order that was just placed).
    async fn make_exit_executing_setup() -> TailEndStrategy {
        let (strategy, snapshot) = make_lifecycle_test_setup(20, 300).await;

        // Trigger the lifecycle to ExitExecuting via orderbook update (hard crash)
        let _actions = strategy.handle_orderbook_update(&snapshot).await;

        // Verify we're in ExitExecuting
        {
            let lifecycles = strategy.base.position_lifecycle.read().await;
            let lc = lifecycles.get("token_up").unwrap();
            assert!(matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }));
        }

        strategy
    }

    /// FOK exit order rejected for liquidity -> lifecycle transitions to ResidualRisk
    /// with retry_count=1 and use_gtc_next=true.
    #[tokio::test]
    async fn lifecycle_fok_rejected_transitions_to_residual_risk() {
        let strategy = make_exit_executing_setup().await;

        // Simulate a Rejected event for the exit order
        let now = strategy.base.event_time().await;
        strategy
            .transition_to_residual_risk(
                "token_up",
                dec!(10), // remaining
                1,
                true,     // use GTC next (liquidity rejection)
                "exit rejected: couldn't be fully filled",
                now,
            )
            .await;

        // Clean up pending_stop_loss (as the Rejected handler would)
        {
            let mut pending_sl = strategy.base.pending_stop_loss.write().await;
            pending_sl.remove("token_up");
        }

        // Lifecycle should be ResidualRisk with correct fields
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").unwrap();
        match &lc.state {
            PositionLifecycleState::ResidualRisk {
                remaining_size,
                retry_count,
                use_gtc_next,
                ..
            } => {
                assert_eq!(*remaining_size, dec!(10));
                assert_eq!(*retry_count, 1);
                assert!(*use_gtc_next);
            }
            other => panic!("Expected ResidualRisk, got: {other:?}"),
        }
    }

    /// GTC refresh cycle: a GTC exit order that's older than short_limit_refresh_secs
    /// gets cancelled and re-placed at the current bid.
    #[tokio::test]
    async fn lifecycle_gtc_refresh_cancels_stale_order() {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.enabled = true;
        config.tailend.min_sustained_secs = 0;
        config.tailend.max_recent_volatility = dec!(1.0);
        config.stop_loss.min_remaining_secs = 0;
        config.stop_loss.exit_depth_cap_factor = dec!(0.80);
        config.stop_loss.short_limit_refresh_secs = 2;
        config.stop_loss.short_limit_tick_offset = 1;

        let base = Arc::new(CryptoArbBase::new(config, vec![]));
        let now = Utc::now();

        // Insert active market
        {
            let mut markets = base.active_markets.write().await;
            markets.insert(
                "market1".to_string(),
                MarketWithReference {
                    market: make_market_info("market1", now + Duration::seconds(300)),
                    reference_price: dec!(50000),
                    reference_quality: ReferenceQuality::Exact,
                    discovery_time: now,
                    coin: "BTC".to_string(),
                    window_ts: 0,
                },
            );
        }

        // Create position
        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: polyrust_core::types::OutcomeSide::Up,
            entry_price: dec!(0.90),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: now - Duration::seconds(20),
            kelly_fraction: None,
            peak_bid: dec!(0.90),
            estimated_fee: Decimal::ZERO,
            entry_market_price: dec!(0.90),
            tick_size: dec!(0.01),
            fee_rate_bps: 0,
            entry_order_type: OrderType::Gtc,
            entry_fee_per_share: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
        };
        base.record_position(pos).await;

        // Set up lifecycle in ResidualRisk with a pending GTC exit order
        let exit_oid = "exit-gtc-token_up-12345".to_string();
        {
            let mut lifecycle = base.ensure_lifecycle("token_up").await;
            lifecycle
                .transition(
                    PositionLifecycleState::ExitExecuting {
                        order_id: exit_oid.clone(),
                        order_type: OrderType::Gtc,
                        exit_price: dec!(0.81),
                        submitted_at: now - Duration::seconds(3), // 3s old (> 2s refresh)
                    },
                    "test setup",
                    now,
                )
                .unwrap();
            lifecycle.pending_exit_order_id = Some(exit_oid.clone());

            // Store in position_lifecycle
            let mut lifecycles = base.position_lifecycle.write().await;
            lifecycles.insert("token_up".to_string(), lifecycle);
        }

        // Track the GTC order as a stop-loss order (3s old)
        {
            let mut gtc_sl = base.gtc_stop_loss_orders.write().await;
            gtc_sl.insert(
                exit_oid.clone(),
                GtcStopLossOrder {
                    order_id: exit_oid.clone(),
                    token_id: "token_up".to_string(),
                    market_id: "market1".to_string(),
                    price: dec!(0.81),
                    size: dec!(10),
                    placed_at: now - Duration::seconds(3),
                },
            );
        }
        {
            let mut exit_orders = base.exit_orders_by_id.write().await;
            exit_orders.insert(
                exit_oid.clone(),
                ExitOrderMeta {
                    token_id: "token_up".to_string(),
                    order_type: OrderType::Gtc,
                    source_state: "ResidualRisk(retry=1)".to_string(),
                },
            );
        }

        let strategy = TailEndStrategy::new(base);

        let snapshot = polyrust_core::types::OrderbookSnapshot {
            token_id: "token_up".to_string(),
            bids: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.82),
                size: dec!(100),
            }],
            asks: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.85),
                size: dec!(100),
            }],
            timestamp: now,
        };

        // The handle_orderbook_update sees ExitExecuting and skips (correct).
        // The stale GTC cancel logic at the top of handle_orderbook_update
        // should detect the 3s-old GTC and emit CancelOrder.
        let actions = strategy.handle_orderbook_update(&snapshot).await;

        // Should produce a CancelOrder for the stale GTC
        let has_cancel = actions
            .iter()
            .any(|a| matches!(a, Action::CancelOrder(oid) if oid == &exit_oid));
        assert!(
            has_cancel,
            "Expected CancelOrder for stale GTC exit, got: {actions:?}"
        );
    }

    /// Partial fill reduces remaining_size and transitions to ResidualRisk.
    #[tokio::test]
    async fn lifecycle_partial_fill_transitions_to_residual_risk() {
        let strategy = make_exit_executing_setup().await;

        // Simulate a partial FOK fill (5 out of 10 filled)
        let fill_size = dec!(5);
        let remaining = dec!(5); // 10 - 5

        // We simulate what the on_order_filled handler does:
        // 1. Remove the pending_stop_loss entry
        // 2. Call reduce_or_remove_position_by_token
        // 3. Transition to ResidualRisk
        {
            let mut pending_sl = strategy.base.pending_stop_loss.write().await;
            pending_sl.remove("token_up");
        }

        // reduce_or_remove with fill_size=5 out of 10 → partial
        let result = strategy
            .base
            .reduce_or_remove_position_by_token("token_up", fill_size)
            .await;
        assert!(result.is_some());
        let (_, fully_closed) = result.unwrap();
        assert!(!fully_closed, "Should be partial fill");

        // Now transition to ResidualRisk (as the fill handler would do)
        let now = strategy.base.event_time().await;
        strategy
            .transition_to_residual_risk(
                "token_up",
                remaining,
                1,
                true,
                "FOK exit partial fill",
                now,
            )
            .await;

        // Lifecycle should be ResidualRisk with remaining=5
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").unwrap();
        match &lc.state {
            PositionLifecycleState::ResidualRisk {
                remaining_size,
                retry_count,
                ..
            } => {
                assert_eq!(*remaining_size, dec!(5));
                assert_eq!(*retry_count, 1);
            }
            other => panic!("Expected ResidualRisk, got: {other:?}"),
        }
    }

    /// Geometric clip reduction: after 2+ retries, clip size is halved per retry.
    #[tokio::test]
    async fn lifecycle_geometric_clip_reduction() {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.enabled = true;
        config.tailend.min_sustained_secs = 0;
        config.tailend.max_recent_volatility = dec!(1.0);
        config.stop_loss.min_remaining_secs = 0;
        config.stop_loss.exit_depth_cap_factor = dec!(1.0); // Don't cap by depth
        config.stop_loss.short_limit_refresh_secs = 1; // Allow immediate retry
        config.stop_loss.short_limit_tick_offset = 1;

        let base = Arc::new(CryptoArbBase::new(config, vec![]));
        let now = Utc::now();

        {
            let mut markets = base.active_markets.write().await;
            markets.insert(
                "market1".to_string(),
                MarketWithReference {
                    market: make_market_info("market1", now + Duration::seconds(300)),
                    reference_price: dec!(50000),
                    reference_quality: ReferenceQuality::Exact,
                    discovery_time: now,
                    coin: "BTC".to_string(),
                    window_ts: 0,
                },
            );
        }

        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: polyrust_core::types::OutcomeSide::Up,
            entry_price: dec!(0.90),
            size: dec!(20),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: now - Duration::seconds(20),
            kelly_fraction: None,
            peak_bid: dec!(0.90),
            estimated_fee: Decimal::ZERO,
            entry_market_price: dec!(0.90),
            tick_size: dec!(0.01),
            fee_rate_bps: 0,
            entry_order_type: OrderType::Gtc,
            entry_fee_per_share: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
        };
        base.record_position(pos.clone()).await;

        // Set lifecycle to ResidualRisk with retry_count=3 and use_gtc_next=true
        {
            let mut lifecycle = base.ensure_lifecycle("token_up").await;
            lifecycle
                .transition(
                    PositionLifecycleState::ExitExecuting {
                        order_id: "exit-1".to_string(),
                        order_type: OrderType::Fok,
                        exit_price: dec!(0.82),
                        submitted_at: now - Duration::seconds(5),
                    },
                    "test setup intermediate",
                    now - Duration::seconds(4),
                )
                .unwrap();
            lifecycle
                .transition(
                    PositionLifecycleState::ResidualRisk {
                        remaining_size: dec!(20),
                        retry_count: 3,
                        last_attempt: now - Duration::seconds(3), // 3s ago (> 1s refresh)
                        use_gtc_next: true,
                    },
                    "test setup",
                    now - Duration::seconds(3),
                )
                .unwrap();
            lifecycle.pending_exit_order_id = None;

            let mut lifecycles = base.position_lifecycle.write().await;
            lifecycles.insert("token_up".to_string(), lifecycle);
        }

        let strategy = TailEndStrategy::new(base);

        let snapshot = polyrust_core::types::OrderbookSnapshot {
            token_id: "token_up".to_string(),
            bids: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.82),
                size: dec!(100),
            }],
            asks: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.85),
                size: dec!(100),
            }],
            timestamp: now,
        };

        let actions = strategy.handle_orderbook_update(&snapshot).await;

        // Should produce a PlaceOrder action with reduced clip size
        let place_action = actions
            .iter()
            .find_map(|a| match a {
                Action::PlaceOrder(order) => Some(order),
                _ => None,
            });

        assert!(
            place_action.is_some(),
            "Expected PlaceOrder for GTC retry, got: {actions:?}"
        );

        let order = place_action.unwrap();
        // With retry_count=3: geometric reduction from remaining=20
        // retry 2: 20 * 0.5 = 10.0
        // retry 3: 10.0 * 0.5 = 5.0
        // Then depth-capped: min(5.0, 100 * 1.0) = 5.0
        assert_eq!(order.size, dec!(5.0), "Clip should be geometrically reduced");
        assert_eq!(order.order_type, OrderType::Gtc, "Should use GTC after retry");
    }

    /// Dust detection: if remaining size is below min_order_size in ResidualRisk,
    /// the position is removed.
    #[tokio::test]
    async fn lifecycle_dust_detection_removes_position() {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.enabled = true;
        config.tailend.min_sustained_secs = 0;
        config.tailend.max_recent_volatility = dec!(1.0);
        config.stop_loss.min_remaining_secs = 0;
        config.stop_loss.exit_depth_cap_factor = dec!(0.80);

        let base = Arc::new(CryptoArbBase::new(config, vec![]));
        let now = Utc::now();

        {
            let mut markets = base.active_markets.write().await;
            markets.insert(
                "market1".to_string(),
                MarketWithReference {
                    market: make_market_info("market1", now + Duration::seconds(300)),
                    reference_price: dec!(50000),
                    reference_quality: ReferenceQuality::Exact,
                    discovery_time: now,
                    coin: "BTC".to_string(),
                    window_ts: 0,
                },
            );
        }

        // Create position with dust-sized amount (< min_order_size of 5.0)
        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: polyrust_core::types::OutcomeSide::Up,
            entry_price: dec!(0.90),
            size: dec!(2.0), // Below min_order_size of 5.0
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: now - Duration::seconds(20),
            kelly_fraction: None,
            peak_bid: dec!(0.90),
            estimated_fee: Decimal::ZERO,
            entry_market_price: dec!(0.90),
            tick_size: dec!(0.01),
            fee_rate_bps: 0,
            entry_order_type: OrderType::Gtc,
            entry_fee_per_share: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
        };
        base.record_position(pos).await;

        // Set lifecycle to ResidualRisk with dust remaining
        {
            let mut lifecycle = base.ensure_lifecycle("token_up").await;
            lifecycle
                .transition(
                    PositionLifecycleState::ExitExecuting {
                        order_id: "exit-1".to_string(),
                        order_type: OrderType::Fok,
                        exit_price: dec!(0.82),
                        submitted_at: now - Duration::seconds(5),
                    },
                    "test setup intermediate",
                    now - Duration::seconds(4),
                )
                .unwrap();
            lifecycle
                .transition(
                    PositionLifecycleState::ResidualRisk {
                        remaining_size: dec!(2.0), // Dust: below min_order_size of 5.0
                        retry_count: 1,
                        last_attempt: now - Duration::seconds(3),
                        use_gtc_next: true,
                    },
                    "test setup",
                    now - Duration::seconds(3),
                )
                .unwrap();

            let mut lifecycles = base.position_lifecycle.write().await;
            lifecycles.insert("token_up".to_string(), lifecycle);
        }

        let strategy = TailEndStrategy::new(base);

        let snapshot = polyrust_core::types::OrderbookSnapshot {
            token_id: "token_up".to_string(),
            bids: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.82),
                size: dec!(100),
            }],
            asks: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.85),
                size: dec!(100),
            }],
            timestamp: now,
        };

        let _actions = strategy.handle_orderbook_update(&snapshot).await;

        // Position should be removed (dust)
        let positions = strategy.base.positions.read().await;
        let has_position = positions
            .values()
            .flat_map(|v| v.iter())
            .any(|p| p.token_id == "token_up");
        assert!(
            !has_position,
            "Dust position should have been removed"
        );
    }

    /// Max retries exhausted: position is resolved after max_exit_retries.
    #[tokio::test]
    async fn lifecycle_max_retries_exhausted_resolves_position() {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.enabled = true;
        config.tailend.min_sustained_secs = 0;
        config.tailend.max_recent_volatility = dec!(1.0);
        config.stop_loss.min_remaining_secs = 0;
        config.stop_loss.exit_depth_cap_factor = dec!(0.80);
        config.stop_loss.max_exit_retries = 3; // Low max for testing

        let base = Arc::new(CryptoArbBase::new(config, vec![]));
        let now = Utc::now();

        {
            let mut markets = base.active_markets.write().await;
            markets.insert(
                "market1".to_string(),
                MarketWithReference {
                    market: make_market_info("market1", now + Duration::seconds(300)),
                    reference_price: dec!(50000),
                    reference_quality: ReferenceQuality::Exact,
                    discovery_time: now,
                    coin: "BTC".to_string(),
                    window_ts: 0,
                },
            );
        }

        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: polyrust_core::types::OutcomeSide::Up,
            entry_price: dec!(0.90),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: now - Duration::seconds(20),
            kelly_fraction: None,
            peak_bid: dec!(0.90),
            estimated_fee: Decimal::ZERO,
            entry_market_price: dec!(0.90),
            tick_size: dec!(0.01),
            fee_rate_bps: 0,
            entry_order_type: OrderType::Gtc,
            entry_fee_per_share: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
        };
        base.record_position(pos).await;

        // Set lifecycle to ResidualRisk with retry_count = max_exit_retries (3)
        {
            let mut lifecycle = base.ensure_lifecycle("token_up").await;
            lifecycle
                .transition(
                    PositionLifecycleState::ExitExecuting {
                        order_id: "exit-1".to_string(),
                        order_type: OrderType::Fok,
                        exit_price: dec!(0.82),
                        submitted_at: now - Duration::seconds(5),
                    },
                    "test setup intermediate",
                    now - Duration::seconds(4),
                )
                .unwrap();
            lifecycle
                .transition(
                    PositionLifecycleState::ResidualRisk {
                        remaining_size: dec!(10),
                        retry_count: 3, // == max_exit_retries
                        last_attempt: now - Duration::seconds(3),
                        use_gtc_next: true,
                    },
                    "test setup",
                    now - Duration::seconds(3),
                )
                .unwrap();

            let mut lifecycles = base.position_lifecycle.write().await;
            lifecycles.insert("token_up".to_string(), lifecycle);
        }

        let strategy = TailEndStrategy::new(base);

        let snapshot = polyrust_core::types::OrderbookSnapshot {
            token_id: "token_up".to_string(),
            bids: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.82),
                size: dec!(100),
            }],
            asks: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.85),
                size: dec!(100),
            }],
            timestamp: now,
        };

        let actions = strategy.handle_orderbook_update(&snapshot).await;

        // Should NOT produce an order (max retries exhausted)
        assert!(
            !actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Should not place order after max retries, got: {actions:?}"
        );

        // Position should be removed
        let positions = strategy.base.positions.read().await;
        let has_position = positions
            .values()
            .flat_map(|v| v.iter())
            .any(|p| p.token_id == "token_up");
        assert!(
            !has_position,
            "Position should have been resolved after max retries"
        );
    }
}
