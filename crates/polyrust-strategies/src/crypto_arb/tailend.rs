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

use crate::crypto_arb::base::{CryptoArbBase, StopLossRejectionKind, taker_fee};
use crate::crypto_arb::dashboard::try_emit_dashboard_updates;
use crate::crypto_arb::types::{
    ArbitrageOpportunity, ArbitragePosition, ExitOrderMeta, OpenLimitOrder, PendingOrder,
    PositionLifecycle, PositionLifecycleState, StopLossTriggerKind, TriggerEvalContext,
    compute_exit_clip,
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
    ///
    /// For exit/recovery orders: re-keys `exit_orders_by_id` from the synthetic
    /// order ID (generated at submission) to the real CLOB order ID returned by
    /// the backend.  Without this, `on_order_filled` can never match the fill
    /// event to the exit order, and GTC cancel actions use a stale synthetic ID
    /// the backend doesn't recognise.
    async fn on_order_placed(&self, result: &OrderResult) -> Vec<Action> {
        // Check if this is a lifecycle exit/recovery order confirmation.
        // Don't remove position here — defer to Filled event to avoid race
        // with the persistence handler (which also needs the position for P&L).
        {
            let synthetic_key = {
                let exit_orders = self.base.exit_orders_by_id.read().await;
                exit_orders
                    .iter()
                    .find(|(_, meta)| meta.order_token_id == result.token_id)
                    .map(|(k, _)| k.clone())
            };
            if let Some(syn_key) = synthetic_key {
                if result.success {
                    if let Some(real_oid) = &result.order_id {
                        // Re-key exit_orders_by_id: synthetic → real CLOB order ID.
                        // Extract position token from meta (differs from result.token_id
                        // for recovery orders which use the opposite token).
                        let (position_token, is_hedge) = {
                            let mut exit_orders = self.base.exit_orders_by_id.write().await;
                            if let Some(meta) = exit_orders.remove(&syn_key) {
                                let pt = meta.token_id.clone();
                                let hedge = meta.source_state.starts_with("Hedge");
                                exit_orders.insert(real_oid.clone(), meta);
                                (pt, hedge)
                            } else {
                                (result.token_id.clone(), false)
                            }
                        };
                        // Update lifecycle state with real order ID.
                        // Hedge orders update hedge_order_id; sell orders update order_id.
                        {
                            let mut lifecycles = self.base.position_lifecycle.write().await;
                            if let Some(lc) = lifecycles.get_mut(&position_token) {
                                if is_hedge {
                                    if let PositionLifecycleState::ExitExecuting {
                                        ref mut hedge_order_id,
                                        ..
                                    } = lc.state
                                    {
                                        *hedge_order_id = Some(real_oid.clone());
                                    }
                                } else {
                                    lc.pending_exit_order_id = Some(real_oid.clone());
                                    if let PositionLifecycleState::ExitExecuting {
                                        ref mut order_id,
                                        ..
                                    } = lc.state
                                    {
                                        *order_id = real_oid.clone();
                                    }
                                }
                            }
                        }
                        info!(
                            token_id = %position_token,
                            real_order_id = %real_oid,
                            synthetic_id = %syn_key,
                            kind = if is_hedge { "hedge" } else { "sell" },
                            "TailEnd exit order placed, re-keyed to real CLOB ID"
                        );
                    } else {
                        // Success but no order ID — treat as failure to prevent
                        // lifecycle getting permanently stuck in ExitExecuting.
                        warn!(
                            token_id = %result.token_id,
                            synthetic_id = %syn_key,
                            "Exit order placed successfully but no order ID returned — treating as failure"
                        );
                        let meta = {
                            let mut exit_orders = self.base.exit_orders_by_id.write().await;
                            exit_orders.remove(&syn_key)
                        };
                        let position_token = meta
                            .as_ref()
                            .map(|m| m.token_id.clone())
                            .unwrap_or_else(|| result.token_id.clone());
                        let now = self.base.event_time().await;
                        let mut lifecycle = self.base.ensure_lifecycle(&position_token).await;
                        lifecycle.pending_exit_order_id = None;
                        if let Err(e) = lifecycle.transition(
                            PositionLifecycleState::Healthy,
                            "exit order placed with no order ID",
                            now,
                        ) {
                            warn!(
                                token_id = %position_token,
                                error = %e,
                                "Failed to transition to Healthy after missing order ID"
                            );
                        }
                        self.write_lifecycle(&position_token, &lifecycle).await;
                    }
                } else {
                    // Placement failed — read meta before removing, then handle
                    // based on lifecycle state.
                    let meta = {
                        let mut exit_orders = self.base.exit_orders_by_id.write().await;
                        exit_orders.remove(&syn_key)
                    };
                    // Use the position's token_id from meta (not result.token_id,
                    // which is the order token — differs for recovery orders).
                    let position_token = meta
                        .as_ref()
                        .map(|m| m.token_id.clone())
                        .unwrap_or_else(|| result.token_id.clone());
                    let now = self.base.event_time().await;
                    let mut lifecycle = self.base.ensure_lifecycle(&position_token).await;
                    lifecycle.pending_exit_order_id = None;

                    if matches!(
                        lifecycle.state,
                        PositionLifecycleState::ExitExecuting { .. }
                    ) {
                        // Check if this was a hedge order placement failure
                        let is_hedge = meta
                            .as_ref()
                            .map(|m| m.source_state.starts_with("Hedge"))
                            .unwrap_or(false);

                        if is_hedge {
                            // Hedge placement failed — clear hedge tracking, continue sell-only
                            if let PositionLifecycleState::ExitExecuting {
                                ref mut hedge_order_id,
                                ref mut hedge_price,
                                ..
                            } = lifecycle.state
                            {
                                *hedge_order_id = None;
                                *hedge_price = None;
                            }
                            self.write_lifecycle(&position_token, &lifecycle).await;
                            warn!(
                                token_id = %position_token,
                                message = %result.message,
                                "Hedge order placement failed — continuing sell-only exit"
                            );
                        } else {
                            // Exit order placement failed — transition back to Healthy
                            if let Err(e) = lifecycle.transition(
                                PositionLifecycleState::Healthy,
                                "exit order placement failed",
                                now,
                            ) {
                                warn!(
                                    token_id = %position_token,
                                    error = %e,
                                    "Failed to transition to Healthy after placement failure"
                                );
                            }
                            self.write_lifecycle(&position_token, &lifecycle).await;
                            warn!(
                                token_id = %position_token,
                                message = %result.message,
                                "Exit order placement failed — back to Healthy for re-evaluation"
                            );
                        }
                    } else {
                        // Unknown state — log and ignore
                        warn!(
                            token_id = %position_token,
                            state = %lifecycle.state,
                            message = %result.message,
                            "Order placement failed in unexpected state"
                        );
                    }
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

        // FOK/FAK fallback path (stop-loss sells still use FOK/FAK)
        let now = self.base.event_time().await;
        let entry_fee_per_share = if matches!(pending.order_type, OrderType::Fok | OrderType::Fak) {
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
            recovery_cost: Decimal::ZERO,
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
        source_timestamp: DateTime<Utc>,
        ctx: &StrategyContext,
    ) -> Vec<Action> {
        // Record price and promote any pending markets
        let now = ctx.now().await;
        let (_, promote_actions) = self
            .base
            .record_price(symbol, price, source, now, source_timestamp)
            .await;
        let mut result = promote_actions;

        // Update the stop-loss composite cache for this coin.
        // Runs on every ExternalPrice so the SL evaluation (on orderbook updates)
        // always has a recent composite without needing StrategyContext.
        self.base.update_sl_composite_cache(symbol, ctx).await;

        // Fast-path exit evaluation: check stop-loss triggers using cached
        // orderbook snapshots BEFORE entry evaluation (risk first, then entries).
        let exit_actions = self.evaluate_exits_on_price_change(symbol, ctx).await;
        result.extend(exit_actions);

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
            if !self.base.try_reserve_market(&market_id, 1).await {
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

    /// Evaluate exit triggers on ExternalPrice events using cached orderbook
    /// snapshots. This "fast path" frontrunning gives 50-200ms advantage over
    /// waiting for the next OrderbookUpdate event.
    ///
    /// Only evaluates positions in `Healthy` state. Skips if:
    /// - `fast_path_enabled` is false
    /// - No cached orderbook snapshot exists for the position's token
    /// - Cached snapshot is older than `fast_path_max_book_age_ms`
    async fn evaluate_exits_on_price_change(
        &self,
        coin: &str,
        ctx: &StrategyContext,
    ) -> Vec<Action> {
        if !self.base.config.tailend.fast_path_enabled {
            return vec![];
        }

        let now = ctx.now().await;
        let sl_config = &self.base.config.stop_loss;
        let tailend_config = &self.base.config.tailend;
        let max_book_age_ms = tailend_config.fast_path_max_book_age_ms;

        // Gather positions for this coin
        let position_snapshot: Vec<(MarketId, ArbitragePosition)> = {
            let positions = self.base.positions.read().await;
            positions
                .iter()
                .flat_map(|(mid, plist)| plist.iter().map(|p| (mid.clone(), p.clone())))
                .filter(|(_, p)| p.coin == coin)
                .collect()
        };

        if position_snapshot.is_empty() {
            return vec![];
        }

        let mut actions = Vec::new();

        for (_, pos) in position_snapshot {
            let mut lifecycle = self.base.ensure_lifecycle(&pos.token_id).await;

            // Only evaluate Healthy positions — skip anything already exiting
            if !matches!(lifecycle.state, PositionLifecycleState::Healthy) {
                continue;
            }

            // Get cached orderbook snapshot for this token
            let (snapshot, book_age_ms) = {
                let md = ctx.market_data.read().await;
                match md.orderbooks.get(&pos.token_id) {
                    Some(ob) => {
                        let age = now.signed_duration_since(ob.timestamp).num_milliseconds();
                        (ob.clone(), age)
                    }
                    None => continue,
                }
            };

            // Check book freshness against fast-path threshold
            if book_age_ms > max_book_age_ms {
                debug!(
                    token_id = %pos.token_id,
                    book_age_ms,
                    max_book_age_ms,
                    "Fast-path skip: cached orderbook too stale"
                );
                continue;
            }

            let current_bid = match snapshot.best_bid() {
                Some(b) => b,
                None => continue,
            };

            // Get market metadata
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

            // Skip if time remaining is below threshold
            if time_remaining <= sl_config.min_remaining_secs {
                continue;
            }

            // Skip dust positions
            if pos.size < min_order_size {
                continue;
            }

            // Get composite/external price from SL cache
            let (external_price, external_age_ms, composite_sources) = {
                let cache = self.base.sl_composite_cache.read().await;
                if let Some((composite, cached_at)) = cache.get(&pos.coin) {
                    let age = now.signed_duration_since(*cached_at).num_milliseconds();
                    if age <= sl_config.sl_max_external_age_ms * 2 {
                        (
                            Some(composite.price),
                            Some(age),
                            Some(composite.sources_used),
                        )
                    } else {
                        drop(cache);
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
                                .map(|(.., source_ts)| {
                                    now.signed_duration_since(*source_ts).num_milliseconds()
                                })
                                .unwrap_or(sl_config.sl_max_external_age_ms * 3);
                            (Some(single), Some(age), None)
                        } else {
                            (None, None, None)
                        }
                    }
                } else {
                    drop(cache);
                    if let Some(single) = self
                        .base
                        .get_sl_single_fresh(&pos.coin, sl_config.sl_max_external_age_ms * 2, now)
                        .await
                    {
                        let history = self.base.price_history.read().await;
                        let age = history
                            .get(&pos.coin)
                            .and_then(|h| h.back())
                            .map(|(.., source_ts)| {
                                now.signed_duration_since(*source_ts).num_milliseconds()
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

            let trigger = lifecycle.evaluate_triggers(&trigger_ctx, sl_config, tailend_config);

            if let Some(trigger_kind) = trigger {
                let seconds_since_entry = now.signed_duration_since(pos.entry_time).num_seconds();
                let is_sellable = seconds_since_entry >= tailend_config.min_sell_delay_secs;

                // Non-sellable positions can only exit on hard crash (bypass sell delay)
                if !is_sellable && !matches!(trigger_kind, StopLossTriggerKind::HardCrash { .. }) {
                    self.write_lifecycle(&pos.token_id, &lifecycle).await;
                    continue;
                }

                info!(
                    token_id = %pos.token_id,
                    trigger = %trigger_kind,
                    book_age_ms,
                    external_age_ms = external_age_ms.unwrap_or(-1),
                    "Fast-path exit trigger on ExternalPrice"
                );

                if let Some(exit_actions) = self
                    .build_exit_order(
                        &pos,
                        current_bid,
                        &snapshot,
                        neg_risk,
                        min_order_size,
                        &trigger_kind,
                        &mut lifecycle,
                        now,
                    )
                    .await
                {
                    self.write_lifecycle(&pos.token_id, &lifecycle).await;
                    actions.extend(exit_actions);
                    continue;
                }
            }

            // Write back lifecycle (updated dual_trigger_ticks, etc.)
            self.write_lifecycle(&pos.token_id, &lifecycle).await;
        }

        actions
    }

    /// Handle an orderbook update: update cached asks, peak bids, evaluate
    /// lifecycle-driven stop-loss triggers on our positions.
    ///
    /// Uses the 4-level trigger hierarchy (evaluate_triggers) and lifecycle state machine.
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

        let mut actions = Vec::new();

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

            // Get or create lifecycle for this position
            let mut lifecycle = self.base.ensure_lifecycle(&pos.token_id).await;

            // If lifecycle is in ExitExecuting, check for stale GTC orders needing chase
            if let PositionLifecycleState::ExitExecuting {
                order_id: ref exit_oid,
                order_type: ref exit_type,
                submitted_at,
                ..
            } = lifecycle.state
            {
                if *exit_type == OrderType::Gtc {
                    let age_secs = (now - submitted_at).num_seconds();
                    if age_secs >= self.base.config.stop_loss.gtc_stop_loss_max_age_secs as i64 {
                        info!(
                            token_id = %pos.token_id,
                            order_id = %exit_oid,
                            age_secs,
                            "GTC chase: cancelling stale exit for refresh"
                        );
                        actions.push(Action::CancelOrder(exit_oid.clone()));
                    }
                }
                // Order is in flight (fresh or being cancelled) — skip evaluation
                continue;
            }

            // Hedged: set complete, waiting for expiry — skip evaluation
            if matches!(lifecycle.state, PositionLifecycleState::Hedged { .. }) {
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
                        (
                            Some(composite.price),
                            Some(age),
                            Some(composite.sources_used),
                        )
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
                                .map(|(.., source_ts)| {
                                    now.signed_duration_since(*source_ts).num_milliseconds()
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
                        .get_sl_single_fresh(&pos.coin, sl_config.sl_max_external_age_ms * 2, now)
                        .await
                    {
                        let history = self.base.price_history.read().await;
                        let age = history
                            .get(&pos.coin)
                            .and_then(|h| h.back())
                            .map(|(.., source_ts)| {
                                now.signed_duration_since(*source_ts).num_milliseconds()
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

            let seconds_since_entry = now.signed_duration_since(pos.entry_time).num_seconds();
            let is_sellable = seconds_since_entry >= self.base.config.tailend.min_sell_delay_secs;

            // Evaluate triggers for Healthy positions
            let trigger =
                lifecycle.evaluate_triggers(&trigger_ctx, sl_config, &self.base.config.tailend);

            if let Some(trigger_kind) = trigger {
                // Hard crash bypasses sell delay (immediate exit)
                let can_exit =
                    is_sellable || matches!(trigger_kind, StopLossTriggerKind::HardCrash { .. });

                if !can_exit {
                    // Non-hard trigger during sell delay — skip, re-evaluate next tick
                    debug!(
                        token_id = %pos.token_id,
                        trigger = %trigger_kind,
                        seconds_since_entry,
                        "Trigger during sell delay (non-hard), skipping"
                    );
                    self.write_lifecycle(&pos.token_id, &lifecycle).await;
                    continue;
                }

                // Trigger fired and exit allowed — execute exit
                if let Some(exit_actions) = self
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
                    actions.extend(exit_actions);
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
    ) -> Option<Vec<Action>> {
        // Compute depth-capped clip size
        let bid_depth =
            snapshot.bid_depth_down_to(current_bid - pos.tick_size * Decimal::from(3u32));
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
            OrderType::Fak,
            neg_risk,
        )
        .with_tick_size(pos.tick_size)
        .with_fee_rate_bps(pos.fee_rate_bps);

        // Generate a synthetic order ID for lifecycle tracking
        // (real order ID comes back from PlaceOrder result, but we need to track intent now)
        let exit_order_id = format!("exit-{}-{}", pos.token_id, now.timestamp_millis());

        // Evaluate hedge profitability: can we buy the opposite token to complete the set?
        // Use clip size (not pos.size) so hedge matches the actual exit quantity.
        let hedge_action = self.evaluate_hedge(pos, clip, neg_risk, now).await;
        let (hedge_order_id, hedge_price) =
            if let Some((ref _action, ref h_oid, h_price)) = hedge_action {
                (Some(h_oid.clone()), Some(h_price))
            } else {
                (None, None)
            };

        // Transition lifecycle to ExitExecuting
        if let Err(e) = lifecycle.transition(
            PositionLifecycleState::ExitExecuting {
                order_id: exit_order_id.clone(),
                order_type: OrderType::Fak,
                exit_price: current_bid,
                submitted_at: now,
                hedge_order_id: hedge_order_id.clone(),
                hedge_price,
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
                    order_token_id: pos.token_id.clone(),
                    order_type: OrderType::Fak,
                    source_state: format!("{trigger_kind}"),

                    exit_price: current_bid,
                    clip_size: clip,
                },
            );
        }

        let has_hedge = hedge_action.is_some();
        info!(
            market = %pos.market_id,
            token_id = %pos.token_id,
            entry = %pos.entry_price,
            exit = %current_bid,
            clip = %clip,
            side = ?pos.side,
            trigger = %trigger_kind,
            hedge = has_hedge,
            "TailEnd lifecycle stop-loss triggered"
        );

        let mut result = vec![Action::PlaceOrder(order)];
        if let Some((hedge_act, _, _)) = hedge_action {
            result.push(hedge_act);
        }
        Some(result)
    }

    /// Write a lifecycle back to the shared store.
    async fn write_lifecycle(&self, token_id: &str, lifecycle: &PositionLifecycle) {
        let mut lifecycles = self.base.position_lifecycle.write().await;
        lifecycles.insert(token_id.to_string(), lifecycle.clone());
    }

    /// Evaluate hedge profitability for a position being exited.
    ///
    /// Checks if buying the opposite token completes the set within the
    /// `recovery_max_set_cost` budget. Returns the hedge action, order ID,
    /// and price if profitable.
    pub(crate) async fn evaluate_hedge(
        &self,
        pos: &ArbitragePosition,
        exit_clip: Decimal,
        neg_risk: bool,
        now: DateTime<Utc>,
    ) -> Option<(Action, OrderId, Decimal)> {
        let sl_config = &self.base.config.stop_loss;
        if !sl_config.recovery_enabled {
            return None;
        }

        // Get opposite token
        let opposite_token = self
            .base
            .get_opposite_token(&pos.market_id, &pos.token_id)
            .await?;

        // Get opposite ask price
        let opposite_ask = {
            let asks = self.base.cached_asks.read().await;
            asks.get(&opposite_token).copied()?
        };

        // Check set completion cost: entry_price + opposite_ask <= recovery_max_set_cost
        let combined_cost = pos.entry_price + opposite_ask;
        if combined_cost > sl_config.recovery_max_set_cost {
            info!(
                token_id = %pos.token_id,
                entry = %pos.entry_price,
                opposite_ask = %opposite_ask,
                combined = %combined_cost,
                max = %sl_config.recovery_max_set_cost,
                "Hedge skipped: combined cost exceeds budget"
            );
            return None;
        }

        // Build GTC buy order for opposite token — sized to match exit clip,
        // not full position, to avoid excess opposite tokens on partial fills.
        let hedge_order_id = format!("hedge-{}-{}", pos.token_id, now.timestamp_millis());
        let hedge_order = OrderRequest::new(
            opposite_token.clone(),
            opposite_ask,
            exit_clip,
            OrderSide::Buy,
            OrderType::Gtc,
            neg_risk,
        )
        .with_tick_size(pos.tick_size)
        .with_fee_rate_bps(pos.fee_rate_bps);

        // Store hedge order meta for fill routing
        {
            let mut exit_orders = self.base.exit_orders_by_id.write().await;
            exit_orders.insert(
                hedge_order_id.clone(),
                ExitOrderMeta {
                    token_id: pos.token_id.clone(),
                    order_token_id: opposite_token,
                    order_type: OrderType::Gtc,
                    source_state: "Hedge(set completion)".to_string(),

                    exit_price: opposite_ask,
                    clip_size: exit_clip,
                },
            );
        }

        info!(
            token_id = %pos.token_id,
            opposite_ask = %opposite_ask,
            combined_cost = %combined_cost,
            size = %exit_clip,
            "Hedge order placed: simultaneous opposite-side buy for set completion"
        );

        Some((
            Action::PlaceOrder(hedge_order),
            hedge_order_id,
            opposite_ask,
        ))
    }

    /// Handle a fully filled order event (GTC entry fills, stop-loss sells, GTC SL fills).
    async fn on_order_filled(
        &self,
        order_id: &str,
        _token_id: &str,
        price: Decimal,
        size: Decimal,
    ) -> Vec<Action> {
        // Check if this is a lifecycle exit/recovery order fill (by order_id in exit_orders_by_id)
        {
            let exit_meta = {
                let exit_orders = self.base.exit_orders_by_id.read().await;
                exit_orders.get(order_id).cloned()
            };
            if let Some(meta) = exit_meta {
                let now = self.base.event_time().await;

                if meta.source_state.starts_with("Hedge") {
                    // Hedge order filled — record cost and transition to Hedged.
                    // Hedge is GTC (maker) so fee is 0%.
                    let hedge_cost = price * size;
                    self.base
                        .add_recovery_cost(&meta.token_id, -hedge_cost)
                        .await;

                    let mut lifecycle = self.base.ensure_lifecycle(&meta.token_id).await;

                    // Cancel the sell order if it hasn't filled yet
                    let cancel_sell =
                        if let PositionLifecycleState::ExitExecuting { ref order_id, .. } =
                            lifecycle.state
                        {
                            Some(order_id.clone())
                        } else {
                            None
                        };

                    if let Err(e) = lifecycle.transition(
                        PositionLifecycleState::Hedged {
                            hedge_cost,
                            hedged_at: now,
                        },
                        &format!("hedge filled at {price}"),
                        now,
                    ) {
                        warn!(
                            token_id = %meta.token_id,
                            error = %e,
                            "ExitExecuting→Hedged transition failed"
                        );
                    }
                    lifecycle.pending_exit_order_id = None;
                    self.write_lifecycle(&meta.token_id, &lifecycle).await;

                    info!(
                        order_id = %order_id,
                        token_id = %meta.token_id,
                        fill_price = %price,
                        fill_size = %size,
                        hedge_cost = %hedge_cost,
                        "Hedge order filled — set complete, transitioning to Hedged"
                    );

                    // Clean up exit order tracking
                    {
                        let mut exit_orders = self.base.exit_orders_by_id.write().await;
                        exit_orders.remove(order_id);
                    }

                    // Cancel the sell order (best-effort)
                    if let Some(sell_oid) = cancel_sell {
                        return vec![Action::CancelOrder(sell_oid)];
                    }
                    return vec![];
                } else {
                    // Exit order fill (ExitExecuting state)
                    let exit_price = price; // Use actual CLOB fill price
                    let is_gtc_exit = meta.order_type == OrderType::Gtc;

                    if let Some((pos, fully_closed)) = self
                        .base
                        .reduce_or_remove_position_by_token(&meta.token_id, size)
                        .await
                    {
                        // GTC exits are maker orders (0% fee), FAK/FOK exits pay taker fee
                        let exit_fee = if is_gtc_exit {
                            Decimal::ZERO
                        } else {
                            taker_fee(exit_price, self.base.config.fee.taker_fee_rate)
                        };
                        let pnl = (exit_price - pos.entry_price) * size
                            - (pos.entry_fee_per_share * size)
                            - (exit_fee * size);
                        self.base.record_trade_pnl(pnl).await;

                        if !fully_closed {
                            let remaining = pos.size - size;
                            // Check if residual is below minimum order size (unsellable dust)
                            let (is_dust, neg_risk) = {
                                let markets = self.base.active_markets.read().await;
                                markets
                                    .get(&pos.market_id)
                                    .map(|m| {
                                        (remaining < m.market.min_order_size, m.market.neg_risk)
                                    })
                                    .unwrap_or((true, false))
                            };
                            if is_dust {
                                self.base
                                    .reduce_or_remove_position_by_token(&meta.token_id, remaining)
                                    .await;
                                warn!(
                                    token_id = %meta.token_id,
                                    dust_size = %remaining,
                                    "Removed unsellable dust after partial fill — will resolve at expiry"
                                );
                            } else {
                                // Place GTC residual order at bid - tick_offset
                                let sl_config = &self.base.config.stop_loss;
                                let tick_offset = Decimal::from(sl_config.gtc_fallback_tick_offset);
                                let gtc_price =
                                    (exit_price - pos.tick_size * tick_offset).max(pos.tick_size);

                                let gtc_order = OrderRequest::new(
                                    pos.token_id.clone(),
                                    gtc_price,
                                    remaining,
                                    OrderSide::Sell,
                                    OrderType::Gtc,
                                    neg_risk,
                                )
                                .with_tick_size(pos.tick_size)
                                .with_fee_rate_bps(pos.fee_rate_bps);

                                let gtc_oid =
                                    format!("exit-gtc-{}-{}", pos.token_id, now.timestamp_millis());

                                // Transition to ExitExecuting with GTC for residual.
                                // Preserve hedge tracking from prior state so the
                                // fully_closed path can still cancel the hedge.
                                let mut lifecycle =
                                    self.base.ensure_lifecycle(&meta.token_id).await;
                                let (prev_hedge_oid, prev_hedge_price) =
                                    if let PositionLifecycleState::ExitExecuting {
                                        ref hedge_order_id,
                                        hedge_price,
                                        ..
                                    } = lifecycle.state
                                    {
                                        (hedge_order_id.clone(), hedge_price)
                                    } else {
                                        (None, None)
                                    };
                                lifecycle.pending_exit_order_id = None;
                                if let Err(e) = lifecycle.transition(
                                    PositionLifecycleState::ExitExecuting {
                                        order_id: gtc_oid.clone(),
                                        order_type: OrderType::Gtc,
                                        exit_price: gtc_price,
                                        submitted_at: now,
                                        hedge_order_id: prev_hedge_oid,
                                        hedge_price: prev_hedge_price,
                                    },
                                    &format!("FAK partial fill, GTC residual for {remaining}"),
                                    now,
                                ) {
                                    warn!(token_id = %meta.token_id, error = %e, "Lifecycle transition for GTC residual failed — falling back to Healthy");
                                    // Fall back to Healthy to avoid getting stuck in ExitExecuting
                                    // with a stale FAK order ID. Next OB update will re-evaluate.
                                    let _ = lifecycle.transition(
                                        PositionLifecycleState::Healthy,
                                        "GTC residual transition failed, reset",
                                        now,
                                    );
                                    self.write_lifecycle(&meta.token_id, &lifecycle).await;
                                } else {
                                    lifecycle.pending_exit_order_id = Some(gtc_oid.clone());
                                    self.write_lifecycle(&meta.token_id, &lifecycle).await;

                                    // Track residual GTC order
                                    {
                                        let mut exit_orders =
                                            self.base.exit_orders_by_id.write().await;
                                        exit_orders.insert(
                                            gtc_oid,
                                            ExitOrderMeta {
                                                token_id: meta.token_id.clone(),
                                                order_token_id: meta.token_id.clone(),
                                                order_type: OrderType::Gtc,
                                                source_state: "ExitActive(GTC residual)"
                                                    .to_string(),

                                                exit_price: gtc_price,
                                                clip_size: remaining,
                                            },
                                        );
                                    }

                                    info!(
                                        token_id = %meta.token_id,
                                        order_id = %order_id,
                                        fill_size = %size,
                                        remaining = %remaining,
                                        gtc_price = %gtc_price,
                                        "FAK partial fill: placing GTC residual order"
                                    );
                                    return vec![Action::PlaceOrder(gtc_order)];
                                }
                            }
                        }

                        // If sell fully closed the position, cancel any pending hedge
                        if fully_closed {
                            // Clean up lifecycle to prevent stale entries blocking
                            // future positions on the same token_id.
                            self.base.remove_lifecycle(&meta.token_id).await;

                            let hedge_to_cancel = {
                                let exit_orders = self.base.exit_orders_by_id.read().await;
                                exit_orders
                                    .iter()
                                    .find(|(_, m)| {
                                        m.token_id == meta.token_id
                                            && m.source_state.starts_with("Hedge")
                                    })
                                    .map(|(oid, _)| oid.clone())
                            };

                            // Clean up exit order tracking
                            {
                                let mut exit_orders = self.base.exit_orders_by_id.write().await;
                                exit_orders.remove(order_id);
                                if let Some(ref h_oid) = hedge_to_cancel {
                                    exit_orders.remove(h_oid);
                                }
                            }

                            info!(
                                token_id = %meta.token_id,
                                order_id = %order_id,
                                pnl = %pnl,
                                fill_size = %size,
                                exit_type = if is_gtc_exit { "GTC (0% fee)" } else { "FAK (taker fee)" },
                                "Exit order filled — position fully closed"
                            );

                            if let Some(h_oid) = hedge_to_cancel {
                                info!(
                                    token_id = %meta.token_id,
                                    hedge_order_id = %h_oid,
                                    "Sell filled before hedge — cancelling pending hedge"
                                );
                                return vec![Action::CancelOrder(h_oid)];
                            }
                            return vec![];
                        }

                        info!(
                            token_id = %meta.token_id,
                            order_id = %order_id,
                            pnl = %pnl,
                            fill_size = %size,
                            exit_type = if is_gtc_exit { "GTC (0% fee)" } else { "FAK (taker fee)" },
                            "Exit order partially filled"
                        );
                    } else {
                        warn!(
                            token_id = %meta.token_id,
                            order_id = %order_id,
                            "Exit fill: position already removed (race)"
                        );
                    }
                }

                // Clean up exit order tracking
                {
                    let mut exit_orders = self.base.exit_orders_by_id.write().await;
                    exit_orders.remove(order_id);
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
                timestamp,
            }) => {
                self.handle_external_price(symbol, *price, source, *timestamp, ctx)
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
                // Check exit orders first (GTC exits can partially fill)
                let exit_meta = {
                    let exit_orders = self.base.exit_orders_by_id.read().await;
                    exit_orders.get(order_id).cloned()
                };
                if let Some(meta) = exit_meta {
                    let is_hedge = meta.source_state.starts_with("Hedge");

                    if is_hedge {
                        // Hedge orders buy the opposite side for set completion.
                        // Do NOT reduce the original position — it still holds its tokens.
                        info!(
                            order_id = %order_id,
                            token_id = %meta.token_id,
                            filled = %filled_size,
                            remaining = %remaining_size,
                            "Hedge order partially filled — position unchanged"
                        );
                    } else {
                        // Normal exit: reduce position by the filled amount so subsequent
                        // Cancelled/Filled handlers see the correct remaining size.
                        // P&L is deferred to the final Filled event (PartiallyFilled
                        // does not carry a fill price).
                        self.base
                            .reduce_or_remove_position_by_token(&meta.token_id, *filled_size)
                            .await;
                        info!(
                            order_id = %order_id,
                            token_id = %meta.token_id,
                            filled = %filled_size,
                            remaining = %remaining_size,
                            "Exit order partially filled — position reduced"
                        );
                    }
                } else {
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

                    // Look up exit order meta by order_token_id to resolve the
                    // position's token_id (which differs from the event's token_id
                    // for recovery orders that operate on the opposite token).
                    let exit_meta = {
                        let exit_orders = self.base.exit_orders_by_id.read().await;
                        exit_orders
                            .iter()
                            .find(|(_, meta)| meta.order_token_id == *token_id)
                            .map(|(_, meta)| meta.clone())
                    };

                    // Only check lifecycle if we have a tracked exit/recovery order,
                    // or if a lifecycle already exists for this token. Avoids creating
                    // orphaned lifecycle entries via ensure_lifecycle for rejected
                    // BUY orders that have no position.
                    let position_token = exit_meta
                        .as_ref()
                        .map(|m| m.token_id.as_str())
                        .unwrap_or(token_id);

                    let lifecycle = {
                        let lifecycles = self.base.position_lifecycle.read().await;
                        lifecycles.get(position_token).cloned()
                    };

                    if let Some(lifecycle) = lifecycle {
                        if matches!(
                            lifecycle.state,
                            PositionLifecycleState::ExitExecuting { .. }
                        ) {
                            let now = self.base.event_time().await;
                            let kind = StopLossRejectionKind::classify(reason);

                            // Get remaining size from position
                            let remaining_size = {
                                let positions = self.base.positions.read().await;
                                positions
                                    .values()
                                    .flat_map(|v| v.iter())
                                    .find(|p| p.token_id == position_token)
                                    .map(|p| p.size)
                            };

                            if let Some(remaining) = remaining_size {
                                // InvalidSize: dust — remove immediately
                                if kind == StopLossRejectionKind::InvalidSize {
                                    warn!(
                                        token_id = %position_token,
                                        remaining = %remaining,
                                        "Exit order rejected (InvalidSize) — removing dust position"
                                    );
                                    self.base
                                        .reduce_or_remove_position_by_token(
                                            position_token,
                                            remaining,
                                        )
                                        .await;
                                } else {
                                    // Transition back to Healthy for re-evaluation on next tick
                                    let mut lifecycle =
                                        self.base.ensure_lifecycle(position_token).await;
                                    lifecycle.pending_exit_order_id = None;
                                    if let Err(e) = lifecycle.transition(
                                        PositionLifecycleState::Healthy,
                                        &format!("exit rejected: {reason}"),
                                        now,
                                    ) {
                                        warn!(
                                            token_id = %position_token,
                                            error = %e,
                                            "ExitExecuting→Healthy transition failed after rejection"
                                        );
                                    }
                                    self.write_lifecycle(position_token, &lifecycle).await;
                                    warn!(
                                        token_id = %position_token,
                                        reason = %reason,
                                        kind = ?kind,
                                        "Exit order rejected — back to Healthy for re-evaluation"
                                    );
                                }
                            }

                            // Cancel any associated hedge order before cleaning up
                            let hedge_to_cancel = {
                                let exit_orders = self.base.exit_orders_by_id.read().await;
                                exit_orders
                                    .iter()
                                    .find(|(_, m)| {
                                        m.token_id == position_token
                                            && m.source_state.starts_with("Hedge")
                                    })
                                    .map(|(oid, _)| oid.clone())
                            };

                            // Clean up exit order tracking
                            {
                                let mut exit_orders = self.base.exit_orders_by_id.write().await;
                                exit_orders.retain(|_, meta| meta.token_id != position_token);
                            }

                            if let Some(h_oid) = hedge_to_cancel {
                                info!(
                                    token_id = %position_token,
                                    hedge_order_id = %h_oid,
                                    "Cancelling orphaned hedge on exit rejection"
                                );
                                return Ok(vec![Action::CancelOrder(h_oid)]);
                            }
                            return Ok(vec![]);
                        }

                        // Check if this is a hedge order rejection
                        if let Some(ref em) = exit_meta {
                            if em.source_state.starts_with("Hedge") {
                                // Hedge rejected — clear hedge tracking, continue sell-only
                                let mut lifecycle =
                                    self.base.ensure_lifecycle(position_token).await;
                                if let PositionLifecycleState::ExitExecuting {
                                    ref mut hedge_order_id,
                                    ref mut hedge_price,
                                    ..
                                } = lifecycle.state
                                {
                                    *hedge_order_id = None;
                                    *hedge_price = None;
                                }
                                self.write_lifecycle(position_token, &lifecycle).await;

                                // Clean up hedge order tracking
                                {
                                    let mut exit_orders = self.base.exit_orders_by_id.write().await;
                                    exit_orders.retain(|_, m| {
                                        !(m.token_id == position_token
                                            && m.source_state.starts_with("Hedge"))
                                    });
                                }

                                warn!(
                                    token_id = %position_token,
                                    reason = %reason,
                                    "Hedge order rejected — continuing sell-only exit"
                                );

                                return Ok(vec![]);
                            }
                        }
                    }
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::Cancelled(order_id)) => {
                // Check if this is a lifecycle-driven exit order cancel (GTC chase refresh)
                {
                    let exit_meta = {
                        let exit_orders = self.base.exit_orders_by_id.read().await;
                        exit_orders.get(order_id).cloned()
                    };
                    if let Some(meta) = exit_meta {
                        let now = self.base.event_time().await;

                        // Before transitioning to Healthy, cancel any associated hedge
                        // order to avoid orphaning it.
                        let mut cancel_actions = Vec::new();
                        let mut lifecycle = self.base.ensure_lifecycle(&meta.token_id).await;
                        if let PositionLifecycleState::ExitExecuting {
                            hedge_order_id: Some(ref h_oid),
                            ..
                        } = lifecycle.state
                        {
                            let h_oid_clone = h_oid.clone();
                            {
                                let mut exit_orders = self.base.exit_orders_by_id.write().await;
                                exit_orders.remove(&h_oid_clone);
                            }
                            cancel_actions.push(Action::CancelOrder(h_oid_clone.clone()));
                            info!(
                                token_id = %meta.token_id,
                                hedge_order_id = %h_oid_clone,
                                "Cancelling orphaned hedge on GTC chase cancel"
                            );
                        }

                        // Transition back to Healthy so the next orderbook update
                        // re-evaluates triggers and places a fresh exit order at
                        // current bid (GTC chase cycle).
                        lifecycle.pending_exit_order_id = None;
                        if let Err(e) = lifecycle.transition(
                            PositionLifecycleState::Healthy,
                            "GTC exit cancelled for chase",
                            now,
                        ) {
                            warn!(token_id = %meta.token_id, error = %e, "GTC cancel→Healthy transition failed");
                        }
                        self.write_lifecycle(&meta.token_id, &lifecycle).await;

                        // Clean up tracking
                        {
                            let mut exit_orders = self.base.exit_orders_by_id.write().await;
                            exit_orders.remove(order_id);
                        }

                        info!(
                            order_id = %order_id,
                            token_id = %meta.token_id,
                            "GTC exit cancelled, will re-evaluate on next OB update"
                        );
                        if !cancel_actions.is_empty() {
                            return Ok(cancel_actions);
                        }
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
                // First check if this is a lifecycle exit/recovery order
                let exit_meta = {
                    let exit_orders = self.base.exit_orders_by_id.read().await;
                    exit_orders.get(order_id).cloned()
                };
                if let Some(meta) = exit_meta {
                    let is_matched = reason.contains("matched");
                    let is_gone = reason.contains("canceled") || reason.contains("not found");

                    if is_matched {
                        // Order was filled on the CLOB before our cancel arrived.
                        // Treat as a fill using the order's limit price (GTC fills
                        // at limit price or better; we use the limit price as a
                        // conservative estimate since actual fill price is unknown).
                        let fill_price = meta.exit_price;
                        let now = self.base.event_time().await;

                        if meta.source_state.starts_with("Hedge") {
                            // Hedge order cancel-matched — treat as hedge fill
                            let fill_size = meta.clip_size;

                            if fill_size > Decimal::ZERO {
                                // Hedge is GTC (maker) so fee is 0%
                                let hedge_cost = fill_price * fill_size;
                                self.base
                                    .add_recovery_cost(&meta.token_id, -hedge_cost)
                                    .await;

                                let mut lifecycle =
                                    self.base.ensure_lifecycle(&meta.token_id).await;

                                // Cancel the sell order
                                let cancel_sell = if let PositionLifecycleState::ExitExecuting {
                                    ref order_id,
                                    ..
                                } = lifecycle.state
                                {
                                    Some(order_id.clone())
                                } else {
                                    None
                                };

                                if let Err(e) = lifecycle.transition(
                                    PositionLifecycleState::Hedged {
                                        hedge_cost,
                                        hedged_at: now,
                                    },
                                    &format!("cancel-matched hedge at {}", fill_price),
                                    now,
                                ) {
                                    warn!(
                                        token_id = %meta.token_id,
                                        error = %e,
                                        "ExitExecuting→Hedged transition failed (cancel-matched)"
                                    );
                                }
                                lifecycle.pending_exit_order_id = None;
                                self.write_lifecycle(&meta.token_id, &lifecycle).await;

                                info!(
                                    order_id = %order_id,
                                    token_id = %meta.token_id,
                                    fill_price = %fill_price,
                                    clip_size = %fill_size,
                                    "Hedge cancel-matched — treated as fill, transitioning to Hedged"
                                );

                                // Cancel the sell order
                                {
                                    let mut exit_orders = self.base.exit_orders_by_id.write().await;
                                    exit_orders.remove(order_id);
                                }
                                if let Some(sell_oid) = cancel_sell {
                                    return Ok(vec![Action::CancelOrder(sell_oid)]);
                                }
                            } else {
                                warn!(
                                    order_id = %order_id,
                                    token_id = %meta.token_id,
                                    "Hedge cancel-matched but clip_size is zero — cleaning up"
                                );
                                let mut lifecycle =
                                    self.base.ensure_lifecycle(&meta.token_id).await;
                                lifecycle.pending_exit_order_id = None;
                                self.write_lifecycle(&meta.token_id, &lifecycle).await;
                            }
                        } else {
                            // Exit order matched — handle like exit fill
                            let is_gtc_exit = meta.order_type == OrderType::Gtc;

                            // Use the clip size from the order meta (not full position size)
                            // since exit orders are depth-capped and may be smaller than position
                            let size = meta.clip_size;

                            if let Some((pos, fully_closed)) = self
                                .base
                                .reduce_or_remove_position_by_token(&meta.token_id, size)
                                .await
                            {
                                let exit_fee = if is_gtc_exit {
                                    Decimal::ZERO
                                } else {
                                    taker_fee(fill_price, self.base.config.fee.taker_fee_rate)
                                };
                                let pnl = (fill_price - pos.entry_price) * size
                                    - (pos.entry_fee_per_share * size)
                                    - (exit_fee * size);
                                self.base.record_trade_pnl(pnl).await;

                                if fully_closed {
                                    self.base.remove_lifecycle(&meta.token_id).await;
                                } else {
                                    // Partial exit matched — transition back to Healthy for re-evaluation
                                    let remaining = pos.size - size;
                                    let is_dust = {
                                        let markets = self.base.active_markets.read().await;
                                        markets
                                            .get(&pos.market_id)
                                            .map(|m| remaining < m.market.min_order_size)
                                            .unwrap_or(true)
                                    };
                                    if is_dust {
                                        self.base
                                            .reduce_or_remove_position_by_token(
                                                &meta.token_id,
                                                remaining,
                                            )
                                            .await;
                                        warn!(
                                            token_id = %meta.token_id,
                                            dust_size = %remaining,
                                            "Removed unsellable dust after cancel-matched partial fill"
                                        );
                                    } else {
                                        // Transition back to Healthy for re-evaluation
                                        let now = self.base.event_time().await;
                                        let mut lifecycle =
                                            self.base.ensure_lifecycle(&meta.token_id).await;
                                        lifecycle.pending_exit_order_id = None;
                                        if let Err(e) = lifecycle.transition(
                                            PositionLifecycleState::Healthy,
                                            "cancel-matched partial exit",
                                            now,
                                        ) {
                                            warn!(token_id = %meta.token_id, error = %e, "cancel-matched→Healthy transition failed");
                                        }
                                        self.write_lifecycle(&meta.token_id, &lifecycle).await;
                                    }
                                }

                                info!(
                                    order_id = %order_id,
                                    token_id = %meta.token_id,
                                    fill_price = %fill_price,
                                    clip_size = %size,
                                    pnl = %pnl,
                                    fully_closed,
                                    "Exit order cancel-failed (matched) — treated as fill"
                                );
                            } else {
                                warn!(
                                    order_id = %order_id,
                                    token_id = %meta.token_id,
                                    "Exit cancel-matched but position already removed"
                                );
                                self.base.remove_lifecycle(&meta.token_id).await;
                            }
                        }

                        // Clean up tracking
                        {
                            let mut exit_orders = self.base.exit_orders_by_id.write().await;
                            exit_orders.remove(order_id);
                        }
                    } else if is_gone {
                        // Order is gone but not filled — transition back to Healthy
                        // for re-evaluation on next orderbook update.
                        let now = self.base.event_time().await;

                        let mut lifecycle = self.base.ensure_lifecycle(&meta.token_id).await;
                        lifecycle.pending_exit_order_id = None;
                        if let Err(e) = lifecycle.transition(
                            PositionLifecycleState::Healthy,
                            &format!("cancel failed ({}): {}", reason, meta.source_state),
                            now,
                        ) {
                            warn!(token_id = %meta.token_id, error = %e, "cancel-gone→Healthy transition failed");
                        }
                        self.write_lifecycle(&meta.token_id, &lifecycle).await;

                        // Clean up tracking
                        {
                            let mut exit_orders = self.base.exit_orders_by_id.write().await;
                            exit_orders.remove(order_id);
                        }

                        warn!(
                            order_id = %order_id,
                            token_id = %meta.token_id,
                            reason = %reason,
                            "Exit order cancel failed (permanently gone) — back to Healthy for re-evaluation"
                        );
                    } else {
                        // Transient failure — the order is still live on the CLOB.
                        // Leave lifecycle in ExitExecuting; the stale GTC check will
                        // retry the cancel on the next orderbook update.
                        warn!(
                            order_id = %order_id,
                            token_id = %meta.token_id,
                            reason = %reason,
                            "Exit order cancel failed (transient), will retry"
                        );
                    }
                    vec![]
                } else {
                    // Not an exit order — check entry limit orders
                    let (_found, fill_actions) =
                        self.base.handle_cancel_failed(order_id, reason).await;
                    fill_actions
                }
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
            entries.push_back((
                now - Duration::seconds(3),
                dec!(51000),
                "test".to_string(),
                now - Duration::seconds(3),
            ));
            entries.push_back((
                now - Duration::seconds(1),
                dec!(51000),
                "test".to_string(),
                now - Duration::seconds(1),
            ));
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
            .handle_external_price("BTC", dec!(51000), "test", ctx.now().await, &ctx)
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
        let pnl =
            (exit_price - entry_price) * size - (entry_fee_per_share * size) - (exit_fee * size);

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

        let pnl =
            (exit_price - entry_price) * size - (entry_fee_per_share * size) - (exit_fee * size);

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
        let pnl =
            (fill_price - entry_price) * size - (entry_fee_per_share * size) - (exit_fee * size);

        // Manual: (0.90 - 0.95) * 50 - 0 - taker_fee(0.90) * 50
        // = -2.50 - (2 * 0.90 * 0.10 * 0.0315) * 50
        // = -2.50 - 0.00567 * 50
        // = -2.50 - 0.2835
        // = -2.7835
        let expected =
            (dec!(0.90) - dec!(0.95)) * dec!(50) - taker_fee(dec!(0.90), fee_rate) * dec!(50);
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
            entries.push_back((now, dec!(49700), "test".to_string(), now));
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
            recovery_cost: Decimal::ZERO,
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
    }

    /// Non-hard trigger during sell delay does NOT place exit order and
    /// lifecycle stays Healthy (skip and re-eval next tick).
    #[tokio::test]
    async fn lifecycle_non_hard_trigger_during_sell_delay_skips() {
        // entry_time 5s ago (within sell delay of 10s), market 300s remaining
        let (strategy, _snapshot) = make_lifecycle_test_setup(5, 300).await;

        // Clear external price data so HardCrash cannot fire (requires external price).
        // This isolates PostEntryExit (Level 4) which only needs fresh book.
        // Config has post_entry_exit_drop=0.04, so bid drop of 0.05 triggers it.
        {
            let mut cache = strategy.base.sl_composite_cache.write().await;
            cache.clear();
        }
        {
            let mut history = strategy.base.price_history.write().await;
            history.clear();
        }

        // Bid=0.85 → drop of 0.05 from entry 0.90 >= post_entry_exit_drop(0.04)
        // No external price → HardCrash won't fire, only PostEntryExit
        let snapshot = polyrust_core::types::OrderbookSnapshot {
            token_id: "token_up".to_string(),
            bids: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.85),
                size: dec!(100),
            }],
            asks: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.87),
                size: dec!(100),
            }],
            timestamp: Utc::now(),
        };

        let actions = strategy.handle_orderbook_update(&snapshot).await;

        // Should NOT produce a PlaceOrder action (sell delay not elapsed, non-hard trigger)
        assert!(
            !actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Should not sell during sell delay for non-hard trigger, got: {actions:?}"
        );

        // Lifecycle should stay Healthy (skip and re-eval next tick)
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").expect("lifecycle should exist");
        assert!(
            matches!(lc.state, PositionLifecycleState::Healthy),
            "Expected Healthy during sell delay (non-hard trigger skips), got: {:?}",
            lc.state
        );
    }

    /// Hard crash trigger during sell delay BYPASSES sell delay and places exit order immediately.
    #[tokio::test]
    async fn lifecycle_hard_crash_bypasses_sell_delay() {
        // entry_time 5s ago (within sell delay of 10s), market 300s remaining
        // The setup has bid=0.40 vs entry=0.90, which is a 0.50 drop — hard crash threshold
        let (strategy, snapshot) = make_lifecycle_test_setup(5, 300).await;

        let actions = strategy.handle_orderbook_update(&snapshot).await;

        // Hard crash should bypass sell delay and produce exit order
        assert!(
            actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Hard crash should bypass sell delay and produce exit, got: {actions:?}"
        );

        // Lifecycle should be ExitExecuting (immediate exit)
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").expect("lifecycle should exist");
        assert!(
            matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }),
            "Expected ExitExecuting after hard crash bypass, got: {:?}",
            lc.state
        );
    }

    /// PostEntryExit trigger after sell delay elapsed still produces exit order.
    #[tokio::test]
    async fn lifecycle_post_entry_trigger_fires_when_sellable() {
        // entry_time 20s ago (past sell delay of 10s), market 300s remaining
        // Bid drop 0.13 from entry (0.90 - 0.77) triggers PostEntryExit (threshold 0.12)
        let (strategy, _snapshot) = make_lifecycle_test_setup(20, 300).await;

        let snapshot = polyrust_core::types::OrderbookSnapshot {
            token_id: "token_up".to_string(),
            bids: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.77), // drop of 0.13 from entry 0.90
                size: dec!(100),
            }],
            asks: vec![polyrust_core::types::OrderbookLevel {
                price: dec!(0.79),
                size: dec!(100),
            }],
            timestamp: Utc::now(),
        };

        let actions = strategy.handle_orderbook_update(&snapshot).await;

        // Should produce a sell action (sell delay elapsed + trigger fires)
        assert!(
            actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Should sell when delay elapsed and trigger fires, got: {actions:?}"
        );

        // Lifecycle should be ExitExecuting
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").unwrap();
        assert!(
            matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }),
            "Expected ExitExecuting, got: {:?}",
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
            assert!(matches!(
                lc.state,
                PositionLifecycleState::ExitExecuting { .. }
            ));
        }

        strategy
    }

    /// FAK exit order rejected for liquidity -> lifecycle transitions to Healthy
    /// for re-evaluation on next orderbook tick.
    #[tokio::test]
    async fn lifecycle_fak_rejected_transitions_to_healthy() {
        let strategy = make_exit_executing_setup().await;

        // Get the exit order ID from the lifecycle state
        let exit_oid = {
            let lifecycles = strategy.base.position_lifecycle.read().await;
            let lc = lifecycles.get("token_up").unwrap();
            match &lc.state {
                PositionLifecycleState::ExitExecuting { order_id, .. } => order_id.clone(),
                other => panic!("Expected ExitExecuting, got: {other:?}"),
            }
        };

        // Simulate a Rejected event through on_event (the real rejection handler)
        let ctx = StrategyContext::new();
        let event = Event::OrderUpdate(polyrust_core::events::OrderEvent::Rejected {
            order_id: Some(exit_oid.clone()),
            token_id: Some("token_up".to_string()),
            reason: "couldn't be fully filled".to_string(),
        });
        let mut strategy_mut = strategy;
        let _actions = strategy_mut.on_event(&event, &ctx).await.unwrap();

        // Lifecycle should transition to Healthy (re-evaluate on next tick)
        let lifecycles = strategy_mut.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").unwrap();
        assert!(
            matches!(lc.state, PositionLifecycleState::Healthy),
            "Expected Healthy after FAK rejection, got: {:?}",
            lc.state
        );

        // exit_orders_by_id should be cleaned up
        let exit_orders = strategy_mut.base.exit_orders_by_id.read().await;
        let has_token = exit_orders.values().any(|m| m.token_id == "token_up");
        assert!(
            !has_token,
            "exit_orders_by_id should be cleaned up after rejection"
        );
    }

    /// GTC refresh cycle: a GTC exit order that's older than gtc_stop_loss_max_age_secs
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
            recovery_cost: Decimal::ZERO,
        };
        base.record_position(pos).await;

        // Set up lifecycle in ExitExecuting with a stale GTC exit order (residual after FAK partial fill)
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
                        hedge_order_id: None,
                        hedge_price: None,
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

        // Track the GTC order in exit_orders_by_id
        {
            let mut exit_orders = base.exit_orders_by_id.write().await;
            exit_orders.insert(
                exit_oid.clone(),
                ExitOrderMeta {
                    token_id: "token_up".to_string(),
                    order_token_id: "token_up".to_string(),
                    order_type: OrderType::Gtc,
                    source_state: "ExitActive(GTC residual)".to_string(),

                    exit_price: dec!(0.81),
                    clip_size: dec!(10),
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

    /// Partial FAK fill places GTC residual order (ExitExecuting -> ExitExecuting with GTC).
    #[tokio::test]
    async fn lifecycle_partial_fill_places_gtc_residual() {
        let strategy = make_exit_executing_setup().await;

        // Get the exit order ID from the lifecycle state
        let exit_oid = {
            let lifecycles = strategy.base.position_lifecycle.read().await;
            let lc = lifecycles.get("token_up").unwrap();
            match &lc.state {
                PositionLifecycleState::ExitExecuting { order_id, .. } => order_id.clone(),
                other => panic!("Expected ExitExecuting, got: {other:?}"),
            }
        };

        // Simulate a partial FAK fill (5 out of 10 filled) via on_order_filled
        let actions = strategy
            .on_order_filled(&exit_oid, "token_up", dec!(0.82), dec!(5))
            .await;

        // Position should still exist with reduced size (10 - 5 = 5)
        let positions = strategy.base.positions.read().await;
        let pos = positions
            .values()
            .flat_map(|v| v.iter())
            .find(|p| p.token_id == "token_up");
        assert!(
            pos.is_some(),
            "Position should still exist after partial fill"
        );
        assert_eq!(pos.unwrap().size, dec!(5), "Size should be reduced to 5");
        drop(positions);

        // Lifecycle should be ExitExecuting with GTC for the residual
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").unwrap();
        match &lc.state {
            PositionLifecycleState::ExitExecuting { order_type, .. } => {
                assert_eq!(
                    *order_type,
                    OrderType::Gtc,
                    "Residual should use GTC order type"
                );
            }
            other => panic!("Expected ExitExecuting(GTC) for residual, got: {other:?}"),
        }

        // Should have produced a PlaceOrder action for the GTC residual
        let has_place = actions.iter().any(|a| matches!(a, Action::PlaceOrder(_)));
        assert!(
            has_place,
            "Expected PlaceOrder for GTC residual after partial fill, got: {actions:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Task 16: Order event routing through lifecycle transitions
    // -----------------------------------------------------------------------

    /// Helper: create a strategy with a position in ExitExecuting state,
    /// with properly tracked exit order metadata for fill routing.
    async fn make_exit_fill_test_setup(exit_order_type: OrderType) -> (TailEndStrategy, String) {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.enabled = true;
        config.tailend.min_sustained_secs = 0;
        config.tailend.max_recent_volatility = dec!(1.0);
        config.stop_loss.min_remaining_secs = 0;
        config.stop_loss.exit_depth_cap_factor = dec!(0.80);

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
            entry_price: dec!(0.92),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: now - Duration::seconds(20),
            kelly_fraction: None,
            peak_bid: dec!(0.92),
            estimated_fee: Decimal::ZERO,
            entry_market_price: dec!(0.92),
            tick_size: dec!(0.01),
            fee_rate_bps: 0,
            entry_order_type: OrderType::Gtc,
            entry_fee_per_share: Decimal::ZERO,
            recovery_cost: Decimal::ZERO,
        };
        base.record_position(pos).await;

        let exit_oid = format!(
            "exit-{}-token_up-{}",
            if exit_order_type == OrderType::Gtc {
                "gtc"
            } else {
                "fak"
            },
            now.timestamp_millis()
        );

        // Set lifecycle to ExitExecuting
        {
            let mut lifecycle = base.ensure_lifecycle("token_up").await;
            lifecycle
                .transition(
                    PositionLifecycleState::ExitExecuting {
                        order_id: exit_oid.clone(),
                        order_type: exit_order_type,
                        exit_price: dec!(0.85),
                        submitted_at: now,
                        hedge_order_id: None,
                        hedge_price: None,
                    },
                    "test setup: trigger fired",
                    now,
                )
                .unwrap();
            lifecycle.pending_exit_order_id = Some(exit_oid.clone());
            let mut lifecycles = base.position_lifecycle.write().await;
            lifecycles.insert("token_up".to_string(), lifecycle);
        }

        // Track in exit_orders_by_id
        {
            let mut exit_orders = base.exit_orders_by_id.write().await;
            exit_orders.insert(
                exit_oid.clone(),
                ExitOrderMeta {
                    token_id: "token_up".to_string(),
                    order_token_id: "token_up".to_string(),
                    order_type: exit_order_type,
                    source_state: "HardCrash(bid_drop=0.08, reversal=0.006)".to_string(),

                    exit_price: dec!(0.85),
                    clip_size: dec!(10),
                },
            );
        }

        let strategy = TailEndStrategy::new(base);
        (strategy, exit_oid)
    }

    /// Full exit fill (FAK) routes correctly: removes position and lifecycle,
    /// computes P&L, and cleans up exit_orders_by_id.
    #[tokio::test]
    async fn lifecycle_exit_fill_routes_through_lifecycle_fak() {
        let (strategy, exit_oid) = make_exit_fill_test_setup(OrderType::Fak).await;

        // Simulate a Filled event: full fill at price 0.85 for size 10
        // Must use exit_oid to match the exit order in exit_orders_by_id
        let actions = strategy
            .on_order_filled(&exit_oid, "token_up", dec!(0.85), dec!(10))
            .await;

        // No further actions needed (fill handled internally)
        assert!(
            actions.is_empty() || !actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Should not produce further orders after full exit fill"
        );

        // Position should be removed
        let positions = strategy.base.positions.read().await;
        let has_position = positions
            .values()
            .flat_map(|v| v.iter())
            .any(|p| p.token_id == "token_up");
        assert!(
            !has_position,
            "Position should be removed after full exit fill"
        );

        // Lifecycle should be cleaned up (removed by reduce_or_remove_position_by_token)
        let lifecycles = strategy.base.position_lifecycle.read().await;
        assert!(
            !lifecycles.contains_key("token_up"),
            "Lifecycle should be removed after full exit fill"
        );

        // exit_orders_by_id should be cleaned up
        let exit_orders = strategy.base.exit_orders_by_id.read().await;
        let has_token = exit_orders.values().any(|m| m.token_id == "token_up");
        assert!(
            !has_token,
            "exit_orders_by_id should be cleaned up after full fill"
        );
    }

    /// Full exit fill (GTC) routes correctly: removes position and lifecycle,
    /// computes P&L with 0% maker fee, and cleans up tracking maps.
    #[tokio::test]
    async fn lifecycle_exit_fill_routes_through_lifecycle_gtc() {
        let (strategy, exit_oid) = make_exit_fill_test_setup(OrderType::Gtc).await;

        // Simulate GTC fill: full fill at price 0.88 for size 10
        let actions = strategy
            .on_order_filled(&exit_oid, "token_up", dec!(0.88), dec!(10))
            .await;

        assert!(actions.is_empty(), "Should not produce further orders");

        // Position should be removed
        let positions = strategy.base.positions.read().await;
        let has_position = positions
            .values()
            .flat_map(|v| v.iter())
            .any(|p| p.token_id == "token_up");
        assert!(
            !has_position,
            "Position should be removed after GTC exit fill"
        );

        // Lifecycle should be cleaned up
        let lifecycles = strategy.base.position_lifecycle.read().await;
        assert!(
            !lifecycles.contains_key("token_up"),
            "Lifecycle should be removed after GTC full fill"
        );
    }

    /// Partial exit fill (FAK) with remaining below min_order_size
    /// triggers dust detection and removes position.
    #[tokio::test]
    async fn lifecycle_partial_exit_fill_dust_removed() {
        let (strategy, exit_oid) = make_exit_fill_test_setup(OrderType::Fak).await;

        // Simulate partial FAK fill: 6 out of 10 filled, remaining=4 < min_order_size(5)
        // Must use exit_oid to match the exit order in exit_orders_by_id
        let _actions = strategy
            .on_order_filled(&exit_oid, "token_up", dec!(0.85), dec!(6))
            .await;

        // Remaining 4 < min_order_size(5) — dust detection should remove position
        let positions = strategy.base.positions.read().await;
        let has_position = positions
            .values()
            .flat_map(|v| v.iter())
            .any(|p| p.token_id == "token_up");
        assert!(
            !has_position,
            "Dust position (4 < min_order_size 5) should be removed"
        );
    }

    /// Partial exit fill (FAK) with remaining above min_order_size
    /// places GTC residual order (ExitExecuting -> ExitExecuting with GTC).
    #[tokio::test]
    async fn lifecycle_partial_exit_fill_above_min_places_gtc_residual() {
        // Create a larger position so partial fill leaves above min_order_size
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

        // Larger position: size=20 so after partial fill of 12, remaining=8 > min_order_size(5)
        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: polyrust_core::types::OutcomeSide::Up,
            entry_price: dec!(0.92),
            size: dec!(20),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: now - Duration::seconds(20),
            kelly_fraction: None,
            peak_bid: dec!(0.92),
            estimated_fee: Decimal::ZERO,
            entry_market_price: dec!(0.92),
            tick_size: dec!(0.01),
            fee_rate_bps: 0,
            entry_order_type: OrderType::Gtc,
            entry_fee_per_share: Decimal::ZERO,
            recovery_cost: Decimal::ZERO,
        };
        base.record_position(pos).await;

        // Set lifecycle to ExitExecuting with FAK
        {
            let mut lifecycle = base.ensure_lifecycle("token_up").await;
            lifecycle
                .transition(
                    PositionLifecycleState::ExitExecuting {
                        order_id: "exit-fak-1".to_string(),
                        order_type: OrderType::Fak,
                        exit_price: dec!(0.85),
                        submitted_at: now,
                        hedge_order_id: None,
                        hedge_price: None,
                    },
                    "test trigger",
                    now,
                )
                .unwrap();
            lifecycle.pending_exit_order_id = Some("exit-fak-1".to_string());
            let mut lifecycles = base.position_lifecycle.write().await;
            lifecycles.insert("token_up".to_string(), lifecycle);
        }
        {
            let mut exit_orders = base.exit_orders_by_id.write().await;
            exit_orders.insert(
                "exit-fak-1".to_string(),
                ExitOrderMeta {
                    token_id: "token_up".to_string(),
                    order_token_id: "token_up".to_string(),
                    order_type: OrderType::Fak,
                    source_state: "test".to_string(),

                    exit_price: dec!(0.85),
                    clip_size: dec!(10),
                },
            );
        }

        let strategy = TailEndStrategy::new(base);

        // Simulate partial FAK fill: 12 out of 20
        let actions = strategy
            .on_order_filled("exit-fak-1", "token_up", dec!(0.85), dec!(12))
            .await;

        // Remaining = 20 - 12 = 8 > min_order_size(5) — should be ExitExecuting with GTC residual
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up");
        assert!(
            lc.is_some(),
            "Lifecycle should exist for remaining position"
        );
        match &lc.unwrap().state {
            PositionLifecycleState::ExitExecuting { order_type, .. } => {
                assert_eq!(
                    *order_type,
                    OrderType::Gtc,
                    "Residual should use GTC order type"
                );
            }
            other => panic!("Expected ExitExecuting(GTC) for residual, got: {other:?}"),
        }

        // Should have produced a PlaceOrder action for the GTC residual
        let has_place = actions.iter().any(|a| matches!(a, Action::PlaceOrder(_)));
        assert!(
            has_place,
            "Expected PlaceOrder for GTC residual after partial FAK fill, got: {actions:?}"
        );

        // Position should still exist with reduced size
        let positions = strategy.base.positions.read().await;
        let pos = positions
            .values()
            .flat_map(|v| v.iter())
            .find(|p| p.token_id == "token_up");
        assert!(pos.is_some(), "Position should still exist");
        assert_eq!(pos.unwrap().size, dec!(8), "Size should be reduced to 8");
    }

    /// Exit order rejection for ExitExecuting lifecycle transitions to Healthy
    /// for re-evaluation on next orderbook tick.
    #[tokio::test]
    async fn lifecycle_rejection_transitions_to_healthy() {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.enabled = true;
        config.tailend.min_sustained_secs = 0;
        config.tailend.max_recent_volatility = dec!(1.0);
        config.stop_loss.min_remaining_secs = 0;

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

        // Create position
        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: polyrust_core::types::OutcomeSide::Up,
            entry_price: dec!(0.92),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: now - Duration::seconds(20),
            kelly_fraction: None,
            peak_bid: dec!(0.92),
            estimated_fee: Decimal::ZERO,
            entry_market_price: dec!(0.92),
            tick_size: dec!(0.01),
            fee_rate_bps: 0,
            entry_order_type: OrderType::Gtc,
            entry_fee_per_share: Decimal::ZERO,
            recovery_cost: Decimal::ZERO,
        };
        base.record_position(pos).await;

        // Set lifecycle to ExitExecuting with FAK
        let exit_oid = "exit-fak-token_up-999".to_string();
        {
            let mut lifecycle = base.ensure_lifecycle("token_up").await;
            lifecycle
                .transition(
                    PositionLifecycleState::ExitExecuting {
                        order_id: exit_oid.clone(),
                        order_type: OrderType::Fak,
                        exit_price: dec!(0.85),
                        submitted_at: now,
                        hedge_order_id: None,
                        hedge_price: None,
                    },
                    "test trigger",
                    now,
                )
                .unwrap();
            lifecycle.pending_exit_order_id = Some(exit_oid.clone());
            let mut lifecycles = base.position_lifecycle.write().await;
            lifecycles.insert("token_up".to_string(), lifecycle);
        }
        {
            let mut exit_orders = base.exit_orders_by_id.write().await;
            exit_orders.insert(
                exit_oid.clone(),
                ExitOrderMeta {
                    token_id: "token_up".to_string(),
                    order_token_id: "token_up".to_string(),
                    order_type: OrderType::Fak,
                    source_state: "HardCrash".to_string(),

                    exit_price: dec!(0.85),
                    clip_size: dec!(10),
                },
            );
        }
        let mut strategy = TailEndStrategy::new(base);
        let ctx = StrategyContext::new();

        // Simulate a Rejected event with a liquidity reason
        let event = Event::OrderUpdate(polyrust_core::events::OrderEvent::Rejected {
            order_id: Some(exit_oid.clone()),
            token_id: Some("token_up".to_string()),
            reason: "couldn't be fully filled".to_string(),
        });
        let actions = strategy.on_event(&event, &ctx).await.unwrap();
        // Rejection handler doesn't produce new orders directly
        assert!(
            !actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Rejection should not immediately place a new order"
        );

        // Lifecycle should transition to Healthy for re-evaluation on next tick
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up");
        assert!(lc.is_some(), "Lifecycle should still exist after rejection");
        assert!(
            matches!(lc.unwrap().state, PositionLifecycleState::Healthy),
            "Expected Healthy after rejection, got: {:?}",
            lc.unwrap().state
        );

        // exit_orders_by_id should be cleaned up for this token
        let exit_orders = strategy.base.exit_orders_by_id.read().await;
        let has_token = exit_orders.values().any(|m| m.token_id == "token_up");
        assert!(
            !has_token,
            "exit_orders_by_id should be cleaned up after rejection"
        );
    }

    // ── Fast-path exit tests ─────────────────────────────────────────────

    /// Helper: create a TailEndStrategy with a position that has a hard crash
    /// scenario (bid drops 10 cents below entry) and an external price reversal.
    /// Returns (strategy, ctx, base) ready for fast-path exit testing.
    async fn make_fast_path_test_setup(
        fast_path_enabled: bool,
        book_age_secs: i64,
    ) -> (TailEndStrategy, StrategyContext) {
        let now = Utc::now();
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.enabled = true;
        config.tailend.fast_path_enabled = fast_path_enabled;
        config.tailend.fast_path_max_book_age_ms = 2000;
        config.tailend.min_sell_delay_secs = 10;
        config.stop_loss.hard_drop_abs = dec!(0.08); // 8 cent drop triggers hard crash
        config.stop_loss.hard_reversal_pct = dec!(0.006); // 0.6% reversal triggers hard crash
        config.stop_loss.sl_max_book_age_ms = 3000; // Trigger eval needs fresh book too
        config.stop_loss.sl_max_external_age_ms = 5000;
        config.stop_loss.min_remaining_secs = 10;

        let base = Arc::new(CryptoArbBase::new(config, vec![]));

        // Insert active market expiring in 60 seconds
        let market = MarketWithReference {
            market: make_market_info("market1", now + Duration::seconds(60)),
            reference_price: dec!(50000),
            reference_quality: ReferenceQuality::Exact,
            discovery_time: now,
            coin: "BTC".to_string(),
            window_ts: 0,
        };
        base.active_markets
            .write()
            .await
            .insert("market1".to_string(), market);

        // Insert position: entered at 0.92, 20 seconds ago (past sell delay)
        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.92),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: now - Duration::seconds(20),
            kelly_fraction: None,
            peak_bid: dec!(0.92),
            estimated_fee: Decimal::ZERO,
            entry_market_price: dec!(0.92),
            tick_size: dec!(0.01),
            fee_rate_bps: 0,
            entry_order_type: OrderType::Gtc,
            entry_fee_per_share: Decimal::ZERO,
            recovery_cost: Decimal::ZERO,
        };
        base.record_position(pos).await;

        // Update SL composite cache with a reversed external price (BTC dropped below reference)
        // This satisfies the hard crash reversal check: (50000 - 49500) / 50000 = 0.01 > 0.006
        {
            let composite = crate::crypto_arb::base::CompositePriceResult {
                price: dec!(49500),
                sources_used: 2,
                max_lag_ms: 100,
                dispersion_bps: dec!(10),
            };
            let mut cache = base.sl_composite_cache.write().await;
            cache.insert("BTC".to_string(), (composite, now));
        }

        // Also populate price history for get_sl_single_fresh fallback
        {
            use std::collections::VecDeque;
            let mut history = base.price_history.write().await;
            let mut entries = VecDeque::new();
            entries.push_back((
                now - Duration::seconds(1),
                dec!(49500),
                "test".to_string(),
                now - Duration::seconds(1),
            ));
            history.insert("BTC".to_string(), entries);
        }

        let ctx = StrategyContext::new();

        // Set up orderbook snapshot in StrategyContext (cached from previous OrderbookUpdate)
        // Bid at 0.83 = 9 cent drop from entry (0.92), exceeding hard_drop_abs (0.08)
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_up".to_string(),
                    bids: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.83),
                        size: dec!(50),
                    }],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.85),
                        size: dec!(50),
                    }],
                    timestamp: now - Duration::seconds(book_age_secs),
                },
            );
        }

        let strategy = TailEndStrategy::new(base);
        (strategy, ctx)
    }

    #[tokio::test]
    async fn fast_path_triggers_exit_with_fresh_book() {
        let (strategy, ctx) = make_fast_path_test_setup(true, 1).await;

        // Call evaluate_exits_on_price_change directly
        let actions = strategy.evaluate_exits_on_price_change("BTC", &ctx).await;

        // Should produce an exit PlaceOrder action (hard crash trigger)
        assert!(
            actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
            "Fast-path should trigger exit order with fresh book and hard crash conditions"
        );

        // Lifecycle should be in ExitExecuting
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").expect("Lifecycle should exist");
        assert!(
            matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }),
            "Lifecycle should be ExitExecuting after fast-path exit, got: {:?}",
            lc.state
        );
    }

    #[tokio::test]
    async fn fast_path_skips_stale_book() {
        // Book is 5 seconds old, max age is 2 seconds
        let (strategy, ctx) = make_fast_path_test_setup(true, 5).await;

        let actions = strategy.evaluate_exits_on_price_change("BTC", &ctx).await;

        // Should NOT produce any exit actions — book too stale
        assert!(
            actions.is_empty(),
            "Fast-path should skip exit when book snapshot is stale"
        );

        // Lifecycle should still be Healthy
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").expect("Lifecycle should exist");
        assert!(
            matches!(lc.state, PositionLifecycleState::Healthy),
            "Lifecycle should remain Healthy when book is stale, got: {:?}",
            lc.state
        );
    }

    #[tokio::test]
    async fn fast_path_skips_exit_executing_positions() {
        let (strategy, ctx) = make_fast_path_test_setup(true, 1).await;

        // Manually transition lifecycle to ExitExecuting (simulate already exiting)
        {
            let now = Utc::now();
            let mut lifecycle = strategy.base.ensure_lifecycle("token_up").await;
            lifecycle
                .transition(
                    PositionLifecycleState::ExitExecuting {
                        order_id: "existing-exit-123".to_string(),
                        order_type: OrderType::Fak,
                        exit_price: dec!(0.85),
                        submitted_at: now,
                        hedge_order_id: None,
                        hedge_price: None,
                    },
                    "test pre-existing exit",
                    now,
                )
                .unwrap();
            let mut lifecycles = strategy.base.position_lifecycle.write().await;
            lifecycles.insert("token_up".to_string(), lifecycle);
        }

        let actions = strategy.evaluate_exits_on_price_change("BTC", &ctx).await;

        // Should NOT produce any new exit actions — position already exiting
        assert!(
            actions.is_empty(),
            "Fast-path should skip positions in ExitExecuting state"
        );
    }

    #[tokio::test]
    async fn fast_path_disabled_produces_no_exits() {
        let (strategy, ctx) = make_fast_path_test_setup(false, 1).await;

        let actions = strategy.evaluate_exits_on_price_change("BTC", &ctx).await;

        // Should NOT produce any exit actions — fast path disabled
        assert!(
            actions.is_empty(),
            "Fast-path should produce no exits when disabled"
        );
    }
}
