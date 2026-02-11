use crate::error::Result;
use crate::types::*;
use async_trait::async_trait;
use rust_decimal::Decimal;

// Re-export RedeemRequest and RedeemResult for convenience
pub use crate::types::{RedeemRequest, RedeemResult};

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

    /// Place multiple orders as a batch. Default implementation processes sequentially.
    async fn place_batch_orders(&self, orders: &[OrderRequest]) -> Result<Vec<OrderResult>> {
        let mut results = Vec::with_capacity(orders.len());
        for order in orders {
            results.push(self.place_order(order).await?);
        }
        Ok(results)
    }

    /// Check if a market has resolved on-chain (payoutDenominator > 0).
    async fn is_market_resolved(&self, _condition_id: &str) -> Result<bool> {
        Ok(false)
    }

    /// Redeem winning positions after market resolution.
    async fn redeem_positions(&self, _request: &RedeemRequest) -> Result<RedeemResult> {
        Err(crate::error::PolyError::Execution(
            "Redemption not supported".into(),
        ))
    }

    /// Batch-redeem multiple resolved positions. Default: sequential fallback.
    async fn redeem_positions_batch(
        &self,
        requests: &[RedeemRequest],
    ) -> Result<Vec<RedeemResult>> {
        let mut results = Vec::with_capacity(requests.len());
        for request in requests {
            results.push(self.redeem_positions(request).await?);
        }
        Ok(results)
    }
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
    async fn place_batch_orders(&self, orders: &[OrderRequest]) -> Result<Vec<OrderResult>> {
        (**self).place_batch_orders(orders).await
    }
    async fn is_market_resolved(&self, condition_id: &str) -> Result<bool> {
        (**self).is_market_resolved(condition_id).await
    }
    async fn redeem_positions(&self, request: &RedeemRequest) -> Result<RedeemResult> {
        (**self).redeem_positions(request).await
    }
    async fn redeem_positions_batch(
        &self,
        requests: &[RedeemRequest],
    ) -> Result<Vec<RedeemResult>> {
        (**self).redeem_positions_batch(requests).await
    }
}
