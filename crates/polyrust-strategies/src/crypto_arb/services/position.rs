//! Position management: CRUD, reservations, lifecycle state, P&L tracking.

use rust_decimal::Decimal;
use tracing::debug;

use polyrust_core::prelude::*;

use crate::crypto_arb::domain::{ArbitragePosition, PositionLifecycle};
use crate::crypto_arb::runtime::CryptoArbRuntime;

impl CryptoArbRuntime {
    // -------------------------------------------------------------------------
    // Position capacity
    // -------------------------------------------------------------------------

    /// Check if we can open a new position (respects max_positions limit).
    pub async fn can_open_position(&self) -> bool {
        let positions = self.positions.read().await;
        let pending = self.pending_orders.read().await;
        let limits = self.open_limit_orders.read().await;
        let reservations = self.market_reservations.read().await;

        let total_positions: usize = positions.values().map(|v| v.len()).sum();
        let reserved_slots: usize = reservations.values().sum();
        let total = total_positions + pending.len() + limits.len() + reserved_slots;

        total < self.config.max_positions
    }

    /// Validate that the calculated share size meets the market's minimum order size.
    ///
    /// Returns `true` if the size is valid (>= min_order_size), `false` otherwise.
    /// Logs a warning if the size is below minimum to help diagnose config issues.
    pub async fn validate_min_order_size(&self, market_id: &MarketId, size: Decimal) -> bool {
        let markets = self.active_markets.read().await;
        let market = match markets.get(market_id) {
            Some(m) => &m.market,
            None => return false, // Can't validate without market info
        };

        if size < market.min_order_size {
            debug!(
                market = %market_id,
                size = %size,
                min_order_size = %market.min_order_size,
                "Order size below market minimum - skipping"
            );
            false
        } else {
            true
        }
    }

    /// Check if market already has a position, pending order, open limit order,
    /// or active reservation.
    pub async fn has_market_exposure(&self, market_id: &MarketId) -> bool {
        let positions = self.positions.read().await;
        if positions.contains_key(market_id) {
            return true;
        }

        let pending = self.pending_orders.read().await;
        if pending.values().any(|p| &p.market_id == market_id) {
            return true;
        }

        let limits = self.open_limit_orders.read().await;
        if limits.values().any(|lo| &lo.market_id == market_id) {
            return true;
        }

        let reservations = self.market_reservations.read().await;
        if reservations.contains_key(market_id) {
            return true;
        }

        false
    }

    // -------------------------------------------------------------------------
    // Position CRUD
    // -------------------------------------------------------------------------

    /// Record a new position and create its lifecycle state machine in Healthy state.
    pub async fn record_position(&self, pos: ArbitragePosition) {
        let token_id = pos.token_id.clone();
        let mut positions = self.positions.write().await;
        positions
            .entry(pos.market_id.clone())
            .or_default()
            .push(pos);
        drop(positions);
        self.ensure_lifecycle(&token_id).await;
    }

    /// Get or create a lifecycle entry for the given token_id.
    /// Returns a clone of the current lifecycle state.
    /// Creates a new Healthy lifecycle if none exists (handles migration of
    /// positions that existed before the lifecycle system was added).
    pub async fn ensure_lifecycle(&self, token_id: &str) -> PositionLifecycle {
        let mut lifecycles = self.position_lifecycle.write().await;
        lifecycles
            .entry(token_id.to_string())
            .or_insert_with(PositionLifecycle::new)
            .clone()
    }

    /// Remove the lifecycle entry for the given token_id.
    /// Called when a position is fully closed or expired.
    pub async fn remove_lifecycle(&self, token_id: &str) {
        let mut lifecycles = self.position_lifecycle.write().await;
        lifecycles.remove(token_id);
        // Also clean up any exit orders referencing this token
        let mut exit_orders = self.exit_orders_by_id.write().await;
        exit_orders.retain(|_, meta| meta.token_id != token_id);
    }

    /// Look up the opposite token_id for a given token in its market.
    ///
    /// In Polymarket, each market has two outcome tokens (outcome_a / outcome_b).
    /// Given one token, this returns the other. Returns `None` if the market
    /// isn't found or the token doesn't match either outcome.
    pub async fn get_opposite_token(&self, market_id: &str, token_id: &str) -> Option<TokenId> {
        let markets = self.active_markets.read().await;
        let mwr = markets.get(market_id)?;
        let ids = &mwr.market.token_ids;
        if token_id == ids.outcome_a {
            Some(ids.outcome_b.clone())
        } else if token_id == ids.outcome_b {
            Some(ids.outcome_a.clone())
        } else {
            None
        }
    }

    /// Remove a position by token_id across all markets, returning it.
    /// Also clears the stop-loss retry count for this token.
    pub async fn remove_position_by_token(&self, token_id: &str) -> Option<ArbitragePosition> {
        let removed = {
            let mut positions = self.positions.write().await;
            let mut removed = None;
            let mut empty_markets = Vec::new();

            for (market_id, pos_list) in positions.iter_mut() {
                if let Some(idx) = pos_list.iter().position(|p| p.token_id == token_id) {
                    removed = Some(pos_list.remove(idx));
                }
                if pos_list.is_empty() {
                    empty_markets.push(market_id.clone());
                }
            }

            for market_id in empty_markets {
                positions.remove(&market_id);
            }

            removed
        };

        // Clean up lifecycle when position is removed
        if removed.is_some() {
            self.remove_lifecycle(token_id).await;
        }

        removed
    }

    /// Reduce a position's size by `fill_size`, or remove it entirely if fully closed.
    ///
    /// Returns `(position_snapshot, was_fully_closed)`:
    /// - If `fill_size >= pos.size`: removes position entirely, clears stop-loss state
    /// - If `fill_size < pos.size`: reduces `pos.size` in-place, returns clone before reduction
    ///
    /// The returned snapshot always has the **original** size (before reduction) for P&L calculation.
    pub async fn reduce_or_remove_position_by_token(
        &self,
        token_id: &str,
        fill_size: Decimal,
    ) -> Option<(ArbitragePosition, bool)> {
        let result = {
            let mut positions = self.positions.write().await;
            let mut result = None;
            let mut empty_markets = Vec::new();

            for (market_id, pos_list) in positions.iter_mut() {
                if let Some(idx) = pos_list.iter().position(|p| p.token_id == token_id) {
                    let pos = &pos_list[idx];
                    if fill_size >= pos.size {
                        // Full close: remove entirely
                        let removed = pos_list.remove(idx);
                        result = Some((removed, true));
                    } else {
                        // Partial close: snapshot before reducing
                        let snapshot = pos.clone();
                        pos_list[idx].size -= fill_size;
                        result = Some((snapshot, false));
                    }
                }
                if pos_list.is_empty() {
                    empty_markets.push(market_id.clone());
                }
            }

            for market_id in empty_markets {
                positions.remove(&market_id);
            }

            result
        };

        // Clean up lifecycle only on full close
        if let Some((_, true)) = &result {
            self.remove_lifecycle(token_id).await;
        }

        result
    }

    /// Update peak_bid for trailing stop-loss tracking.
    pub async fn update_peak_bid(&self, token_id: &TokenId, current_bid: Decimal) {
        let mut positions = self.positions.write().await;
        for pos_list in positions.values_mut() {
            for pos in pos_list.iter_mut() {
                if &pos.token_id == token_id && current_bid > pos.peak_bid {
                    pos.peak_bid = current_bid;
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // Performance tracking
    // -------------------------------------------------------------------------

    /// Check if the strategy is auto-disabled due to poor performance.
    pub async fn is_auto_disabled(&self) -> bool {
        if !self.config.performance.auto_disable {
            return false;
        }
        let s = self.stats.read().await;
        s.total_trades() >= self.config.performance.min_trades
            && s.win_rate() < self.config.performance.min_win_rate
    }

    /// Record a trade P&L outcome.
    pub async fn record_trade_pnl(&self, pnl: Decimal) {
        let mut s = self.stats.write().await;
        s.record(pnl);
    }

    /// Adjust total P&L without counting as a separate trade in stats.
    /// Used for costs that are part of an existing trade lifecycle (e.g.,
    /// recovery buy cost) to avoid inflating trade count and skewing win rate.
    pub async fn adjust_trade_pnl(&self, amount: Decimal) {
        let mut s = self.stats.write().await;
        s.adjust_pnl(amount);
    }

    /// Accumulate recovery cost on a position so settlement P&L includes it.
    pub async fn add_recovery_cost(&self, token_id: &str, cost: Decimal) {
        let mut positions = self.positions.write().await;
        for pos_list in positions.values_mut() {
            if let Some(pos) = pos_list.iter_mut().find(|p| p.token_id == token_id) {
                pos.recovery_cost += cost;
                return;
            }
        }
    }
}
