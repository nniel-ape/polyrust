//! Market lifecycle: discovery, promotion, activation, expiry, coin tracking.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tracing::{debug, info};

use polyrust_core::prelude::*;

use crate::crypto_arb::domain::{
    MarketWithReference, PositionLifecycleState,
};
use crate::crypto_arb::runtime::{CryptoArbRuntime, WINDOW_SECS};
use crate::crypto_arb::services::fee_math::parse_slug_timestamp;

impl CryptoArbRuntime {
    // -------------------------------------------------------------------------
    // Market lifecycle (discovery, promotion, expiry)
    // -------------------------------------------------------------------------

    /// Handle a newly discovered market. Extracts the coin, resolves the reference
    /// price, and either activates it immediately or buffers it until a price arrives.
    ///
    /// Returns subscribe action if the market was activated. Idempotent: calling
    /// this multiple times for the same market is safe.
    pub async fn on_market_discovered(
        &self,
        market: &MarketInfo,
        ctx: &StrategyContext,
    ) -> Vec<Action> {
        let coin = match self.extract_coin(&market.question) {
            Some(c) => c,
            None => {
                debug!(
                    market = %market.id,
                    question = %market.question,
                    "Skipping market: could not extract coin from question"
                );
                return vec![];
            }
        };

        if !self.coins.contains(&coin) {
            debug!(
                coin = %coin,
                market = %market.id,
                "Skipping market: coin not in configured set"
            );
            return vec![];
        }

        // Check if already active
        {
            let active = self.active_markets.read().await;
            if active.contains_key(&market.id) {
                debug!(
                    market = %market.id,
                    coin = %coin,
                    "Skipping market: already active"
                );
                return vec![];
            }
        }

        // Get the current crypto price — needed for reference lookup
        let md = ctx.market_data.read().await;
        let current_price = match md.external_prices.get(&coin) {
            Some(&p) => p,
            None => {
                info!(
                    coin = %coin,
                    market = %market.id,
                    "No price yet for coin, buffering market for later activation"
                );
                drop(md);
                let mut pending = self.pending_discovery.write().await;
                pending.entry(coin).or_default().push(market.clone());
                return vec![];
            }
        };
        drop(md);

        let now = ctx.now().await;
        self.activate_market(market, &coin, current_price, now)
            .await
    }

    /// Handle a market expiration. Removes from active markets, resolves open positions.
    ///
    /// Idempotent: calling this multiple times for the same market is safe.
    pub async fn on_market_expired(&self, market_id: &str) -> Vec<Action> {
        // Atomically remove market if present — only the first caller returns
        // the unsubscribe action, avoiding redundant actions when multiple
        // strategy handlers share this base.
        let removed_market = {
            let mut active = self.active_markets.write().await;
            active.remove(market_id)
        };

        let Some(market) = removed_market else {
            // Another handler already processed this expiry
            return vec![];
        };

        info!(
            market = %market_id,
            coin = %market.coin,
            "Market expired, removing from active markets"
        );

        // Clean up cached asks for expired market's token IDs
        {
            let mut cached = self.cached_asks.write().await;
            cached.remove(&market.market.token_ids.outcome_a);
            cached.remove(&market.market.token_ids.outcome_b);
        }

        // Clean up any stale reservation for this market
        {
            let mut reservations = self.market_reservations.write().await;
            reservations.remove(market_id);
        }

        // Cancel outstanding lifecycle exit orders for this market
        let cancel_actions: Vec<Action> = {
            let market_token_ids: Vec<String> = vec![
                market.market.token_ids.outcome_a.clone(),
                market.market.token_ids.outcome_b.clone(),
            ];
            let mut exit_orders = self.exit_orders_by_id.write().await;
            let to_cancel: Vec<(OrderId, String)> = exit_orders
                .iter()
                .filter(|(_, meta)| market_token_ids.contains(&meta.token_id))
                .map(|(oid, meta)| (oid.clone(), meta.token_id.clone()))
                .collect();
            let mut actions = Vec::new();
            for (oid, token_id) in to_cancel {
                exit_orders.remove(&oid);
                info!(
                    order_id = %oid,
                    token_id = %token_id,
                    market = %market_id,
                    "Cancelling exit order on market expiry"
                );
                actions.push(Action::CancelOrder(oid));
            }
            actions
        };

        // Cancel outstanding GTC entry orders for this expired market.
        // These orders are dead — the market no longer exists. Do NOT create
        // synthetic positions; just cancel and remove from tracking.
        let entry_cancel_actions: Vec<Action> = {
            let mut limits = self.open_limit_orders.write().await;
            let to_cancel: Vec<OrderId> = limits
                .iter()
                .filter(|(_, lo)| lo.market_id == market_id)
                .map(|(oid, _)| oid.clone())
                .collect();
            let mut actions = Vec::new();
            for oid in to_cancel {
                if let Some(lo) = limits.remove(&oid) {
                    info!(
                        order_id = %oid,
                        token_id = %lo.token_id,
                        market = %market_id,
                        "Cancelling GTC entry order on market expiry"
                    );
                    actions.push(Action::CancelOrder(oid));
                }
            }
            actions
        };

        // Resolve any remaining positions
        let removed = {
            let mut positions = self.positions.write().await;
            positions.remove(market_id)
        };

        if let Some(positions) = removed {
            let settlement_price = self
                .get_settlement_price(&market.coin, market.market.end_date)
                .await;
            for pos in &positions {
                // Check if position is Hedged (complete set: both YES+NO tokens held).
                // Hedged positions always redeem for $1.00/share regardless of outcome.
                let is_hedged = {
                    let lifecycles = self.position_lifecycle.read().await;
                    lifecycles
                        .get(&pos.token_id)
                        .is_some_and(|lc| matches!(lc.state, PositionLifecycleState::Hedged { .. }))
                };

                let pnl = if is_hedged {
                    // Hedged: set redeems for $1.00/share. P&L = redemption - entry - fees + recovery_cost.
                    // recovery_cost is negative (hedge buy cost already recorded).
                    (Decimal::ONE - pos.entry_price) * pos.size
                        - (pos.entry_fee_per_share * pos.size)
                        + pos.recovery_cost
                } else {
                    let won = match (&pos.side, settlement_price) {
                        (OutcomeSide::Up | OutcomeSide::Yes, Some(cp)) => cp > pos.reference_price,
                        (OutcomeSide::Down | OutcomeSide::No, Some(cp)) => {
                            cp <= pos.reference_price
                        }
                        _ => false,
                    };
                    // Use entry_fee_per_share (0 for GTC entry, actual taker fee for FOK entry)
                    // Include recovery_cost (negative) so win/loss classification reflects
                    // the true net outcome including any opposite-side recovery buys.
                    if won {
                        (Decimal::ONE - pos.entry_price) * pos.size
                            - (pos.entry_fee_per_share * pos.size)
                            + pos.recovery_cost
                    } else {
                        -(pos.entry_price * pos.size) - (pos.entry_fee_per_share * pos.size)
                            + pos.recovery_cost
                    }
                };
                self.record_trade_pnl(pnl).await;
                // Clean up lifecycle state for expired positions
                self.remove_lifecycle(&pos.token_id).await;
                info!(
                    market = %market_id,
                    hedged = is_hedged,
                    pnl = %pnl,
                    settlement_price = ?settlement_price,
                    reference_price = %pos.reference_price,
                    side = ?pos.side,
                    "Position resolved at market expiry"
                );
            }
        }

        self.rebuild_nearest_expiry().await;

        let mut result = cancel_actions;
        result.extend(entry_cancel_actions);
        result.push(Action::UnsubscribeMarket(market_id.to_string()));
        result
    }

    /// Promote pending markets when a price becomes available.
    ///
    /// Called by `record_price` after recording a new price. Returns subscribe
    /// actions for any markets that were promoted.
    pub async fn promote_pending_markets(
        &self,
        symbol: &str,
        current_price: Decimal,
        now: DateTime<Utc>,
    ) -> Vec<Action> {
        let markets = {
            let mut pending = self.pending_discovery.write().await;
            pending.remove(symbol)
        };

        match markets {
            Some(market_list) => {
                let mut actions = Vec::new();
                for m in market_list {
                    actions.extend(self.activate_market(&m, symbol, current_price, now).await);
                }
                actions
            }
            None => vec![],
        }
    }

    /// Internal: activate a market by resolving its reference price and adding it
    /// to active_markets.
    async fn activate_market(
        &self,
        market: &MarketInfo,
        coin: &str,
        current_price: Decimal,
        now: DateTime<Utc>,
    ) -> Vec<Action> {
        let now_ts = now.timestamp();
        let boundary_ts = now_ts - (now_ts % WINDOW_SECS);

        let window_ts = market
            .start_date
            .map(|d| d.timestamp())
            .or_else(|| parse_slug_timestamp(&market.slug))
            .unwrap_or(boundary_ts);

        let (reference_price, reference_quality) = self
            .find_best_reference(coin, window_ts, current_price)
            .await;

        let mwr = MarketWithReference {
            market: market.clone(),
            reference_price,
            reference_quality,
            discovery_time: now,
            coin: coin.to_string(),
            window_ts,
        };

        info!(
            coin = %coin,
            market = %market.id,
            reference = %reference_price,
            quality = ?reference_quality,
            "Activated crypto market"
        );

        let mut active = self.active_markets.write().await;
        active.insert(market.id.clone(), mwr);
        drop(active);

        self.rebuild_nearest_expiry().await;

        vec![Action::SubscribeMarket(market.clone())]
    }

    // -------------------------------------------------------------------------
    // Market management
    // -------------------------------------------------------------------------

    /// Rebuild the coin_nearest_expiry cache from active_markets.
    /// Must be called after any change to active_markets.
    pub async fn rebuild_nearest_expiry(&self) {
        let markets = self.active_markets.read().await;
        let mut nearest: HashMap<String, DateTime<Utc>> = HashMap::new();
        for mwr in markets.values() {
            let entry = nearest
                .entry(mwr.coin.clone())
                .or_insert(mwr.market.end_date);
            if mwr.market.end_date < *entry {
                *entry = mwr.market.end_date;
            }
        }
        let mut cache = self.coin_nearest_expiry.write().await;
        *cache = nearest;
    }

    /// Extract coin symbol from market question string.
    pub fn extract_coin(&self, question: &str) -> Option<String> {
        const COIN_NAMES: &[(&str, &str)] = &[
            ("BITCOIN", "BTC"),
            ("ETHEREUM", "ETH"),
            ("SOLANA", "SOL"),
            ("RIPPLE", "XRP"),
        ];

        let upper = question.to_uppercase();

        // First, check for full coin names
        for &(name, ticker) in COIN_NAMES {
            if upper.contains(name) {
                return Some(ticker.to_string());
            }
        }

        // Then, check for ticker symbols as whole words
        for coin in &self.coins {
            let mut found = false;
            for (idx, _) in upper.match_indices(coin.as_str()) {
                let before_ok = idx == 0
                    || upper[..idx]
                        .chars()
                        .next_back()
                        .is_none_or(|c| !c.is_ascii_alphanumeric());
                let after_idx = idx + coin.len();
                let after_ok = after_idx >= upper.len()
                    || upper[after_idx..]
                        .chars()
                        .next()
                        .is_none_or(|c| !c.is_ascii_alphanumeric());
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
