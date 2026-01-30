//! CrossCorrelated strategy: Correlation-based signals from leader coin spikes.
//!
//! Entry conditions:
//! - Leader coin spikes by >= min_spike_pct
//! - Follower market hasn't moved yet (ask in [0.40, 0.60])
//! - Net margin >= min_profit_margin
//!
//! Confidence is discounted by correlation factor (default 0.7) for uncertainty.
//! Uses GTC maker orders in hybrid mode.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use tracing::{info, warn};

use polyrust_core::prelude::*;

use crate::crypto_arb::base::{kelly_position_size, taker_fee, CryptoArbBase};
use crate::crypto_arb::dashboard::try_emit_dashboard_updates;
use crate::crypto_arb::types::{
    ArbitrageMode, ArbitrageOpportunity, ArbitragePosition, OpenLimitOrder, PendingOrder,
    SpikeEvent,
};

/// CrossCorrelated strategy: trades follower coins when leader spikes.
pub struct CrossCorrStrategy {
    base: Arc<CryptoArbBase>,
}

impl CrossCorrStrategy {
    pub fn new(base: Arc<CryptoArbBase>) -> Self {
        Self { base }
    }

    /// Get follower coins for a leader coin from config.
    fn get_followers(&self, leader: &str) -> Vec<String> {
        self.base
            .config
            .correlation
            .pairs
            .iter()
            .filter(|(l, _)| l == leader)
            .flat_map(|(_, followers)| followers.clone())
            .collect()
    }

    /// Generate cross-correlated opportunities for follower coins.
    async fn generate_opportunities(
        &self,
        leader_coin: &str,
        leader_change_pct: Decimal,
        ctx: &StrategyContext,
    ) -> Vec<(ArbitrageOpportunity, Decimal, Option<Decimal>)> {
        // (opportunity, size, kelly_fraction)
        let followers = self.get_followers(leader_coin);
        if followers.is_empty() {
            return vec![];
        }

        // Compute leader confidence from spike magnitude
        let leader_confidence = leader_change_pct.abs().min(Decimal::ONE);
        let discount = self.base.config.correlation.discount_factor;
        let follower_confidence = leader_confidence * discount;

        // Need at least 50% confidence
        if follower_confidence < Decimal::new(50, 2) {
            return vec![];
        }

        // Check if CrossCorrelated mode is disabled
        let cross_mode = ArbitrageMode::CrossCorrelated {
            leader: leader_coin.to_string(),
        };
        if self.base.is_mode_disabled(&cross_mode.canonical()).await {
            return vec![];
        }

        let md = ctx.market_data.read().await;
        let mut opportunities = Vec::new();

        for follower_coin in &followers {
            // Find active markets for this follower
            let follower_market_ids: Vec<MarketId> = {
                let markets = self.base.active_markets.read().await;
                markets
                    .iter()
                    .filter(|(_, m)| m.coin == *follower_coin)
                    .map(|(id, _)| id.clone())
                    .collect()
            };

            for market_id in follower_market_ids {
                let markets = self.base.active_markets.read().await;
                let market = match markets.get(&market_id) {
                    Some(m) => m.clone(),
                    None => continue,
                };
                drop(markets);

                // Skip if we already have exposure
                if self.base.has_market_exposure(&market_id).await {
                    continue;
                }

                // Skip ended markets
                if market.market.seconds_remaining() <= 0 {
                    continue;
                }

                // Determine predicted side: leader went up → follower Up
                let predicted = if leader_change_pct > Decimal::ZERO {
                    OutcomeSide::Up
                } else {
                    OutcomeSide::Down
                };

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
                    None => continue,
                };

                // Skip if follower market already moved (outside [0.40, 0.60])
                if ask_price > Decimal::new(60, 2) || ask_price < Decimal::new(40, 2) {
                    info!(
                        leader = %leader_coin,
                        follower = %follower_coin,
                        ask = %ask_price,
                        "Skipping cross-correlated: follower market already moved"
                    );
                    continue;
                }

                let profit_margin = Decimal::ONE - ask_price;
                let is_maker = self.base.config.order.hybrid_mode;
                let estimated_fee = if is_maker {
                    Decimal::ZERO
                } else {
                    taker_fee(ask_price, self.base.config.fee.taker_fee_rate)
                };
                let net_margin = profit_margin - estimated_fee;

                if net_margin < self.base.config.min_profit_margin {
                    continue;
                }

                // Check position limits
                if !self.base.can_open_position().await {
                    break;
                }

                info!(
                    leader = %leader_coin,
                    follower = %follower_coin,
                    leader_change = %leader_change_pct,
                    confidence = %follower_confidence,
                    ask = %ask_price,
                    net_margin = %net_margin,
                    "Cross-correlated opportunity detected"
                );

                let opp = ArbitrageOpportunity {
                    mode: ArbitrageMode::CrossCorrelated {
                        leader: leader_coin.to_string(),
                    },
                    market_id: market_id.clone(),
                    outcome_to_buy: predicted,
                    token_id: token_id.clone(),
                    buy_price: ask_price,
                    confidence: follower_confidence,
                    profit_margin,
                    estimated_fee,
                    net_margin,
                };

                // Calculate size and Kelly fraction
                let (size, kelly_frac) = if self.base.config.sizing.use_kelly {
                    let kelly_size = kelly_position_size(
                        opp.confidence,
                        opp.buy_price,
                        &self.base.config.sizing,
                    );
                    if kelly_size.is_zero() {
                        continue;
                    }
                    let shares = kelly_size / opp.buy_price;
                    let payout = Decimal::ONE / opp.buy_price - Decimal::ONE;
                    let kf = if payout > Decimal::ZERO {
                        (opp.confidence * payout - (Decimal::ONE - opp.confidence)) / payout
                    } else {
                        Decimal::ZERO
                    };
                    (shares, Some(kf))
                } else {
                    (self.base.config.sizing.base_size / opp.buy_price, None)
                };

                opportunities.push((opp, size, kelly_frac));
            }
        }

        opportunities
    }

    /// Handle order placement result.
    async fn on_order_placed(&self, result: &OrderResult) -> Vec<Action> {
        // Check if this is a stop-loss sell confirmation
        {
            let mut pending_sl = self.base.pending_stop_loss.write().await;
            if let Some(exit_price) = pending_sl.remove(&result.token_id) {
                if result.success {
                    if let Some(pos) = self.base.remove_position_by_token(&result.token_id).await {
                        if matches!(pos.mode, ArbitrageMode::CrossCorrelated { .. }) {
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
                                "CrossCorr stop-loss sell confirmed"
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
                        "CrossCorr stop-loss sell failed"
                    );
                }
                return vec![];
            }
        }

        let pending = {
            let mut orders = self.base.pending_orders.write().await;
            match orders.remove(&result.token_id) {
                Some(p) if matches!(p.mode, ArbitrageMode::CrossCorrelated { .. }) => p,
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
                "CrossCorr order rejected"
            );
            return vec![];
        }

        // GTC orders: track as open limit order
        if pending.order_type == OrderType::Gtc {
            if let Some(order_id) = &result.order_id {
                info!(
                    order_id = %order_id,
                    market = %pending.market_id,
                    price = %pending.price,
                    "CrossCorr GTC limit order placed"
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
                        placed_at: tokio::time::Instant::now(),
                        mode: pending.mode,
                        kelly_fraction: pending.kelly_fraction,
                        estimated_fee: pending.estimated_fee,
                    },
                );
            }
            return vec![];
        }

        // FOK orders fill immediately
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
        };

        info!(
            market = %pending.market_id,
            side = ?position.side,
            price = %position.entry_price,
            "CrossCorr position created"
        );

        self.base.record_position(position).await;
        vec![]
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
                Some(lo) if matches!(lo.mode, ArbitrageMode::CrossCorrelated { .. }) => lo,
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
            "CrossCorr GTC order filled"
        );

        let position = ArbitragePosition {
            market_id: lo.market_id.clone(),
            token_id: lo.token_id,
            side: lo.side,
            entry_price: price,
            size,
            reference_price: lo.reference_price,
            coin: lo.coin,
            order_id: Some(order_id.to_string()),
            entry_time: Utc::now(),
            kelly_fraction: lo.kelly_fraction,
            peak_bid: price,
            mode: lo.mode,
            estimated_fee: lo.estimated_fee,
        };

        self.base.record_position(position).await;
        vec![]
    }
}

#[async_trait]
impl Strategy for CrossCorrStrategy {
    fn name(&self) -> &str {
        "crypto-arb-crosscorr"
    }

    fn description(&self) -> &str {
        "Cross-correlated arbitrage: trades follower coins on leader spikes"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        info!(
            enabled = self.base.config.correlation.enabled,
            pairs = ?self.base.config.correlation.pairs,
            "CrossCorr strategy started"
        );
        Ok(())
    }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        // Skip if correlation is disabled
        if !self.base.config.correlation.enabled {
            return Ok(vec![]);
        }

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
                // Record price, promote pending markets, and check for spike
                let (spike, promote_actions) =
                    self.base.record_price(symbol, *price, source).await;

                // Only process if spike exceeds correlation threshold
                let change_pct = match spike {
                    Some(pct) if pct.abs() >= self.base.config.correlation.min_spike_pct => pct,
                    _ => return Ok(promote_actions),
                };

                // Check if this coin is a leader in any pairs
                if self.get_followers(symbol).is_empty() {
                    return Ok(promote_actions);
                }

                // Record spike event
                {
                    let from_price = {
                        let history = self.base.price_history.read().await;
                        history.get(symbol).and_then(|h| {
                            let cutoff = Utc::now()
                                - chrono::Duration::seconds(
                                    self.base.config.spike.window_secs as i64,
                                );
                            h.iter()
                                .rev()
                                .find(|(ts, _, _)| *ts <= cutoff)
                                .map(|(_, p, _)| *p)
                        })
                    }
                    .unwrap_or(*price);

                    self.base
                        .record_spike(SpikeEvent {
                            coin: symbol.to_string(),
                            timestamp: Utc::now(),
                            change_pct,
                            from_price,
                            to_price: *price,
                            acted: true,
                        })
                        .await;
                }

                // Generate opportunities for follower coins
                let opportunities = self.generate_opportunities(symbol, change_pct, ctx).await;

                let mut result = promote_actions;

                for (opp, size, kelly_frac) in opportunities {
                    let (order_type, order_price) = if self.base.config.order.hybrid_mode {
                        let limit_price = (opp.buy_price - self.base.config.order.limit_offset)
                            .max(Decimal::new(1, 2));
                        (OrderType::Gtc, limit_price)
                    } else {
                        (OrderType::Fok, opp.buy_price)
                    };

                    let order = OrderRequest {
                        token_id: opp.token_id.clone(),
                        price: order_price,
                        size,
                        side: OrderSide::Buy,
                        order_type,
                        neg_risk: false,
                    };

                    info!(
                        mode = ?opp.mode,
                        market = %opp.market_id,
                        confidence = %opp.confidence,
                        price = %order_price,
                        kelly = ?kelly_frac,
                        "Submitting CrossCorr order"
                    );

                    {
                        let markets = self.base.active_markets.read().await;
                        if let Some(market) = markets.get(&opp.market_id) {
                            let mut pending = self.base.pending_orders.write().await;
                            pending.insert(
                                opp.token_id.clone(),
                                PendingOrder {
                                    market_id: opp.market_id.clone(),
                                    token_id: opp.token_id.clone(),
                                    side: opp.outcome_to_buy,
                                    price: order_price,
                                    size,
                                    reference_price: market.reference_price,
                                    coin: market.coin.clone(),
                                    order_type,
                                    mode: opp.mode.clone(),
                                    kelly_fraction: kelly_frac,
                                    estimated_fee: opp.estimated_fee,
                                },
                            );
                        }
                    }

                    result.push(Action::PlaceOrder(order));
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
                        .filter(|(_, p)| matches!(p.mode, ArbitrageMode::CrossCorrelated { .. }))
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
                            "CrossCorr stop-loss triggered"
                        );
                        let mut pending_sl = self.base.pending_stop_loss.write().await;
                        pending_sl.insert(pos.token_id.clone(), exit_price);
                        actions.push(action);
                    }
                }

                actions
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
                if let Some(lo) = limits.get_mut(order_id)
                    && matches!(lo.mode, ArbitrageMode::CrossCorrelated { .. })
                {
                    lo.size = *remaining_size;
                    info!(
                        order_id = %order_id,
                        filled = %filled_size,
                        remaining = %remaining_size,
                        "CrossCorr GTC order partially filled"
                    );
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::Cancelled(order_id)) => {
                let mut limits = self.base.open_limit_orders.write().await;
                if let Some(lo) = limits.remove(order_id)
                    && matches!(lo.mode, ArbitrageMode::CrossCorrelated { .. })
                {
                    info!(
                        order_id = %order_id,
                        market = %lo.market_id,
                        "CrossCorr GTC order cancelled"
                    );
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::Rejected { token_id, .. }) => {
                if let Some(token_id) = token_id {
                    // Clear pending buy order if it's ours
                    let mut pending = self.base.pending_orders.write().await;
                    if let Some(p) = pending.get(token_id)
                        && matches!(p.mode, ArbitrageMode::CrossCorrelated { .. })
                    {
                        pending.remove(token_id);
                        warn!(
                            token_id = %token_id,
                            "CrossCorr pending order rejected"
                        );
                    }

                    // Clear pending stop-loss
                    let mut pending_sl = self.base.pending_stop_loss.write().await;
                    if pending_sl.remove(token_id).is_some() {
                        warn!(
                            token_id = %token_id,
                            "CrossCorr stop-loss sell rejected"
                        );
                    }
                }
                vec![]
            }

            _ => vec![],
        };

        // Check stale limit orders
        actions.extend(self.base.check_stale_limit_orders().await);

        // Emit SSE dashboard updates (throttled to ~5s across all strategies)
        actions.extend(try_emit_dashboard_updates(&self.base).await);

        Ok(actions)
    }

    async fn on_stop(&mut self, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        info!("CrossCorr strategy stopping");
        Ok(vec![])
    }

    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        None // Uses shared dashboard
    }
}
