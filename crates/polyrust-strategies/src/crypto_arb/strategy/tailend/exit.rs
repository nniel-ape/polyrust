//! Exit evaluation: stop-loss triggers, exit order building, and orderbook-driven exits.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use polyrust_core::prelude::*;

use crate::crypto_arb::domain::{
    ArbitragePosition, ExitOrderMeta, PositionLifecycle, PositionLifecycleState,
    StopLossTriggerKind, TriggerEvalContext, compute_exit_clip,
};

use super::TailEndStrategy;

impl TailEndStrategy {
    /// Evaluate exit triggers on ExternalPrice events using cached orderbook
    /// snapshots. This "fast path" frontrunning gives 50-200ms advantage over
    /// waiting for the next OrderbookUpdate event.
    ///
    /// Only evaluates positions in `Healthy` state. Skips if:
    /// - `fast_path_enabled` is false
    /// - No cached orderbook snapshot exists for the position's token
    /// - Cached snapshot is older than `fast_path_max_book_age_ms`
    pub(crate) async fn evaluate_exits_on_price_change(
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
            let (external_price, external_age_ms, composite_sources) =
                self.get_sl_price_data(&pos.coin, sl_config, now).await;

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
    pub(crate) async fn handle_orderbook_update(&self, snapshot: &OrderbookSnapshot) -> Vec<Action> {
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
            let (external_price, external_age_ms, composite_sources) =
                self.get_sl_price_data(&pos.coin, sl_config, now).await;

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
    pub(super) async fn build_exit_order(
        &self,
        pos: &ArbitragePosition,
        current_bid: Decimal,
        snapshot: &OrderbookSnapshot,
        neg_risk: bool,
        min_order_size: Decimal,
        trigger_kind: &StopLossTriggerKind,
        lifecycle: &mut crate::crypto_arb::domain::PositionLifecycle,
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
    pub(super) async fn write_lifecycle(&self, token_id: &str, lifecycle: &PositionLifecycle) {
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

    /// Get stop-loss composite/external price data from cache.
    /// Returns (price, age_ms, sources) tuple.
    async fn get_sl_price_data(
        &self,
        coin: &str,
        sl_config: &crate::crypto_arb::config::StopLossConfig,
        now: DateTime<Utc>,
    ) -> (Option<Decimal>, Option<i64>, Option<usize>) {
        let max_age = sl_config.sl_max_external_age_ms * 2;

        // Try composite cache first
        let cache = self.base.sl_composite_cache.read().await;
        if let Some((composite, cached_at)) = cache.get(coin) {
            let age = now.signed_duration_since(*cached_at).num_milliseconds();
            if age <= max_age {
                return (
                    Some(composite.price),
                    Some(age),
                    Some(composite.sources_used),
                );
            }
        }
        drop(cache);

        // Composite missing or stale — fall back to single fresh source
        if let Some(single) = self
            .base
            .get_sl_single_fresh(coin, max_age, now)
            .await
        {
            let history = self.base.price_history.read().await;
            let age = history
                .get(coin)
                .and_then(|h| h.back())
                .map(|(.., source_ts)| {
                    now.signed_duration_since(*source_ts).num_milliseconds()
                })
                .unwrap_or(sl_config.sl_max_external_age_ms * 3);
            return (Some(single), Some(age), None);
        }
        (None, None, None)
    }
}
