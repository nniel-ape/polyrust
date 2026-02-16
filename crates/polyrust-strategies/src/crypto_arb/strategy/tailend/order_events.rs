//! Order event handlers: placed, filled, partially filled, rejected, cancelled, cancel-failed.

use rust_decimal::Decimal;
use tracing::{info, warn};

use polyrust_core::prelude::*;

use crate::crypto_arb::domain::{
    ArbitragePosition, ExitOrderMeta, OpenLimitOrder, PositionLifecycleState, StopLossRejectionKind,
};
use crate::crypto_arb::services::taker_fee;

use super::TailEndStrategy;

impl TailEndStrategy {
    /// Handle order placement result.
    ///
    /// For exit/recovery orders: re-keys `exit_orders_by_id` from the synthetic
    /// order ID (generated at submission) to the real CLOB order ID returned by
    /// the backend.  Without this, `on_order_filled` can never match the fill
    /// event to the exit order, and GTC cancel actions use a stale synthetic ID
    /// the backend doesn't recognise.
    pub(super) async fn on_order_placed(&self, result: &OrderResult) -> Vec<Action> {
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
                        let (position_token, is_hedge, meta_order_type) = {
                            let mut exit_orders = self.base.exit_orders_by_id.write().await;
                            if let Some(meta) = exit_orders.remove(&syn_key) {
                                let pt = meta.token_id.clone();
                                let hedge = meta.source_state.starts_with("Hedge");
                                let ot = meta.order_type;
                                exit_orders.insert(real_oid.clone(), meta);
                                (pt, hedge, ot)
                            } else {
                                (result.token_id.clone(), false, OrderType::Gtc)
                            }
                        };

                        // FAK/FOK orders are immediate — if the CLOB status is not
                        // "Filled", the order terminated with zero fill. Clean up
                        // the re-keyed entry and transition back to Healthy to
                        // prevent the lifecycle from getting permanently stuck.
                        let is_immediate =
                            matches!(meta_order_type, OrderType::Fak | OrderType::Fok);
                        let is_filled = result.status.as_deref() == Some("Filled");
                        if is_immediate && !is_filled && !is_hedge {
                            {
                                let mut exit_orders = self.base.exit_orders_by_id.write().await;
                                exit_orders.remove(real_oid);
                            }
                            let now = self.base.event_time().await;
                            let mut lifecycle = self.base.ensure_lifecycle(&position_token).await;
                            lifecycle.pending_exit_order_id = None;
                            let _ = lifecycle.transition(
                                PositionLifecycleState::Healthy,
                                &format!(
                                    "{:?} zero fill (status: {})",
                                    meta_order_type,
                                    result.status.as_deref().unwrap_or("unknown")
                                ),
                                now,
                            );
                            self.write_lifecycle(&position_token, &lifecycle).await;
                            warn!(
                                token_id = %position_token,
                                order_id = %real_oid,
                                status = ?result.status,
                                order_type = ?meta_order_type,
                                "Exit order got zero fill — back to Healthy for re-evaluation"
                            );
                            return vec![];
                        }

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
            peak_price: pending.price,
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

    /// Handle a fully filled order event (GTC entry fills, stop-loss sells, GTC SL fills).
    pub(crate) async fn on_order_filled(
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

                    // Capture hedge order_id BEFORE reduce_or_remove cleans up
                    // exit_orders_by_id on full close (via remove_lifecycle).
                    let pre_hedge_oid = {
                        let lifecycle = self.base.ensure_lifecycle(&meta.token_id).await;
                        if let PositionLifecycleState::ExitExecuting {
                            hedge_order_id: Some(ref h_oid),
                            ..
                        } = lifecycle.state
                        {
                            Some(h_oid.clone())
                        } else {
                            None
                        }
                    };

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
                                // Record cooldown to prevent rapid re-entry
                                self.base
                                    .record_recovery_exit_cooldown(&pos.market_id)
                                    .await;
                                warn!(
                                    token_id = %meta.token_id,
                                    dust_size = %remaining,
                                    "Removed unsellable dust after partial fill — will resolve at expiry"
                                );
                                // Dust removal fully closes the position (via
                                // remove_lifecycle inside reduce_or_remove).
                                // Cancel hedge if one was in flight.
                                if let Some(h_oid) = pre_hedge_oid {
                                    info!(
                                        token_id = %meta.token_id,
                                        hedge_order_id = %h_oid,
                                        "Dust removal — cancelling pending hedge"
                                    );
                                    return vec![Action::CancelOrder(h_oid)];
                                }
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

                                    // Replace original FAK exit entry with GTC residual.
                                    // Must remove the stale FAK entry to prevent
                                    // on_order_placed from matching it instead of
                                    // the GTC when the Placed event arrives.
                                    {
                                        let mut exit_orders =
                                            self.base.exit_orders_by_id.write().await;
                                        exit_orders.remove(order_id);
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

                        // If sell fully closed the position, cancel any pending hedge.
                        // Note: reduce_or_remove_position_by_token already called
                        // remove_lifecycle on full close, which cleaned up
                        // exit_orders_by_id. We use pre_hedge_oid (captured before
                        // the reduce call) to cancel the hedge on the CLOB.
                        if fully_closed {
                            // Record cooldown to prevent rapid re-entry after stop-loss exit
                            self.base
                                .record_recovery_exit_cooldown(&pos.market_id)
                                .await;

                            // Clean up current exit order tracking (may already
                            // be gone from remove_lifecycle, but safe to call)
                            {
                                let mut exit_orders = self.base.exit_orders_by_id.write().await;
                                exit_orders.remove(order_id);
                            }

                            info!(
                                token_id = %meta.token_id,
                                order_id = %order_id,
                                pnl = %pnl,
                                fill_size = %size,
                                exit_type = if is_gtc_exit { "GTC (0% fee)" } else { "FAK (taker fee)" },
                                "Exit order filled — position fully closed"
                            );

                            if let Some(h_oid) = pre_hedge_oid {
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

    /// Handle a partially filled order event.
    pub(super) async fn handle_partially_filled(
        &self,
        order_id: &str,
        filled_size: Decimal,
        remaining_size: Decimal,
    ) -> Vec<Action> {
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
                    .reduce_or_remove_position_by_token(&meta.token_id, filled_size)
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
                lo.size = remaining_size;
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

    /// Handle a rejected order event.
    pub(super) async fn handle_rejected(
        &self,
        token_id: Option<&str>,
        reason: &str,
    ) -> Result<Vec<Action>> {
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
                                .reduce_or_remove_position_by_token(position_token, remaining)
                                .await;
                        } else {
                            // Transition back to Healthy for re-evaluation on next tick
                            let mut lifecycle = self.base.ensure_lifecycle(position_token).await;
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
                                m.token_id == position_token && m.source_state.starts_with("Hedge")
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
                if let Some(ref em) = exit_meta
                    && em.source_state.starts_with("Hedge")
                {
                    // Hedge rejected — clear hedge tracking, continue sell-only
                    let mut lifecycle = self.base.ensure_lifecycle(position_token).await;
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
                            !(m.token_id == position_token && m.source_state.starts_with("Hedge"))
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
        Ok(vec![])
    }

    /// Handle a cancelled order event.
    pub(super) async fn handle_cancelled(&self, order_id: &str) -> Result<Vec<Action>> {
        // Check if this is a lifecycle-driven exit order cancel
        {
            let exit_meta = {
                let exit_orders = self.base.exit_orders_by_id.read().await;
                exit_orders.get(order_id).cloned()
            };
            if let Some(meta) = exit_meta {
                let is_hedge = meta.source_state.starts_with("Hedge");
                let now = self.base.event_time().await;

                if is_hedge {
                    // Hedge order cancelled — clear hedge tracking but keep
                    // the sell order active in ExitExecuting state.
                    let mut lifecycle = self.base.ensure_lifecycle(&meta.token_id).await;
                    if let PositionLifecycleState::ExitExecuting {
                        ref mut hedge_order_id,
                        ref mut hedge_price,
                        ..
                    } = lifecycle.state
                    {
                        *hedge_order_id = None;
                        *hedge_price = None;
                    }
                    self.write_lifecycle(&meta.token_id, &lifecycle).await;

                    {
                        let mut exit_orders = self.base.exit_orders_by_id.write().await;
                        exit_orders.remove(order_id);
                    }

                    info!(
                        order_id = %order_id,
                        token_id = %meta.token_id,
                        "Hedge order cancelled — sell exit continues"
                    );
                    return Ok(vec![]);
                }

                // Sell order cancel (GTC chase refresh): transition back
                // to Healthy and cancel any associated hedge order.
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
        Ok(vec![])
    }

    /// Handle a cancel-failed order event.
    pub(super) async fn handle_cancel_failed(&self, order_id: &str, reason: &str) -> Vec<Action> {
        // First check if this is a lifecycle exit/recovery order
        let exit_meta = {
            let exit_orders = self.base.exit_orders_by_id.read().await;
            exit_orders.get(order_id).cloned()
        };
        if let Some(meta) = exit_meta {
            let is_matched = reason.contains("matched");
            let is_gone = reason.contains("canceled") || reason.contains("not found");
            let mut exit_fill_actions: Vec<Action> = Vec::new();

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

                        let mut lifecycle = self.base.ensure_lifecycle(&meta.token_id).await;

                        // Cancel the sell order
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
                            return vec![Action::CancelOrder(sell_oid)];
                        }
                    } else {
                        warn!(
                            order_id = %order_id,
                            token_id = %meta.token_id,
                            "Hedge cancel-matched but clip_size is zero — cleaning up"
                        );
                        let mut lifecycle = self.base.ensure_lifecycle(&meta.token_id).await;
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

                        // Persist the cancel-matched exit fill so it survives restarts.
                        exit_fill_actions.push(Action::RecordFill {
                            order_id: order_id.to_string(),
                            market_id: pos.market_id.clone(),
                            token_id: meta.token_id.clone(),
                            side: OrderSide::Sell,
                            price: fill_price,
                            size,
                            realized_pnl: Some(pnl),
                            fee: Some(exit_fee * size),
                            order_type: Some(format!("{:?}", meta.order_type)),
                            orderbook_snapshot: None,
                        });

                        if !fully_closed {
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
                                    .reduce_or_remove_position_by_token(&meta.token_id, remaining)
                                    .await;
                                self.base
                                    .record_recovery_exit_cooldown(&pos.market_id)
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
                        } else {
                            // fully_closed: reduce_or_remove already called
                            // remove_lifecycle. Record cooldown to prevent rapid
                            // re-entry after stop-loss exit.
                            self.base
                                .record_recovery_exit_cooldown(&pos.market_id)
                                .await;
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
            exit_fill_actions
        } else {
            // Not an exit order — check entry limit orders
            let (_found, fill_actions) = self.base.handle_cancel_failed(order_id, reason).await;
            fill_actions
        }
    }

    /// Handle an open order snapshot for reconciliation.
    pub(super) async fn handle_open_order_snapshot(&self, ids: &[String]) -> Vec<Action> {
        let id_set: std::collections::HashSet<String> = ids.iter().cloned().collect();
        self.base.reconcile_limit_orders(&id_set).await
    }
}
