//! TwoSided strategy: Risk-free arbitrage when both outcomes are mispriced.
//!
//! Entry conditions:
//! - Combined ask of both outcomes < 0.98
//! - Guaranteed profit regardless of which outcome wins
//!
//! Uses batch GTC orders for atomic execution (maker fee = $0).

use std::sync::Arc;

use async_trait::async_trait;
use rust_decimal::Decimal;
use tracing::{info, warn};

use polyrust_core::prelude::*;

use crate::crypto_arb::base::{CryptoArbBase, taker_fee};
use crate::crypto_arb::dashboard::try_emit_dashboard_updates;
use crate::crypto_arb::types::{
    ArbitrageMode, ArbitrageOpportunity, ArbitragePosition, OpenLimitOrder, PendingOrder,
};

/// TwoSided strategy: buys both outcomes when combined price < $1.
pub struct TwoSidedStrategy {
    base: Arc<CryptoArbBase>,
}

impl TwoSidedStrategy {
    pub fn new(base: Arc<CryptoArbBase>) -> Self {
        Self { base }
    }

    /// Evaluate two-sided opportunity for a market.
    async fn evaluate_opportunity(
        &self,
        market_id: &MarketId,
        ctx: &StrategyContext,
    ) -> Option<Vec<ArbitrageOpportunity>> {
        let markets = self.base.active_markets.read().await;
        let market = markets.get(market_id)?;

        let now = ctx.now().await;
        let time_remaining = market.market.seconds_remaining_at(now);
        if time_remaining <= 0 {
            return None;
        }

        // Check if mode is disabled
        if self.base.is_mode_disabled(&ArbitrageMode::TwoSided).await {
            return None;
        }

        let md = ctx.market_data.read().await;

        let up_ask = md
            .orderbooks
            .get(&market.market.token_ids.outcome_a)
            .and_then(|ob| ob.best_ask());
        let down_ask = md
            .orderbooks
            .get(&market.market.token_ids.outcome_b)
            .and_then(|ob| ob.best_ask());

        let (ua, da) = match (up_ask, down_ask) {
            (Some(ua), Some(da)) => (ua, da),
            _ => return None,
        };

        let combined = ua + da;
        // Must be below combined threshold for guaranteed profit
        if combined >= self.base.config.twosided.combined_threshold {
            return None;
        }

        let profit_margin = Decimal::ONE - combined;

        // In hybrid mode, TwoSided uses GTC maker orders (0 fee)
        let is_maker = self.base.config.order.hybrid_mode;
        let fee_up = if is_maker {
            Decimal::ZERO
        } else {
            taker_fee(ua, self.base.config.fee.taker_fee_rate)
        };
        let fee_down = if is_maker {
            Decimal::ZERO
        } else {
            taker_fee(da, self.base.config.fee.taker_fee_rate)
        };
        let total_fee = fee_up + fee_down;
        let net_margin = profit_margin - total_fee;

        // Skip if net margin is negative after fees
        if net_margin <= Decimal::ZERO {
            return None;
        }

        Some(vec![
            ArbitrageOpportunity {
                mode: ArbitrageMode::TwoSided,
                market_id: market_id.clone(),
                outcome_to_buy: OutcomeSide::Up,
                token_id: market.market.token_ids.outcome_a.clone(),
                buy_price: ua,
                confidence: Decimal::ONE,
                profit_margin,
                estimated_fee: fee_up,
                net_margin,
            },
            ArbitrageOpportunity {
                mode: ArbitrageMode::TwoSided,
                market_id: market_id.clone(),
                outcome_to_buy: OutcomeSide::Down,
                token_id: market.market.token_ids.outcome_b.clone(),
                buy_price: da,
                confidence: Decimal::ONE,
                profit_margin,
                estimated_fee: fee_down,
                net_margin,
            },
        ])
    }

    /// Handle order placement result.
    async fn on_order_placed(&self, result: &OrderResult) -> Vec<Action> {
        let pending = {
            let mut orders = self.base.pending_orders.write().await;
            match orders.remove(&result.token_id) {
                Some(p) if p.mode == ArbitrageMode::TwoSided => p,
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
                "TwoSided order rejected"
            );
            return vec![];
        }

        // TwoSided uses GTC orders in hybrid mode
        if pending.order_type == OrderType::Gtc {
            if let Some(order_id) = &result.order_id {
                info!(
                    order_id = %order_id,
                    market = %pending.market_id,
                    mode = ?pending.mode,
                    price = %pending.price,
                    "TwoSided GTC limit order placed"
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
                        mode: pending.mode,
                        kelly_fraction: pending.kelly_fraction,
                        estimated_fee: pending.estimated_fee,
                        tick_size: pending.tick_size,
                        fee_rate_bps: pending.fee_rate_bps,
                        cancel_pending: false,
                    },
                );
            }
            return vec![];
        }

        // FOK orders fill immediately
        let now = self.base.event_time().await;
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
            mode: pending.mode.clone(),
            estimated_fee: pending.estimated_fee,
            entry_market_price: pending.price,
            tick_size: pending.tick_size,
            fee_rate_bps: pending.fee_rate_bps,
        };

        info!(
            market = %pending.market_id,
            side = ?position.side,
            price = %position.entry_price,
            "TwoSided position confirmed"
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
                Some(lo) if lo.mode == ArbitrageMode::TwoSided => lo,
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
            "TwoSided GTC order filled"
        );

        let now = self.base.event_time().await;
        let position = ArbitragePosition {
            market_id: lo.market_id.clone(),
            token_id: lo.token_id,
            side: lo.side,
            entry_price: price,
            size,
            reference_price: lo.reference_price,
            coin: lo.coin,
            order_id: Some(order_id.to_string()),
            entry_time: now,
            kelly_fraction: lo.kelly_fraction,
            peak_bid: price,
            mode: lo.mode,
            estimated_fee: lo.estimated_fee,
            entry_market_price: price,
            tick_size: lo.tick_size,
            fee_rate_bps: lo.fee_rate_bps,
        };

        self.base.record_position(position).await;
        vec![]
    }
}

#[async_trait]
impl Strategy for TwoSidedStrategy {
    fn name(&self) -> &str {
        "crypto-arb-twosided"
    }

    fn description(&self) -> &str {
        "Two-sided arbitrage: buys both outcomes when combined price < $1"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        info!(
            coins = ?self.base.config.coins,
            "TwoSided strategy started"
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
                ..
            }) => {
                // Record price and promote any pending markets
                let now = ctx.now().await;
                let (_, promote_actions) =
                    self.base.record_price(symbol, *price, source, now).await;
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
                    // Skip if market is in stale-removal cooldown
                    if self.base.is_stale_market_cooled_down(&market_id).await {
                        continue;
                    }

                    // Atomically check exposure + position limits and reserve (2 slots)
                    if !self
                        .base
                        .try_reserve_market(&market_id, ArbitrageMode::TwoSided, 2)
                        .await
                    {
                        continue;
                    }

                    let opps = match self.evaluate_opportunity(&market_id, ctx).await {
                        Some(opps) if opps.len() == 2 => opps,
                        _ => {
                            self.base.release_reservation(&market_id).await;
                            continue;
                        }
                    };

                    // Compute equal share count across both outcomes
                    let combined_price = opps[0].buy_price + opps[1].buy_price;
                    if combined_price <= Decimal::ZERO {
                        self.base.release_reservation(&market_id).await;
                        continue;
                    }
                    let share_count = self.base.config.sizing.base_size / combined_price;
                    if share_count.is_zero() {
                        warn!(
                            market_id = %market_id,
                            combined_price = %combined_price,
                            base_size = %self.base.config.sizing.base_size,
                            "Skipping TwoSided: share_count rounds to zero"
                        );
                        self.base.release_reservation(&market_id).await;
                        continue;
                    }

                    // Validate minimum order size for both legs
                    if !self
                        .base
                        .validate_min_order_size(&market_id, share_count)
                        .await
                    {
                        self.base.release_reservation(&market_id).await;
                        continue;
                    }

                    // Determine order type and price
                    let (order_type, up_price, down_price) =
                        if self.base.config.order.hybrid_mode {
                            let offset = self.base.config.order.limit_offset;
                            (
                                OrderType::Gtc,
                                (opps[0].buy_price - offset).max(Decimal::new(1, 2)),
                                (opps[1].buy_price - offset).max(Decimal::new(1, 2)),
                            )
                        } else {
                            (OrderType::Fok, opps[0].buy_price, opps[1].buy_price)
                        };

                    // Get market info for tick_size and fee_rate_bps
                    let markets = self.base.active_markets.read().await;
                    let market = markets.get(&market_id);
                    let tick_size = market
                        .map(|m| m.market.tick_size)
                        .unwrap_or_else(|| Decimal::new(1, 2));
                    let fee_rate_bps = market.map(|m| m.market.fee_rate_bps).unwrap_or(0);
                    let neg_risk = market.map(|m| m.market.neg_risk).unwrap_or(false);
                    drop(markets);

                    let batch_orders = vec![
                        OrderRequest::new(
                            opps[0].token_id.clone(),
                            up_price,
                            share_count,
                            OrderSide::Buy,
                            order_type,
                            neg_risk,
                        )
                        .with_tick_size(tick_size)
                        .with_fee_rate_bps(fee_rate_bps),
                        OrderRequest::new(
                            opps[1].token_id.clone(),
                            down_price,
                            share_count,
                            OrderSide::Buy,
                            order_type,
                            neg_risk,
                        )
                        .with_tick_size(tick_size)
                        .with_fee_rate_bps(fee_rate_bps),
                    ];

                    info!(
                        market = %market_id,
                        combined = %combined_price,
                        net_margin = %opps[0].net_margin,
                        "Submitting TwoSided batch order"
                    );

                    // Consume reservation and track pending orders
                    self.base.consume_reservation(&market_id).await;
                    {
                        let markets = self.base.active_markets.read().await;
                        if let Some(market) = markets.get(&market_id) {
                            let mut pending = self.base.pending_orders.write().await;
                            for opp in &opps {
                                let actual_price = if opp.outcome_to_buy == OutcomeSide::Up {
                                    up_price
                                } else {
                                    down_price
                                };
                                pending.insert(
                                    opp.token_id.clone(),
                                    PendingOrder {
                                        market_id: market_id.clone(),
                                        token_id: opp.token_id.clone(),
                                        side: opp.outcome_to_buy,
                                        price: actual_price,
                                        size: share_count,
                                        reference_price: market.reference_price,
                                        coin: market.coin.clone(),
                                        order_type,
                                        mode: ArbitrageMode::TwoSided,
                                        kelly_fraction: None,
                                        estimated_fee: opp.estimated_fee,
                                        tick_size: market.market.tick_size,
                                        fee_rate_bps: market.market.fee_rate_bps,
                                    },
                                );
                            }
                        }
                    }

                    result.push(Action::PlaceBatchOrder(batch_orders));
                }

                result
            }

            Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot)) => {
                // Update cached asks
                if let Some(best_ask) = snapshot.asks.first() {
                    let mut cached = self.base.cached_asks.write().await;
                    cached.insert(snapshot.token_id.clone(), best_ask.price);
                }

                // TwoSided positions don't need stop-loss (guaranteed profit)
                vec![]
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
                    && lo.mode == ArbitrageMode::TwoSided
                {
                    lo.size = *remaining_size;
                    info!(
                        order_id = %order_id,
                        filled = %filled_size,
                        remaining = %remaining_size,
                        "TwoSided GTC order partially filled"
                    );
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::Cancelled(order_id)) => {
                let mut limits = self.base.open_limit_orders.write().await;
                if let Some(lo) = limits.remove(order_id)
                    && lo.mode == ArbitrageMode::TwoSided
                {
                    info!(
                        order_id = %order_id,
                        market = %lo.market_id,
                        "TwoSided GTC order cancelled"
                    );
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::CancelFailed { order_id, reason }) => {
                let (_found, fill_actions) = self.base.handle_cancel_failed(order_id, reason).await;
                fill_actions
            }

            Event::System(SystemEvent::OpenOrderSnapshot(ids)) => {
                let id_set: std::collections::HashSet<String> =
                    ids.iter().cloned().collect();
                self.base.reconcile_limit_orders(&id_set).await
            }

            Event::OrderUpdate(OrderEvent::Rejected { token_id, .. }) => {
                if let Some(token_id) = token_id {
                    let mut pending = self.base.pending_orders.write().await;
                    if let Some(p) = pending.get(token_id)
                        && p.mode == ArbitrageMode::TwoSided
                    {
                        pending.remove(token_id);
                        warn!(
                            token_id = %token_id,
                            "TwoSided pending order rejected"
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
        info!("TwoSided strategy stopping");
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

    async fn make_twosided(time_remaining: i64) -> (TwoSidedStrategy, StrategyContext) {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.twosided.enabled = true;
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

        let ctx = StrategyContext::new();
        let strategy = TwoSidedStrategy::new(base);
        (strategy, ctx)
    }

    #[tokio::test]
    async fn twosided_generates_two_opportunities() {
        let (strategy, ctx) = make_twosided(600).await;

        // Both asks below combined threshold: 0.47 + 0.48 = 0.95 < 0.98
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_up".to_string(),
                    bids: vec![],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.47),
                        size: dec!(100),
                    }],
                    timestamp: Utc::now(),
                },
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_down".to_string(),
                    bids: vec![],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.48),
                        size: dec!(100),
                    }],
                    timestamp: Utc::now(),
                },
            );
        }

        let opps = strategy
            .evaluate_opportunity(&"market1".to_string(), &ctx)
            .await;
        assert!(opps.is_some());
        let opps = opps.unwrap();
        assert_eq!(opps.len(), 2);
        assert_eq!(opps[0].outcome_to_buy, OutcomeSide::Up);
        assert_eq!(opps[1].outcome_to_buy, OutcomeSide::Down);
        assert_eq!(opps[0].buy_price, dec!(0.47));
        assert_eq!(opps[1].buy_price, dec!(0.48));
        assert_eq!(opps[0].mode, ArbitrageMode::TwoSided);
    }

    #[tokio::test]
    async fn twosided_skips_above_combined_threshold() {
        let (strategy, ctx) = make_twosided(600).await;

        // Combined = 0.50 + 0.50 = 1.00 >= 0.98
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_up".to_string(),
                    bids: vec![],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.50),
                        size: dec!(100),
                    }],
                    timestamp: Utc::now(),
                },
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_down".to_string(),
                    bids: vec![],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.50),
                        size: dec!(100),
                    }],
                    timestamp: Utc::now(),
                },
            );
        }

        let opps = strategy
            .evaluate_opportunity(&"market1".to_string(), &ctx)
            .await;
        assert!(opps.is_none());
    }

    #[tokio::test]
    async fn twosided_skips_expired_market() {
        let (strategy, ctx) = make_twosided(-10).await; // Already expired

        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_up".to_string(),
                    bids: vec![],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.40),
                        size: dec!(100),
                    }],
                    timestamp: Utc::now(),
                },
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_down".to_string(),
                    bids: vec![],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.40),
                        size: dec!(100),
                    }],
                    timestamp: Utc::now(),
                },
            );
        }

        let opps = strategy
            .evaluate_opportunity(&"market1".to_string(), &ctx)
            .await;
        assert!(opps.is_none());
    }

    #[tokio::test]
    async fn twosided_equal_profit_margin_both_legs() {
        let (strategy, ctx) = make_twosided(600).await;

        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_up".to_string(),
                    bids: vec![],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.45),
                        size: dec!(100),
                    }],
                    timestamp: Utc::now(),
                },
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                polyrust_core::types::OrderbookSnapshot {
                    token_id: "token_down".to_string(),
                    bids: vec![],
                    asks: vec![polyrust_core::types::OrderbookLevel {
                        price: dec!(0.45),
                        size: dec!(100),
                    }],
                    timestamp: Utc::now(),
                },
            );
        }

        let opps = strategy
            .evaluate_opportunity(&"market1".to_string(), &ctx)
            .await;
        assert!(opps.is_some());
        let opps = opps.unwrap();
        // Both legs share the same profit_margin = 1 - 0.90 = 0.10
        assert_eq!(opps[0].profit_margin, opps[1].profit_margin);
        assert_eq!(opps[0].profit_margin, dec!(0.10));
    }
}
