//! Confirmed strategy: Directional trades with dynamic confidence model.
//!
//! Entry conditions:
//! - Confidence >= 50% (from reference price distance + market boost)
//! - Net margin >= min_profit_margin (or late_window_margin if < 300s)
//!
//! Uses GTC maker orders to avoid taker fees.
//! Applies Kelly criterion for position sizing.

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
};

/// Confirmed strategy: directional trades with dynamic confidence model.
pub struct ConfirmedStrategy {
    base: Arc<CryptoArbBase>,
}

impl ConfirmedStrategy {
    pub fn new(base: Arc<CryptoArbBase>) -> Self {
        Self { base }
    }

    /// Evaluate confirmed opportunity for a market.
    async fn evaluate_opportunity(
        &self,
        market_id: &MarketId,
        current_price: Decimal,
        ctx: &StrategyContext,
    ) -> Option<ArbitrageOpportunity> {
        let markets = self.base.active_markets.read().await;
        let market = markets.get(market_id)?;

        let time_remaining = market.market.seconds_remaining();
        if time_remaining <= 0 {
            return None;
        }

        // Skip if TailEnd is enabled and should handle this (within tail-end window)
        // Confirmed takes over from the tail-end threshold onwards
        if self.base.config.tailend.enabled
            && time_remaining < self.base.config.tailend.time_threshold_secs as i64
        {
            return None;
        }

        // Check if mode is disabled
        if self.base.is_mode_disabled(&ArbitrageMode::Confirmed).await {
            return None;
        }

        // Predict winner based on crypto price
        let predicted = market.predict_winner(current_price)?;

        let md = ctx.market_data.read().await;
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

        let ask_price = ask?;

        // Calculate confidence using the market's model
        let confidence = market.get_confidence(current_price, ask_price, time_remaining);

        // Must meet minimum confidence threshold
        if confidence < self.base.config.confirmed.min_confidence {
            return None;
        }

        let profit_margin = Decimal::ONE - ask_price;

        // In hybrid mode, Confirmed uses GTC maker orders (0 fee)
        let is_maker = self.base.config.order.hybrid_mode;
        let estimated_fee = if is_maker {
            Decimal::ZERO
        } else {
            taker_fee(ask_price, self.base.config.fee.taker_fee_rate)
        };
        let net_margin = profit_margin - estimated_fee;

        // Check minimum margin threshold (per-mode config, with late-window override)
        let min_margin = if time_remaining < 300 {
            self.base.config.late_window_margin
        } else {
            self.base.config.confirmed.min_margin
        };

        if net_margin < min_margin {
            return None;
        }

        Some(ArbitrageOpportunity {
            mode: ArbitrageMode::Confirmed,
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
                        if pos.mode == ArbitrageMode::Confirmed {
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
                                "Confirmed stop-loss sell confirmed"
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
                        "Confirmed stop-loss sell failed"
                    );
                }
                return vec![];
            }
        }

        let pending = {
            let mut orders = self.base.pending_orders.write().await;
            match orders.remove(&result.token_id) {
                Some(p) if p.mode == ArbitrageMode::Confirmed => p,
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
                "Confirmed order rejected"
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
                    kelly = ?pending.kelly_fraction,
                    "Confirmed GTC limit order placed"
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
            kelly = ?position.kelly_fraction,
            "Confirmed position created"
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
                Some(lo) if lo.mode == ArbitrageMode::Confirmed => lo,
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
            "Confirmed GTC order filled"
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
impl Strategy for ConfirmedStrategy {
    fn name(&self) -> &str {
        "crypto-arb-confirmed"
    }

    fn description(&self) -> &str {
        "Confirmed arbitrage: directional trades with dynamic confidence model"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        info!(
            coins = ?self.base.config.coins,
            min_margin = %self.base.config.min_profit_margin,
            "Confirmed strategy started"
        );
        Ok(())
    }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
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
                // Record price and promote any pending markets
                let (_, promote_actions) =
                    self.base.record_price(symbol, *price, source).await;
                let mut result = promote_actions;

                // Find active markets for this coin
                let market_ids: Vec<MarketId> = {
                    let markets = self.base.active_markets.read().await;
                    markets
                        .iter()
                        .filter(|(_, m)| m.coin == *symbol)
                        .map(|(id, _)| id.clone())
                        .collect()
                };

                for market_id in market_ids {
                    // Skip if we already have exposure
                    if self.base.has_market_exposure(&market_id).await {
                        continue;
                    }

                    // Check position limits
                    if !self.base.can_open_position().await {
                        break;
                    }

                    // Spike pre-filter: Only evaluate if price delta exceeds fee+margin threshold
                    // This optimization skips evaluation when the price change is too small to be profitable
                    let should_evaluate = {
                        let markets = self.base.active_markets.read().await;
                        if let Some(market) = markets.get(&market_id) {
                            // Calculate mid price from orderbooks
                            let md = ctx.market_data.read().await;
                            let outcome_a_mid = md.orderbooks.get(&market.market.token_ids.outcome_a)
                                .and_then(|ob| {
                                    let best_ask = ob.best_ask()?;
                                    let best_bid = ob.best_bid()?;
                                    Some((best_ask + best_bid) / Decimal::new(2, 0))
                                });
                            let outcome_b_mid = md.orderbooks.get(&market.market.token_ids.outcome_b)
                                .and_then(|ob| {
                                    let best_ask = ob.best_ask()?;
                                    let best_bid = ob.best_bid()?;
                                    Some((best_ask + best_bid) / Decimal::new(2, 0))
                                });

                            // Use whichever outcome is predicted to win
                            let predicted = market.predict_winner(*price);
                            let mid_price = match predicted {
                                Some(OutcomeSide::Up | OutcomeSide::Yes) => outcome_a_mid,
                                Some(OutcomeSide::Down | OutcomeSide::No) => outcome_b_mid,
                                None => None,
                            };

                            if let Some(mid) = mid_price {
                                // Calculate minimum threshold: taker_fee + min_margin
                                // (We use taker fee for conservative estimate even though we'll use GTC)
                                let fee = taker_fee(mid, self.base.config.fee.taker_fee_rate);
                                let min_margin = self.base.config.confirmed.min_margin;
                                let threshold = fee + min_margin;

                                // Calculate crypto price change from reference
                                let delta = (*price - market.reference_price).abs() / market.reference_price;
                                delta >= threshold
                            } else {
                                // Can't compute mid price, evaluate anyway
                                true
                            }
                        } else {
                            false
                        }
                    };

                    if !should_evaluate {
                        continue;
                    }

                    if let Some(opp) = self.evaluate_opportunity(&market_id, *price, ctx).await {
                        if opp.buy_price.is_zero() {
                            warn!(market = %market_id, "skipping Confirmed opportunity with zero buy_price");
                            continue;
                        }

                        // Kelly criterion sizing
                        let (size, kelly_frac) = if self.base.config.sizing.use_kelly {
                            let kelly_size = kelly_position_size(
                                opp.confidence,
                                opp.buy_price,
                                &self.base.config.sizing,
                            );
                            if kelly_size.is_zero() {
                                info!(
                                    market = %market_id,
                                    confidence = %opp.confidence,
                                    price = %opp.buy_price,
                                    "Confirmed Kelly sizing returned 0, skipping"
                                );
                                continue;
                            }
                            // Convert USDC size to share count
                            let shares = kelly_size / opp.buy_price;
                            // Compute raw Kelly fraction for tracking
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

                        // Validate minimum order size
                        if !self.base.validate_min_order_size(&market_id, size).await {
                            continue;
                        }

                        // Hybrid order mode: GTC at best_ask - limit_offset (maker, $0 fee)
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
                            market = %market_id,
                            confidence = %opp.confidence,
                            price = %order_price,
                            order_type = ?order_type,
                            side = ?opp.outcome_to_buy,
                            kelly = ?kelly_frac,
                            "Submitting Confirmed order"
                        );

                        // Track pending order
                        {
                            let markets = self.base.active_markets.read().await;
                            if let Some(market) = markets.get(&market_id) {
                                let mut pending = self.base.pending_orders.write().await;
                                pending.insert(
                                    opp.token_id.clone(),
                                    PendingOrder {
                                        market_id: market_id.clone(),
                                        token_id: opp.token_id.clone(),
                                        side: opp.outcome_to_buy,
                                        price: order_price,
                                        size,
                                        reference_price: market.reference_price,
                                        coin: market.coin.clone(),
                                        order_type,
                                        mode: ArbitrageMode::Confirmed,
                                        kelly_fraction: kelly_frac,
                                        estimated_fee: opp.estimated_fee,
                                    },
                                );
                            }
                        }

                        result.push(Action::PlaceOrder(order));
                    }
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
                        .filter(|(_, p)| p.mode == ArbitrageMode::Confirmed)
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
                            "Confirmed stop-loss triggered"
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
                    && lo.mode == ArbitrageMode::Confirmed
                {
                    lo.size = *remaining_size;
                    info!(
                        order_id = %order_id,
                        filled = %filled_size,
                        remaining = %remaining_size,
                        "Confirmed GTC order partially filled"
                    );
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::Cancelled(order_id)) => {
                let mut limits = self.base.open_limit_orders.write().await;
                if let Some(lo) = limits.remove(order_id)
                    && lo.mode == ArbitrageMode::Confirmed
                {
                    info!(
                        order_id = %order_id,
                        market = %lo.market_id,
                        "Confirmed GTC order cancelled"
                    );
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::Rejected { token_id, .. }) => {
                if let Some(token_id) = token_id {
                    // Clear pending buy order if it's ours
                    let mut pending = self.base.pending_orders.write().await;
                    if let Some(p) = pending.get(token_id)
                        && p.mode == ArbitrageMode::Confirmed
                    {
                        pending.remove(token_id);
                        warn!(
                            token_id = %token_id,
                            "Confirmed pending order rejected"
                        );
                    }

                    // Clear pending stop-loss
                    let mut pending_sl = self.base.pending_stop_loss.write().await;
                    if pending_sl.remove(token_id).is_some() {
                        warn!(
                            token_id = %token_id,
                            "Confirmed stop-loss sell rejected"
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
        info!("Confirmed strategy stopping");
        Ok(vec![])
    }

    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        None // Uses shared dashboard
    }
}
