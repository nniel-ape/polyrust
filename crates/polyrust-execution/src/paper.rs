use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tracing::{debug, info};
use uuid::Uuid;

use polyrust_core::error::{PolyError, Result};
use polyrust_core::execution::{RedeemRequest, RedeemResult};
use polyrust_core::types::*;

/// Fill mode for paper trading orders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillMode {
    /// Orders fill instantly at the requested price.
    Immediate,
    /// Orders remain pending until matched against an orderbook snapshot.
    Orderbook,
}

/// A simulated order tracked by the paper backend.
#[derive(Debug, Clone)]
struct PaperOrder {
    id: OrderId,
    token_id: TokenId,
    side: OrderSide,
    price: Decimal,
    size: Decimal,
    filled_size: Decimal,
    status: OrderStatus,
    created_at: DateTime<Utc>,
}

/// Tracked position metadata for the paper backend.
#[derive(Debug, Clone)]
struct PaperPosition {
    id: Uuid,
    token_id: TokenId,
    size: Decimal,
    entry_price: Decimal,
    entry_time: DateTime<Utc>,
}

/// Internal state for the paper trading backend.
struct PaperState {
    usdc_balance: Decimal,
    /// token_id -> tracked position with metadata
    positions: HashMap<TokenId, PaperPosition>,
    open_orders: HashMap<OrderId, PaperOrder>,
}

/// Paper trading execution backend that simulates order fills without real money.
pub struct PaperBackend {
    state: Arc<RwLock<PaperState>>,
    fill_mode: FillMode,
}

impl PaperBackend {
    /// Create a new paper backend with an initial USDC balance and fill mode.
    pub fn new(initial_balance: Decimal, fill_mode: FillMode) -> Self {
        Self {
            state: Arc::new(RwLock::new(PaperState {
                usdc_balance: initial_balance,
                positions: HashMap::new(),
                open_orders: HashMap::new(),
            })),
            fill_mode,
        }
    }

    /// Process pending orders against a new orderbook snapshot (Orderbook fill mode).
    ///
    /// For BUY orders: fills if order price >= best ask price.
    /// For SELL orders: fills if order price <= best bid price.
    /// Supports partial fills based on available liquidity at each level.
    pub async fn update_orders_with_orderbook(
        &self,
        token_id: &str,
        orderbook: &OrderbookSnapshot,
    ) -> Vec<OrderFill> {
        let mut state = self.state.write().await;
        let mut fills = Vec::new();

        // Collect order IDs for this token to avoid borrow issues
        let matching_order_ids: Vec<OrderId> = state
            .open_orders
            .iter()
            .filter(|(_, o)| {
                o.token_id == token_id
                    && matches!(o.status, OrderStatus::Open | OrderStatus::PartiallyFilled)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for order_id in matching_order_ids {
            let order = match state.open_orders.get(&order_id) {
                Some(o) => o.clone(),
                None => continue,
            };

            let remaining = order.size - order.filled_size;
            if remaining <= Decimal::ZERO {
                continue;
            }

            let fill_result = match order.side {
                OrderSide::Buy => try_fill_buy(&order, remaining, &orderbook.asks),
                OrderSide::Sell => try_fill_sell(&order, remaining, &orderbook.bids),
            };

            if let Some((fill_size, fill_price)) = fill_result {
                // Update the order
                let paper_order = state.open_orders.get_mut(&order_id).unwrap();
                paper_order.filled_size += fill_size;

                if paper_order.filled_size >= paper_order.size {
                    paper_order.status = OrderStatus::Filled;
                } else {
                    paper_order.status = OrderStatus::PartiallyFilled;
                }

                // Update positions and balance
                match order.side {
                    OrderSide::Buy => {
                        // Balance was already deducted at order price when placed.
                        // Refund difference if filled at a better (lower) price.
                        let price_improvement = order.price - fill_price;
                        if price_improvement > Decimal::ZERO {
                            state.usdc_balance += price_improvement * fill_size;
                        }
                        let pos = state
                            .positions
                            .entry(order.token_id.clone())
                            .or_insert_with(|| PaperPosition {
                                id: Uuid::new_v4(),
                                token_id: order.token_id.clone(),
                                size: Decimal::ZERO,
                                entry_price: fill_price,
                                entry_time: Utc::now(),
                            });
                        // Weighted average entry price
                        let total_cost = pos.entry_price * pos.size + fill_price * fill_size;
                        pos.size += fill_size;
                        if pos.size > Decimal::ZERO {
                            pos.entry_price = total_cost / pos.size;
                        }
                    }
                    OrderSide::Sell => {
                        // Position was already deducted when order was placed
                        state.usdc_balance += fill_price * fill_size;
                    }
                }

                fills.push(OrderFill {
                    order_id: order_id.clone(),
                    token_id: order.token_id.clone(),
                    side: order.side,
                    price: fill_price,
                    size: fill_size,
                });

                debug!(
                    order_id = %order_id,
                    fill_size = %fill_size,
                    fill_price = %fill_price,
                    "Paper order filled via orderbook"
                );
            }

            // Remove fully filled orders from open_orders
            if state
                .open_orders
                .get(&order_id)
                .is_some_and(|o| o.status == OrderStatus::Filled)
            {
                state.open_orders.remove(&order_id);
            }
        }

        fills
    }
}

/// Result of an orderbook-based fill.
#[derive(Debug, Clone)]
pub struct OrderFill {
    pub order_id: OrderId,
    pub token_id: TokenId,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
}

/// Try to fill a BUY order against ask levels.
/// Returns (fill_size, weighted_avg_price) if any fill occurs.
fn try_fill_buy(
    order: &PaperOrder,
    remaining: Decimal,
    asks: &[OrderbookLevel],
) -> Option<(Decimal, Decimal)> {
    let mut filled = Decimal::ZERO;
    let mut cost = Decimal::ZERO;

    for level in asks {
        if level.price > order.price {
            break; // Asks are sorted ascending; no more matchable levels
        }
        let available = remaining - filled;
        if available <= Decimal::ZERO {
            break;
        }
        let fill_at_level = available.min(level.size);
        filled += fill_at_level;
        cost += fill_at_level * level.price;
    }

    if filled > Decimal::ZERO {
        Some((filled, cost / filled))
    } else {
        None
    }
}

/// Try to fill a SELL order against bid levels.
/// Returns (fill_size, weighted_avg_price) if any fill occurs.
fn try_fill_sell(
    order: &PaperOrder,
    remaining: Decimal,
    bids: &[OrderbookLevel],
) -> Option<(Decimal, Decimal)> {
    let mut filled = Decimal::ZERO;
    let mut revenue = Decimal::ZERO;

    for level in bids {
        if level.price < order.price {
            break; // Bids are sorted descending; no more matchable levels
        }
        let available = remaining - filled;
        if available <= Decimal::ZERO {
            break;
        }
        let fill_at_level = available.min(level.size);
        filled += fill_at_level;
        revenue += fill_at_level * level.price;
    }

    if filled > Decimal::ZERO {
        Some((filled, revenue / filled))
    } else {
        None
    }
}

#[async_trait]
impl polyrust_core::execution::ExecutionBackend for PaperBackend {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        use crate::rounding::{round_price, round_size};

        let mut state = self.state.write().await;
        let order_id = Uuid::new_v4().to_string();

        // Apply same rounding as live backend for simulation fidelity
        let price = round_price(order.price);
        let size = round_size(order.size);

        // Validate price range
        if price <= Decimal::ZERO || price > Decimal::ONE {
            return Ok(OrderResult {
                success: false,
                order_id: None,
                token_id: order.token_id.clone(),
                price,
                size,
                side: order.side,
                status: None,
                message: format!("Invalid price {price}: must be in (0, 1]"),
            });
        }
        if size <= Decimal::ZERO {
            return Ok(OrderResult {
                success: false,
                order_id: None,
                token_id: order.token_id.clone(),
                price,
                size,
                side: order.side,
                status: None,
                message: format!("Invalid size {size}: must be > 0"),
            });
        }

        match order.side {
            OrderSide::Buy => {
                let cost = price * size;
                if state.usdc_balance < cost {
                    return Ok(OrderResult {
                        success: false,
                        order_id: None,
                        token_id: order.token_id.clone(),
                        price,
                        size,
                        side: order.side,
                        status: None,
                        message: format!(
                            "Insufficient balance: need {} USDC, have {}",
                            cost, state.usdc_balance
                        ),
                    });
                }

                // Deduct balance immediately (locked for order)
                state.usdc_balance -= cost;

                match self.fill_mode {
                    FillMode::Immediate => {
                        // Fill instantly: add shares to position
                        let pos = state
                            .positions
                            .entry(order.token_id.clone())
                            .or_insert_with(|| PaperPosition {
                                id: Uuid::new_v4(),
                                token_id: order.token_id.clone(),
                                size: Decimal::ZERO,
                                entry_price: price,
                                entry_time: Utc::now(),
                            });
                        // Weighted average entry price
                        let total_cost = pos.entry_price * pos.size + price * size;
                        pos.size += size;
                        if pos.size > Decimal::ZERO {
                            pos.entry_price = total_cost / pos.size;
                        }

                        info!(
                            order_id = %order_id,
                            token_id = %order.token_id,
                            price = %price,
                            size = %size,
                            "Paper BUY filled immediately"
                        );
                    }
                    FillMode::Orderbook => {
                        // Add to pending orders, wait for orderbook match
                        state.open_orders.insert(
                            order_id.clone(),
                            PaperOrder {
                                id: order_id.clone(),
                                token_id: order.token_id.clone(),
                                side: order.side,
                                price,
                                size,
                                filled_size: Decimal::ZERO,
                                status: OrderStatus::Open,
                                created_at: Utc::now(),
                            },
                        );

                        debug!(
                            order_id = %order_id,
                            "Paper BUY order queued for orderbook matching"
                        );
                    }
                }
            }
            OrderSide::Sell => {
                let current_size = state
                    .positions
                    .get(&order.token_id)
                    .map(|p| p.size)
                    .unwrap_or(Decimal::ZERO);

                if current_size < size {
                    return Ok(OrderResult {
                        success: false,
                        order_id: None,
                        token_id: order.token_id.clone(),
                        price,
                        size,
                        side: order.side,
                        status: None,
                        message: format!(
                            "Insufficient position: need {} shares, have {}",
                            size, current_size
                        ),
                    });
                }

                // Deduct position immediately (locked for order)
                if let Some(pos) = state.positions.get_mut(&order.token_id) {
                    pos.size -= size;
                    // Clean up zero positions
                    if pos.size == Decimal::ZERO {
                        state.positions.remove(&order.token_id);
                    }
                }

                match self.fill_mode {
                    FillMode::Immediate => {
                        // Fill instantly: add USDC revenue
                        state.usdc_balance += price * size;

                        info!(
                            order_id = %order_id,
                            token_id = %order.token_id,
                            price = %price,
                            size = %size,
                            "Paper SELL filled immediately"
                        );
                    }
                    FillMode::Orderbook => {
                        state.open_orders.insert(
                            order_id.clone(),
                            PaperOrder {
                                id: order_id.clone(),
                                token_id: order.token_id.clone(),
                                side: order.side,
                                price,
                                size,
                                filled_size: Decimal::ZERO,
                                status: OrderStatus::Open,
                                created_at: Utc::now(),
                            },
                        );

                        debug!(
                            order_id = %order_id,
                            "Paper SELL order queued for orderbook matching"
                        );
                    }
                }
            }
        }

        let status = match self.fill_mode {
            FillMode::Immediate => "Filled",
            FillMode::Orderbook => "Open",
        };
        Ok(OrderResult {
            success: true,
            order_id: Some(order_id),
            token_id: order.token_id.clone(),
            price,
            size,
            side: order.side,
            status: Some(status.to_string()),
            message: "ok".to_string(),
        })
    }

    async fn place_batch_orders(&self, orders: &[OrderRequest]) -> Result<Vec<OrderResult>> {
        // Process all orders sequentially under the same logical flow.
        // Each place_order call already acquires the lock atomically, and
        // balance is deducted per-order, so earlier orders in the batch
        // can affect whether later ones have sufficient balance.
        let mut results = Vec::with_capacity(orders.len());
        for order in orders {
            results.push(self.place_order(order).await?);
        }
        Ok(results)
    }

    async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let mut state = self.state.write().await;

        let order = state
            .open_orders
            .remove(order_id)
            .ok_or_else(|| PolyError::Execution(format!("Order not found: {order_id}")))?;

        // Restore locked resources for unfilled portion
        let unfilled = order.size - order.filled_size;
        match order.side {
            OrderSide::Buy => {
                // Restore locked USDC for unfilled portion
                state.usdc_balance += order.price * unfilled;
            }
            OrderSide::Sell => {
                // Restore locked shares for unfilled portion
                let pos = state
                    .positions
                    .entry(order.token_id.clone())
                    .or_insert_with(|| PaperPosition {
                        id: Uuid::new_v4(),
                        token_id: order.token_id.clone(),
                        size: Decimal::ZERO,
                        entry_price: order.price,
                        entry_time: Utc::now(),
                    });
                pos.size += unfilled;
            }
        }

        info!(order_id, "Paper order cancelled");
        Ok(())
    }

    async fn cancel_all_orders(&self) -> Result<()> {
        let mut state = self.state.write().await;

        let orders: Vec<PaperOrder> = state.open_orders.values().cloned().collect();
        state.open_orders.clear();

        for order in &orders {
            let unfilled = order.size - order.filled_size;
            match order.side {
                OrderSide::Buy => {
                    state.usdc_balance += order.price * unfilled;
                }
                OrderSide::Sell => {
                    let pos = state
                        .positions
                        .entry(order.token_id.clone())
                        .or_insert_with(|| PaperPosition {
                            id: Uuid::new_v4(),
                            token_id: order.token_id.clone(),
                            size: Decimal::ZERO,
                            entry_price: order.price,
                            entry_time: Utc::now(),
                        });
                    pos.size += unfilled;
                }
            }
        }

        info!(count = orders.len(), "All paper orders cancelled");
        Ok(())
    }

    async fn get_open_orders(&self) -> Result<Vec<Order>> {
        let state = self.state.read().await;
        let orders = state
            .open_orders
            .values()
            .map(|o| Order {
                id: o.id.clone(),
                token_id: o.token_id.clone(),
                side: o.side,
                price: o.price,
                size: o.size,
                filled_size: o.filled_size,
                status: o.status,
                created_at: o.created_at,
            })
            .collect();
        Ok(orders)
    }

    async fn get_positions(&self) -> Result<Vec<Position>> {
        let state = self.state.read().await;
        let positions = state
            .positions
            .values()
            .filter(|pos| pos.size > Decimal::ZERO)
            .map(|pos| Position {
                id: pos.id,
                market_id: String::new(),
                token_id: pos.token_id.clone(),
                side: OutcomeSide::Yes,
                entry_price: pos.entry_price,
                size: pos.size,
                current_price: Decimal::ZERO, // Updated by engine from market data
                entry_time: pos.entry_time,
                strategy_name: "paper".to_string(),
            })
            .collect();
        Ok(positions)
    }

    async fn get_balance(&self) -> Result<Decimal> {
        let state = self.state.read().await;
        Ok(state.usdc_balance)
    }

    async fn is_market_resolved(&self, _condition_id: &str) -> Result<bool> {
        // In paper mode, markets resolve instantly (no UMA dispute window simulation)
        Ok(true)
    }

    async fn redeem_positions(&self, request: &RedeemRequest) -> Result<RedeemResult> {
        let mut state = self.state.write().await;

        // Find positions for this market's tokens
        let mut total_redeemed = Decimal::ZERO;
        for token_id in &request.token_ids {
            if let Some(pos) = state.positions.remove(token_id) {
                // Assume winning outcome pays 1 USDC per share
                let redemption_value = pos.size;
                state.usdc_balance += redemption_value;
                total_redeemed += redemption_value;
                info!(
                    token_id = %token_id,
                    size = %pos.size,
                    value = %redemption_value,
                    "Paper redemption: position redeemed"
                );
            }
        }

        if total_redeemed == Decimal::ZERO {
            return Ok(RedeemResult {
                market_id: request.market_id.clone(),
                tx_hash: format!("paper-{}", Uuid::new_v4()),
                success: false,
                message: "No positions found to redeem".to_string(),
            });
        }

        info!(
            market_id = %request.market_id,
            total_redeemed = %total_redeemed,
            new_balance = %state.usdc_balance,
            "Paper redemption: completed"
        );

        Ok(RedeemResult {
            market_id: request.market_id.clone(),
            tx_hash: format!("paper-{}", Uuid::new_v4()),
            success: true,
            message: format!("Redeemed {} USDC", total_redeemed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polyrust_core::execution::ExecutionBackend;
    use rust_decimal_macros::dec;

    fn buy_order(token_id: &str, price: Decimal, size: Decimal) -> OrderRequest {
        OrderRequest::new(
            token_id.to_string(),
            price,
            size,
            OrderSide::Buy,
            OrderType::Gtc,
            false,
        )
    }

    fn sell_order(token_id: &str, price: Decimal, size: Decimal) -> OrderRequest {
        OrderRequest::new(
            token_id.to_string(),
            price,
            size,
            OrderSide::Sell,
            OrderType::Gtc,
            false,
        )
    }

    fn make_orderbook(
        token_id: &str,
        bids: Vec<(Decimal, Decimal)>,
        asks: Vec<(Decimal, Decimal)>,
    ) -> OrderbookSnapshot {
        OrderbookSnapshot {
            token_id: token_id.to_string(),
            bids: bids
                .into_iter()
                .map(|(price, size)| OrderbookLevel { price, size })
                .collect(),
            asks: asks
                .into_iter()
                .map(|(price, size)| OrderbookLevel { price, size })
                .collect(),
            timestamp: Utc::now(),
        }
    }

    // --- Immediate fill mode tests ---

    #[tokio::test]
    async fn buy_order_sufficient_balance_succeeds() {
        let backend = PaperBackend::new(dec!(1000), FillMode::Immediate);

        let result = backend
            .place_order(&buy_order("token1", dec!(0.50), dec!(10)))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.order_id.is_some());

        // Balance reduced by price * size = 0.50 * 10 = 5.00
        let balance = backend.get_balance().await.unwrap();
        assert_eq!(balance, dec!(995));

        // Position created with 10 shares
        let positions = backend.get_positions().await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].size, dec!(10));
        assert_eq!(positions[0].token_id, "token1");
    }

    #[tokio::test]
    async fn buy_order_insufficient_balance_fails() {
        let backend = PaperBackend::new(dec!(1), FillMode::Immediate);

        let result = backend
            .place_order(&buy_order("token1", dec!(0.50), dec!(10)))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.message.contains("Insufficient balance"));

        // Balance unchanged
        let balance = backend.get_balance().await.unwrap();
        assert_eq!(balance, dec!(1));

        // No position created
        let positions = backend.get_positions().await.unwrap();
        assert!(positions.is_empty());
    }

    #[tokio::test]
    async fn sell_order_with_position_succeeds() {
        let backend = PaperBackend::new(dec!(1000), FillMode::Immediate);

        // First buy to create position
        backend
            .place_order(&buy_order("token1", dec!(0.40), dec!(20)))
            .await
            .unwrap();
        let balance_after_buy = backend.get_balance().await.unwrap();
        assert_eq!(balance_after_buy, dec!(992)); // 1000 - 0.40*20

        // Sell 10 shares at 0.60
        let result = backend
            .place_order(&sell_order("token1", dec!(0.60), dec!(10)))
            .await
            .unwrap();
        assert!(result.success);

        // Balance increased by 0.60 * 10 = 6.00
        let balance = backend.get_balance().await.unwrap();
        assert_eq!(balance, dec!(998)); // 992 + 6

        // Position reduced to 10 shares
        let positions = backend.get_positions().await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].size, dec!(10));
    }

    #[tokio::test]
    async fn sell_order_no_position_fails() {
        let backend = PaperBackend::new(dec!(1000), FillMode::Immediate);

        let result = backend
            .place_order(&sell_order("token1", dec!(0.50), dec!(10)))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.message.contains("Insufficient position"));
    }

    #[tokio::test]
    async fn cancel_order_restores_locked_balance() {
        let backend = PaperBackend::new(dec!(100), FillMode::Orderbook);

        // Place a buy order (locks 0.50 * 10 = 5.00 USDC)
        let result = backend
            .place_order(&buy_order("token1", dec!(0.50), dec!(10)))
            .await
            .unwrap();
        assert!(result.success);
        let order_id = result.order_id.unwrap();

        assert_eq!(backend.get_balance().await.unwrap(), dec!(95));

        // Cancel it
        backend.cancel_order(&order_id).await.unwrap();

        // Balance restored
        assert_eq!(backend.get_balance().await.unwrap(), dec!(100));

        // No open orders
        let orders = backend.get_open_orders().await.unwrap();
        assert!(orders.is_empty());
    }

    #[tokio::test]
    async fn cancel_all_orders_cancels_all() {
        let backend = PaperBackend::new(dec!(100), FillMode::Orderbook);

        // Place two buy orders
        backend
            .place_order(&buy_order("token1", dec!(0.40), dec!(10)))
            .await
            .unwrap();
        backend
            .place_order(&buy_order("token2", dec!(0.30), dec!(10)))
            .await
            .unwrap();

        // Balance reduced by 4.00 + 3.00 = 7.00
        assert_eq!(backend.get_balance().await.unwrap(), dec!(93));

        // 2 open orders
        assert_eq!(backend.get_open_orders().await.unwrap().len(), 2);

        // Cancel all
        backend.cancel_all_orders().await.unwrap();

        // Balance restored
        assert_eq!(backend.get_balance().await.unwrap(), dec!(100));
        assert!(backend.get_open_orders().await.unwrap().is_empty());
    }

    // --- Orderbook fill mode tests ---

    #[tokio::test]
    async fn orderbook_fill_buy_at_ask() {
        let backend = PaperBackend::new(dec!(100), FillMode::Orderbook);

        // Place buy order at 0.55
        let result = backend
            .place_order(&buy_order("token1", dec!(0.55), dec!(10)))
            .await
            .unwrap();
        assert!(result.success);

        // Create orderbook with ask at 0.50 (below our bid -> should fill)
        let ob = make_orderbook(
            "token1",
            vec![(dec!(0.48), dec!(100))],
            vec![(dec!(0.50), dec!(20))],
        );

        let fills = backend.update_orders_with_orderbook("token1", &ob).await;
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].size, dec!(10));
        assert_eq!(fills[0].price, dec!(0.50));

        // Position should now exist
        let positions = backend.get_positions().await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].size, dec!(10));

        // No more open orders
        assert!(backend.get_open_orders().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn orderbook_fill_sell_at_bid() {
        let backend = PaperBackend::new(dec!(100), FillMode::Immediate);

        // Create position first (using Immediate to simplify setup)
        backend
            .place_order(&buy_order("token1", dec!(0.40), dec!(20)))
            .await
            .unwrap();

        // Now create an Orderbook backend sharing the same state
        // Instead, let's use a single Orderbook backend and set up the position manually
        let ob_backend = PaperBackend::new(dec!(100), FillMode::Orderbook);

        // Buy immediately doesn't work in orderbook mode, so set up state directly
        {
            let mut state = ob_backend.state.write().await;
            state.positions.insert(
                "token1".to_string(),
                PaperPosition {
                    id: Uuid::new_v4(),
                    token_id: "token1".to_string(),
                    size: dec!(20),
                    entry_price: dec!(0.40),
                    entry_time: Utc::now(),
                },
            );
        }

        // Place sell order at 0.50
        let result = ob_backend
            .place_order(&sell_order("token1", dec!(0.50), dec!(10)))
            .await
            .unwrap();
        assert!(result.success);

        // Create orderbook with bid at 0.55 (above our ask -> should fill)
        let ob = make_orderbook(
            "token1",
            vec![(dec!(0.55), dec!(20))],
            vec![(dec!(0.60), dec!(100))],
        );

        let fills = ob_backend.update_orders_with_orderbook("token1", &ob).await;
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].size, dec!(10));
        assert_eq!(fills[0].price, dec!(0.55));

        // Balance should increase by 0.55 * 10 = 5.50
        let balance = ob_backend.get_balance().await.unwrap();
        assert_eq!(balance, dec!(105.50));
    }

    #[tokio::test]
    async fn immediate_fill_mode_fills_instantly() {
        let backend = PaperBackend::new(dec!(100), FillMode::Immediate);

        let result = backend
            .place_order(&buy_order("token1", dec!(0.50), dec!(10)))
            .await
            .unwrap();
        assert!(result.success);

        // No open orders (filled immediately)
        assert!(backend.get_open_orders().await.unwrap().is_empty());

        // Position exists immediately
        let positions = backend.get_positions().await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].size, dec!(10));
    }

    #[tokio::test]
    async fn partial_fill_tracking() {
        let backend = PaperBackend::new(dec!(100), FillMode::Orderbook);

        // Place buy order for 20 shares at 0.55
        let result = backend
            .place_order(&buy_order("token1", dec!(0.55), dec!(20)))
            .await
            .unwrap();
        assert!(result.success);

        // Orderbook with only 8 shares available at ask
        let ob = make_orderbook("token1", vec![], vec![(dec!(0.50), dec!(8))]);

        let fills = backend.update_orders_with_orderbook("token1", &ob).await;
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].size, dec!(8));

        // Order should still be open (partially filled)
        let orders = backend.get_open_orders().await.unwrap();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].filled_size, dec!(8));
        assert_eq!(orders[0].status, OrderStatus::PartiallyFilled);

        // Position should have 8 shares
        let positions = backend.get_positions().await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].size, dec!(8));

        // Feed another orderbook with 12 more shares
        let ob2 = make_orderbook("token1", vec![], vec![(dec!(0.52), dec!(15))]);
        let fills2 = backend.update_orders_with_orderbook("token1", &ob2).await;
        assert_eq!(fills2.len(), 1);
        assert_eq!(fills2[0].size, dec!(12)); // remaining 12

        // Order should now be fully filled and removed
        assert!(backend.get_open_orders().await.unwrap().is_empty());

        // Position should now have 20 shares
        let positions2 = backend.get_positions().await.unwrap();
        assert_eq!(positions2.len(), 1);
        assert_eq!(positions2[0].size, dec!(20));
    }

    #[tokio::test]
    async fn cancel_sell_order_restores_position() {
        let backend = PaperBackend::new(dec!(100), FillMode::Orderbook);

        // Set up position manually
        {
            let mut state = backend.state.write().await;
            state.positions.insert(
                "token1".to_string(),
                PaperPosition {
                    id: Uuid::new_v4(),
                    token_id: "token1".to_string(),
                    size: dec!(20),
                    entry_price: dec!(0.50),
                    entry_time: Utc::now(),
                },
            );
        }

        // Place sell order for 10 shares
        let result = backend
            .place_order(&sell_order("token1", dec!(0.60), dec!(10)))
            .await
            .unwrap();
        assert!(result.success);
        let order_id = result.order_id.unwrap();

        // Position should be reduced to 10
        let positions = backend.get_positions().await.unwrap();
        assert_eq!(positions[0].size, dec!(10));

        // Cancel the sell order
        backend.cancel_order(&order_id).await.unwrap();

        // Position restored to 20
        let positions2 = backend.get_positions().await.unwrap();
        assert_eq!(positions2[0].size, dec!(20));
    }

    // --- Batch order tests ---

    #[tokio::test]
    async fn batch_with_two_orders_produces_two_results() {
        let backend = PaperBackend::new(dec!(1000), FillMode::Immediate);

        let orders = vec![
            buy_order("token1", dec!(0.50), dec!(10)),
            buy_order("token2", dec!(0.40), dec!(10)),
        ];
        let results = backend.place_batch_orders(&orders).await.unwrap();

        assert_eq!(results.len(), 2);
        assert!(results[0].success);
        assert!(results[1].success);
        assert_eq!(results[0].token_id, "token1");
        assert_eq!(results[1].token_id, "token2");
    }

    #[tokio::test]
    async fn batch_deducts_balance_correctly() {
        let backend = PaperBackend::new(dec!(100), FillMode::Immediate);

        let orders = vec![
            buy_order("token1", dec!(0.50), dec!(10)), // cost = 5.00
            buy_order("token2", dec!(0.40), dec!(10)), // cost = 4.00
        ];
        let results = backend.place_batch_orders(&orders).await.unwrap();

        assert!(results[0].success);
        assert!(results[1].success);

        // Balance should be 100 - 5.00 - 4.00 = 91.00
        let balance = backend.get_balance().await.unwrap();
        assert_eq!(balance, dec!(91));

        // Both positions should exist
        let positions = backend.get_positions().await.unwrap();
        assert_eq!(positions.len(), 2);
    }

    #[tokio::test]
    async fn batch_second_order_fails_on_insufficient_balance() {
        let backend = PaperBackend::new(dec!(6), FillMode::Immediate);

        let orders = vec![
            buy_order("token1", dec!(0.50), dec!(10)), // cost = 5.00 (succeeds)
            buy_order("token2", dec!(0.50), dec!(10)), // cost = 5.00 (fails: only 1.00 left)
        ];
        let results = backend.place_batch_orders(&orders).await.unwrap();

        assert!(results[0].success);
        assert!(!results[1].success);
        assert!(results[1].message.contains("Insufficient balance"));
    }

    // --- Phase 3: New paper backend tests ---

    #[tokio::test]
    async fn concurrent_orders_same_token_no_corruption() {
        let backend = PaperBackend::new(dec!(1000), FillMode::Immediate);

        // Place two buy orders on the same token
        let r1 = backend
            .place_order(&buy_order("token1", dec!(0.50), dec!(10)))
            .await
            .unwrap();
        let r2 = backend
            .place_order(&buy_order("token1", dec!(0.60), dec!(5)))
            .await
            .unwrap();
        assert!(r1.success);
        assert!(r2.success);

        // Position should be combined: 15 shares, weighted avg price
        let positions = backend.get_positions().await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].size, dec!(15));
        // Weighted avg: (0.50*10 + 0.60*5) / 15 = 8.00/15 ≈ 0.5333...
        let expected_avg = (dec!(0.50) * dec!(10) + dec!(0.60) * dec!(5)) / dec!(15);
        assert_eq!(positions[0].entry_price, expected_avg);

        // Balance: 1000 - 5.00 - 3.00 = 992.00
        let balance = backend.get_balance().await.unwrap();
        assert_eq!(balance, dec!(992));
    }

    #[tokio::test]
    async fn cancel_partially_filled_order_restores_unfilled() {
        let backend = PaperBackend::new(dec!(100), FillMode::Orderbook);

        // Place buy order for 20 shares at 0.55
        let result = backend
            .place_order(&buy_order("token1", dec!(0.55), dec!(20)))
            .await
            .unwrap();
        assert!(result.success);
        let order_id = result.order_id.unwrap();

        // Partially fill with 8 shares
        let ob = make_orderbook("token1", vec![], vec![(dec!(0.50), dec!(8))]);
        let fills = backend.update_orders_with_orderbook("token1", &ob).await;
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].size, dec!(8));

        // Balance was deducted 0.55*20=11.00 upfront. 8 shares filled at 0.50
        // price improvement refund: (0.55-0.50)*8 = 0.40
        // balance = 100 - 11.00 + 0.40 = 89.40
        let balance_before = backend.get_balance().await.unwrap();
        assert_eq!(balance_before, dec!(89.40));

        // Cancel remaining 12 shares
        backend.cancel_order(&order_id).await.unwrap();

        // Unfilled = 12 shares at 0.55 → restore 0.55*12 = 6.60
        let balance_after = backend.get_balance().await.unwrap();
        assert_eq!(balance_after, dec!(96.00));

        // No open orders
        assert!(backend.get_open_orders().await.unwrap().is_empty());

        // Position should still have 8 shares
        let positions = backend.get_positions().await.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].size, dec!(8));
    }

    #[tokio::test]
    async fn multi_level_orderbook_matching() {
        let backend = PaperBackend::new(dec!(100), FillMode::Orderbook);

        // Place buy order for 25 shares at 0.60
        let result = backend
            .place_order(&buy_order("token1", dec!(0.60), dec!(25)))
            .await
            .unwrap();
        assert!(result.success);

        // Orderbook with multiple ask levels:
        // Level 1: 10 shares @ 0.50
        // Level 2: 10 shares @ 0.55
        // Level 3: 10 shares @ 0.65 (above order price, won't match)
        let ob = make_orderbook(
            "token1",
            vec![],
            vec![
                (dec!(0.50), dec!(10)),
                (dec!(0.55), dec!(10)),
                (dec!(0.65), dec!(10)),
            ],
        );

        let fills = backend.update_orders_with_orderbook("token1", &ob).await;
        assert_eq!(fills.len(), 1);
        // Should fill 20 shares (10@0.50 + 10@0.55), remaining 5 unfilled (0.65 > 0.60)
        assert_eq!(fills[0].size, dec!(20));
        // Weighted avg: (10*0.50 + 10*0.55) / 20 = 10.50/20 = 0.525
        assert_eq!(fills[0].price, dec!(0.525));

        // Order should still be open with 5 remaining
        let orders = backend.get_open_orders().await.unwrap();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].filled_size, dec!(20));
        assert_eq!(orders[0].status, OrderStatus::PartiallyFilled);
    }

    #[tokio::test]
    async fn redemption_adds_balance() {
        let backend = PaperBackend::new(dec!(100), FillMode::Immediate);

        // Buy 20 shares at 0.50 → costs 10.00
        backend
            .place_order(&buy_order("token1", dec!(0.50), dec!(20)))
            .await
            .unwrap();
        assert_eq!(backend.get_balance().await.unwrap(), dec!(90));

        // Redeem: winning side pays $1 per share → 20 USDC
        let request = polyrust_core::execution::RedeemRequest {
            market_id: "market1".to_string(),
            condition_id: "cond1".to_string(),
            token_ids: vec!["token1".to_string()],
            neg_risk: false,
        };
        let result = backend.redeem_positions(&request).await.unwrap();
        assert!(result.success);
        assert!(result.message.contains("20"));

        // Balance: 90 + 20 = 110
        assert_eq!(backend.get_balance().await.unwrap(), dec!(110));

        // Position removed
        assert!(backend.get_positions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn redemption_no_positions_returns_failure() {
        let backend = PaperBackend::new(dec!(100), FillMode::Immediate);

        let request = polyrust_core::execution::RedeemRequest {
            market_id: "market1".to_string(),
            condition_id: "cond1".to_string(),
            token_ids: vec!["nonexistent".to_string()],
            neg_risk: false,
        };
        let result = backend.redeem_positions(&request).await.unwrap();
        assert!(!result.success);
        assert!(result.message.contains("No positions found"));
    }

    #[tokio::test]
    async fn zero_size_order_rejected() {
        let backend = PaperBackend::new(dec!(100), FillMode::Immediate);

        let result = backend
            .place_order(&buy_order("token1", dec!(0.50), dec!(0)))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.message.contains("Invalid size"));
    }

    #[tokio::test]
    async fn invalid_price_order_rejected() {
        let backend = PaperBackend::new(dec!(100), FillMode::Immediate);

        // Price > 1.0
        let result = backend
            .place_order(&buy_order("token1", dec!(1.50), dec!(10)))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.message.contains("Invalid price"));

        // Price = 0
        let result = backend
            .place_order(&buy_order("token1", dec!(0), dec!(10)))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.message.contains("Invalid price"));
    }
}
