//! Order tracking, cooldowns, reconciliation, and cancel-failure handling.

use std::collections::HashSet;

use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use polyrust_core::prelude::*;

use crate::crypto_arb::domain::ArbitragePosition;
use crate::crypto_arb::runtime::CryptoArbRuntime;

impl CryptoArbRuntime {
    // -------------------------------------------------------------------------
    // Market reservations
    // -------------------------------------------------------------------------

    /// Atomically check exposure + position limits and reserve a market for trading.
    ///
    /// Returns `true` if the reservation succeeded (no existing exposure,
    /// position limit not exceeded). The reservation prevents concurrent
    /// entry into the same market.
    pub async fn try_reserve_market(&self, market_id: &MarketId, slot_count: usize) -> bool {
        // Acquire all locks in a consistent order to prevent deadlocks
        let positions = self.positions.read().await;
        let pending = self.pending_orders.read().await;
        let limits = self.open_limit_orders.read().await;
        let mut reservations = self.market_reservations.write().await;

        // Check no existing exposure (same logic as has_market_exposure, inline)
        if positions.contains_key(market_id)
            || pending.values().any(|p| &p.market_id == market_id)
            || limits.values().any(|lo| &lo.market_id == market_id)
            || reservations.contains_key(market_id)
        {
            return false;
        }

        // Check position limit (reservations track slot counts)
        let total_positions: usize = positions.values().map(|v| v.len()).sum();
        let reserved_slots: usize = reservations.values().sum();
        let total = total_positions + pending.len() + limits.len() + reserved_slots;
        if total + slot_count > self.config.max_positions {
            return false;
        }

        reservations.insert(market_id.clone(), slot_count);
        true
    }

    /// Release a market reservation (called on early-exit paths before order placement).
    pub async fn release_reservation(&self, market_id: &MarketId) {
        let mut reservations = self.market_reservations.write().await;
        reservations.remove(market_id);
    }

    /// Consume a market reservation (called just before inserting into pending_orders).
    /// This transfers the "slot" from reservations to pending_orders atomically.
    pub async fn consume_reservation(&self, market_id: &MarketId) {
        let mut reservations = self.market_reservations.write().await;
        reservations.remove(market_id);
    }

    // -------------------------------------------------------------------------
    // Cooldowns
    // -------------------------------------------------------------------------

    /// Record a rejection cooldown for a market.
    pub async fn record_rejection_cooldown(&self, market_id: &MarketId, cooldown_secs: u64) {
        let now = self.event_time().await;
        let expires_at = now + chrono::Duration::seconds(cooldown_secs as i64);
        let mut cooldowns = self.rejection_cooldowns.write().await;
        cooldowns.insert(market_id.clone(), expires_at);
    }

    /// Check if a market is still in rejection cooldown.
    pub async fn is_rejection_cooled_down(&self, market_id: &MarketId) -> bool {
        let now = self.event_time().await;
        let cooldowns = self.rejection_cooldowns.read().await;
        if let Some(expires_at) = cooldowns.get(market_id) {
            now < *expires_at
        } else {
            false
        }
    }

    /// Record a stale market cooldown to prevent re-entry after position removal.
    pub async fn record_stale_market_cooldown(&self, market_id: &MarketId, cooldown_secs: u64) {
        let now = self.event_time().await;
        let expires_at = now + chrono::Duration::seconds(cooldown_secs as i64);
        let mut cooldowns = self.stale_market_cooldowns.write().await;
        cooldowns.insert(market_id.clone(), expires_at);
    }

    /// Check if a market is still in stale-removal cooldown.
    pub async fn is_stale_market_cooled_down(&self, market_id: &MarketId) -> bool {
        let now = self.event_time().await;
        let cooldowns = self.stale_market_cooldowns.read().await;
        if let Some(expires_at) = cooldowns.get(market_id) {
            now < *expires_at
        } else {
            false
        }
    }

    /// Record a recovery exit cooldown to prevent same-side re-entry too quickly.
    pub async fn record_recovery_exit_cooldown(&self, market_id: &MarketId) {
        let now = self.event_time().await;
        let expires_at =
            now + chrono::Duration::seconds(self.config.stop_loss.recovery_cooldown_secs as i64);
        let mut cooldowns = self.recovery_exit_cooldowns.write().await;
        cooldowns.insert(market_id.clone(), expires_at);
    }

    /// Check if a market is still in recovery exit cooldown (preventing re-entry).
    pub async fn is_recovery_exit_cooled_down(&self, market_id: &MarketId) -> bool {
        let now = self.event_time().await;
        let cooldowns = self.recovery_exit_cooldowns.read().await;
        if let Some(expires_at) = cooldowns.get(market_id) {
            now < *expires_at
        } else {
            false
        }
    }

    // -------------------------------------------------------------------------
    // Cancel failure handling
    // -------------------------------------------------------------------------

    /// Handle a CancelFailed event for a limit order.
    ///
    /// If the reason indicates the order is permanently gone (matched/canceled/not found),
    /// remove it from `open_limit_orders` to prevent retry loops. Otherwise, reset
    /// `cancel_pending` so the stale-order check can retry later.
    ///
    /// Returns `(found, actions)` — `found` is true if the order was in our tracking,
    /// and `actions` contains a matched-fill signal if the order was matched by a
    /// counterparty (so the claim monitor can track the position).
    pub async fn handle_cancel_failed(&self, order_id: &str, reason: &str) -> (bool, Vec<Action>) {
        let mut limits = self.open_limit_orders.write().await;
        if let Some(lo) = limits.get_mut(order_id) {
            let permanently_gone = reason.contains("matched")
                || reason.contains("canceled")
                || reason.contains("not found");
            if permanently_gone {
                let lo = limits.remove(order_id).unwrap();
                warn!(
                    order_id = %order_id,
                    market = %lo.market_id,
                    reason = %reason,
                    "Order permanently gone — removed from tracking"
                );

                let mut actions = Vec::new();
                if reason.contains("matched") {
                    info!(
                        order_id = %order_id,
                        market = %lo.market_id,
                        "Detected matched fill from cancel failure — creating position"
                    );
                    let now = self.event_time().await;
                    let position = ArbitragePosition::from_limit_order(
                        &lo,
                        lo.price,
                        lo.size,
                        Some(order_id.to_string()),
                        now,
                    );
                    self.record_position(position).await;
                    // Emit RecordFill so the persistence handler records this trade.
                    // Matched fills are always entry buys (GTC maker = 0 fee).
                    actions.push(Action::RecordFill {
                        order_id: order_id.to_string(),
                        market_id: lo.market_id.clone(),
                        token_id: lo.token_id.clone(),
                        side: OrderSide::Buy,
                        price: lo.price,
                        size: lo.size,
                        realized_pnl: None,
                        fee: Some(Decimal::ZERO),
                        order_type: Some("Gtc".to_string()),
                        orderbook_snapshot: None,
                    });
                    // Also emit signal for dashboard/logging consumers
                    actions.push(Action::EmitSignal {
                        signal_type: "matched-fill".to_string(),
                        payload: serde_json::json!({
                            "order_id": order_id,
                            "market_id": lo.market_id,
                            "token_id": lo.token_id,
                        }),
                    });
                }
                return (true, actions);
            } else {
                lo.cancel_pending = false;
                warn!(
                    order_id = %order_id,
                    market = %lo.market_id,
                    reason = %reason,
                    "Cancel failed (transient), will retry"
                );
            }
            return (true, vec![]);
        }
        (false, vec![])
    }

    // -------------------------------------------------------------------------
    // Reconciliation
    // -------------------------------------------------------------------------

    /// Reconcile tracked limit orders against the CLOB's actual open order set.
    ///
    /// Orders in `open_limit_orders` that are NOT in `clob_open_ids` (and not
    /// already cancel_pending) are treated as potentially filled. However, a
    /// single miss could be a transient API snapshot gap, so we require **2
    /// consecutive misses** before creating a synthetic fill position.
    ///
    /// - First miss: increment `reconcile_miss_count`, log warning, skip
    /// - Second+ miss: proceed with synthetic fill (position + RecordFill)
    /// - Order reappears in snapshot: reset `reconcile_miss_count` to 0
    ///
    /// Returns actions (signals) for each confirmed fill.
    pub async fn reconcile_limit_orders(&self, clob_open_ids: &HashSet<String>) -> Vec<Action> {
        let mut limits = self.open_limit_orders.write().await;
        let mut confirmed_fills = Vec::new();
        let now = self.event_time().await;

        // Phase 1: Update miss counters and reset orders that reappeared
        let all_oids: Vec<String> = limits.keys().cloned().collect();
        for oid in &all_oids {
            let lo = limits.get_mut(oid).unwrap();
            if lo.cancel_pending {
                continue;
            }
            if clob_open_ids.contains(oid) {
                // Order is still on the book — reset miss counter
                if lo.reconcile_miss_count > 0 {
                    debug!(
                        order_id = %oid,
                        prev_misses = lo.reconcile_miss_count,
                        "Order reappeared in CLOB snapshot, resetting miss counter"
                    );
                    lo.reconcile_miss_count = 0;
                }
            } else {
                // Order missing from snapshot
                lo.reconcile_miss_count += 1;
                if lo.reconcile_miss_count < 2 {
                    warn!(
                        order_id = %oid,
                        market = %lo.market_id,
                        token = %lo.token_id,
                        miss_count = lo.reconcile_miss_count,
                        "Order missing from CLOB snapshot (miss {}/2), deferring reconciliation",
                        lo.reconcile_miss_count
                    );
                }
            }
        }

        // Phase 2: Collect confirmed misses (miss_count >= 2) for synthetic fill
        let confirmed_oids: Vec<String> = limits
            .iter()
            .filter(|(_, lo)| !lo.cancel_pending && lo.reconcile_miss_count >= 2)
            .map(|(oid, _)| oid.clone())
            .collect();

        for order_id in confirmed_oids {
            let lo = limits.remove(&order_id).unwrap();
            info!(
                order_id = %order_id,
                market = %lo.market_id,
                token = %lo.token_id,
                price = %lo.price,
                size = %lo.size,
                miss_count = lo.reconcile_miss_count,
                "Reconciled fill: order confirmed missing from CLOB after {} snapshots",
                lo.reconcile_miss_count
            );

            let position = ArbitragePosition::from_limit_order(
                &lo,
                lo.price,
                lo.size,
                Some(order_id.clone()),
                now,
            );
            confirmed_fills.push((position, order_id, lo));
        }
        drop(limits);

        let mut result_actions = Vec::new();
        for (position, order_id, lo) in confirmed_fills {
            self.record_position(position).await;
            // Emit RecordFill so the persistence handler records this trade.
            // Reconciled fills are always entry buys (GTC maker = 0 fee).
            result_actions.push(Action::RecordFill {
                order_id: order_id.clone(),
                market_id: lo.market_id.clone(),
                token_id: lo.token_id.clone(),
                side: OrderSide::Buy,
                price: lo.price,
                size: lo.size,
                realized_pnl: None,
                fee: Some(Decimal::ZERO),
                order_type: Some("Gtc".to_string()),
                orderbook_snapshot: None,
            });
            // Also emit signal for dashboard/logging consumers
            result_actions.push(Action::EmitSignal {
                signal_type: "reconciled-fill".to_string(),
                payload: serde_json::json!({
                    "order_id": order_id,
                    "market_id": lo.market_id,
                    "token_id": lo.token_id,
                    "price": lo.price.to_string(),
                    "size": lo.size.to_string(),
                    "side": format!("{:?}", lo.side),
                }),
            });
        }

        result_actions
    }

    /// Cancel GTC limit orders that have been open longer than `max_age_secs`.
    ///
    /// Orders are flagged with `cancel_pending = true` rather than removed from
    /// the map. This ensures that if the cancel fails (e.g., order was already
    /// matched), the subsequent `OrderEvent::Filled` can still find the order
    /// and record the position correctly.
    pub async fn check_stale_limit_orders(&self) -> Vec<Action> {
        let max_age_secs = self.config.order.max_age_secs as i64;
        let now = self.event_time().await;

        let mut orders = self.open_limit_orders.write().await;
        let mut actions = Vec::new();
        for (order_id, lo) in orders.iter_mut() {
            if lo.cancel_pending {
                continue; // Already has a cancel in flight
            }
            let age_secs = (now - lo.placed_at).num_seconds();
            if age_secs >= max_age_secs {
                info!(
                    order_id = %order_id,
                    market = %lo.market_id,
                    age_secs = age_secs,
                    "Cancelling stale GTC limit order"
                );
                lo.cancel_pending = true;
                actions.push(Action::CancelOrder(order_id.clone()));
                // Track cancel in telemetry
                let mut telem = self.order_telemetry.lock().unwrap();
                telem.total_cancels += 1;
                *telem.cancel_before_fill.entry(lo.coin.clone()).or_insert(0) += 1;
            }
        }
        actions
    }
}
