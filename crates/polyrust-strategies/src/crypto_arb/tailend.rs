//! TailEnd strategy: High-confidence trades near market expiration.
//!
//! Entry conditions:
//! - Time remaining < 120 seconds
//! - Predicted winner's ask >= 0.90
//! - Confidence: 1.0 (fixed, highest priority)
//!
//! Uses FOK orders for speed (taker fee ~0% at extreme prices).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use polyrust_core::prelude::*;

use crate::crypto_arb::base::{taker_fee, CryptoArbBase};
use crate::crypto_arb::dashboard::try_emit_dashboard_updates;
use crate::crypto_arb::types::{
    ArbitrageMode, ArbitrageOpportunity, ArbitragePosition, PendingOrder,
};

/// TailEnd strategy: trades near expiration with high market prices.
pub struct TailEndStrategy {
    base: Arc<CryptoArbBase>,
}

impl TailEndStrategy {
    pub fn new(base: Arc<CryptoArbBase>) -> Self {
        Self { base }
    }

    /// Evaluate tail-end opportunity for a market.
    async fn evaluate_opportunity(
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

        let time_remaining = market.market.seconds_remaining();

        // Must be within the tail-end window
        if time_remaining >= self.base.config.tailend.time_threshold_secs as i64
            || time_remaining <= 0
        {
            debug!(
                market = %market_id,
                time_remaining = time_remaining,
                "TailEnd skip: time outside (0, 120) window"
            );
            return None;
        }

        // Check if mode is disabled
        if self.base.is_mode_disabled(&ArbitrageMode::TailEnd).await {
            debug!(
                market = %market_id,
                "TailEnd skip: mode auto-disabled by performance tracker"
            );
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
                return None;
            }
        };

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

        let ask_price = match ask {
            Some(p) => p,
            None => {
                debug!(
                    market = %market_id,
                    predicted_side = ?predicted,
                    "TailEnd skip: no ask in orderbook for predicted side"
                );
                return None;
            }
        };

        // Ask must be >= configured threshold for tail-end
        if ask_price < self.base.config.tailend.ask_threshold {
            debug!(
                market = %market_id,
                ask = %ask_price,
                time_remaining = time_remaining,
                "TailEnd skip: ask below 0.90 threshold"
            );
            return None;
        }

        let profit_margin = Decimal::ONE - ask_price;
        let estimated_fee = taker_fee(ask_price, self.base.config.fee.taker_fee_rate);
        let net_margin = profit_margin - estimated_fee;

        Some(ArbitrageOpportunity {
            mode: ArbitrageMode::TailEnd,
            market_id: market_id.clone(),
            outcome_to_buy: predicted,
            token_id: token_id.clone(),
            buy_price: ask_price,
            confidence: Decimal::ONE, // Tail-end always has max confidence
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
                        if pos.mode == ArbitrageMode::TailEnd {
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
                                "TailEnd stop-loss sell confirmed"
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
                        "TailEnd stop-loss sell failed"
                    );
                }
                return vec![];
            }
        }

        let pending = {
            let mut orders = self.base.pending_orders.write().await;
            match orders.remove(&result.token_id) {
                Some(p) if p.mode == ArbitrageMode::TailEnd => p,
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
                "TailEnd order rejected"
            );
            return vec![];
        }

        // FOK orders fill immediately — create position now
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
            size = %position.size,
            mode = %pending.mode,
            "TailEnd position confirmed after order fill"
        );

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
                        debug!(
                            market = %market_id,
                            "TailEnd skip: already have exposure to market"
                        );
                        continue;
                    }

                    // Check position limits
                    if !self.base.can_open_position().await {
                        debug!(
                            market = %market_id,
                            "TailEnd skip: max positions reached"
                        );
                        break;
                    }

                    if let Some(opp) = self.evaluate_opportunity(&market_id, *price, ctx).await {
                        if opp.buy_price.is_zero() {
                            warn!(market = %market_id, "skipping TailEnd opportunity with zero buy_price");
                            continue;
                        }

                        // TailEnd uses fixed sizing (no Kelly - confidence is always 1.0)
                        let size = self.base.config.sizing.base_size / opp.buy_price;

                        // Validate minimum order size
                        if !self.base.validate_min_order_size(&market_id, size).await {
                            continue;
                        }

                        // TailEnd always uses FOK orders (speed matters)
                        let order = OrderRequest {
                            token_id: opp.token_id.clone(),
                            price: opp.buy_price,
                            size,
                            side: OrderSide::Buy,
                            order_type: OrderType::Fok,
                            neg_risk: false,
                        };

                        info!(
                            mode = ?opp.mode,
                            market = %market_id,
                            confidence = %opp.confidence,
                            price = %opp.buy_price,
                            side = ?opp.outcome_to_buy,
                            "Submitting TailEnd order"
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
                                        price: opp.buy_price,
                                        size,
                                        reference_price: market.reference_price,
                                        coin: market.coin.clone(),
                                        order_type: OrderType::Fok,
                                        mode: ArbitrageMode::TailEnd,
                                        kelly_fraction: None,
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
                        .filter(|(_, p)| p.mode == ArbitrageMode::TailEnd)
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
                            "TailEnd stop-loss triggered"
                        );
                        let mut pending_sl = self.base.pending_stop_loss.write().await;
                        pending_sl.insert(pos.token_id.clone(), exit_price);
                        actions.push(action);
                    }
                }

                actions
            }

            Event::OrderUpdate(OrderEvent::Placed(result)) => self.on_order_placed(result).await,

            Event::OrderUpdate(OrderEvent::Rejected { token_id, .. }) => {
                if let Some(token_id) = token_id {
                    // Clear pending buy order if it's ours
                    let mut pending = self.base.pending_orders.write().await;
                    if let Some(p) = pending.get(token_id)
                        && p.mode == ArbitrageMode::TailEnd
                    {
                        pending.remove(token_id);
                        warn!(
                            token_id = %token_id,
                            "TailEnd pending order rejected"
                        );
                    }

                    // Clear pending stop-loss
                    let mut pending_sl = self.base.pending_stop_loss.write().await;
                    if pending_sl.remove(token_id).is_some() {
                        warn!(
                            token_id = %token_id,
                            "TailEnd stop-loss sell rejected"
                        );
                    }
                }
                vec![]
            }

            _ => vec![],
        };

        // Check stale limit orders (TailEnd doesn't use GTC, but check anyway for shared state)
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
