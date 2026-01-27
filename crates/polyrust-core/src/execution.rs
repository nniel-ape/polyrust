use crate::error::Result;
use crate::types::*;
use async_trait::async_trait;
use rust_decimal::Decimal;

/// Abstraction over order execution.
///
/// `LiveBackend` sends real orders to Polymarket via rs-clob-client.
/// `PaperBackend` simulates fills against orderbook snapshots.
#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    /// Place an order. Returns the result (success/failure + order ID).
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult>;

    /// Cancel a specific order by ID.
    async fn cancel_order(&self, order_id: &str) -> Result<()>;

    /// Cancel all open orders.
    async fn cancel_all_orders(&self) -> Result<()>;

    /// Get all currently open orders.
    async fn get_open_orders(&self) -> Result<Vec<Order>>;

    /// Get current positions.
    async fn get_positions(&self) -> Result<Vec<Position>>;

    /// Get available USDC balance.
    async fn get_balance(&self) -> Result<Decimal>;
}

#[async_trait]
impl ExecutionBackend for Box<dyn ExecutionBackend> {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        (**self).place_order(order).await
    }
    async fn cancel_order(&self, order_id: &str) -> Result<()> {
        (**self).cancel_order(order_id).await
    }
    async fn cancel_all_orders(&self) -> Result<()> {
        (**self).cancel_all_orders().await
    }
    async fn get_open_orders(&self) -> Result<Vec<Order>> {
        (**self).get_open_orders().await
    }
    async fn get_positions(&self) -> Result<Vec<Position>> {
        (**self).get_positions().await
    }
    async fn get_balance(&self) -> Result<Decimal> {
        (**self).get_balance().await
    }
}
