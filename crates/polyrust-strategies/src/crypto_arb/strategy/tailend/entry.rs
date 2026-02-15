//! Entry evaluation: opportunity scoring, threshold logic, and external price handling.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use polyrust_core::prelude::*;

use crate::crypto_arb::domain::{ArbitrageOpportunity, PendingOrder};
use crate::crypto_arb::services::taker_fee;

use super::TailEndStrategy;

impl TailEndStrategy {
    /// Internal implementation of dynamic ask threshold.
    pub(super) fn get_ask_threshold_impl(&self, time_remaining_secs: i64) -> Decimal {
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

    /// Handle an external price update: record price, scan near-expiry markets,
    /// evaluate tail-end opportunities, and submit GTC orders.
    pub(super) async fn handle_external_price(
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
}
