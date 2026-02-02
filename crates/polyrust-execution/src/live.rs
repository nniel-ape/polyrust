use std::str::FromStr;

use async_trait::async_trait;
use polymarket_client_sdk::POLYGON;
use polymarket_client_sdk::auth::{LocalSigner, Signer};
use polymarket_client_sdk::clob::types::request::{BalanceAllowanceRequest, OrdersRequest};
use polymarket_client_sdk::clob::types::{
    OrderType as SdkOrderType, Side as SdkSide, SignatureType,
};
use polymarket_client_sdk::clob::{Client, Config as SdkConfig};
use polymarket_client_sdk::types::{Address as SdkAddress, U256 as SdkU256};
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use polyrust_core::config::Config;
use polyrust_core::error::{PolyError, Result};
use polyrust_core::types::*;

use crate::rounding::{round_price_with_decimals, round_size_with_decimals};

/// Rounding configuration for order amounts.
#[derive(Debug, Clone, Copy)]
pub struct RoundingConfig {
    /// Maximum decimal places for order size (taker amount).
    /// SDK enforces max 2 decimals (LOT_SIZE_SCALE = 2).
    pub size_decimals: u32,
    /// Maximum decimal places for order price.
    /// Tick size typically determines this (0.01 = 2 decimals).
    pub price_decimals: u32,
}

impl Default for RoundingConfig {
    fn default() -> Self {
        Self {
            size_decimals: 2,  // SDK enforces max 2 (LOT_SIZE_SCALE = 2)
            price_decimals: 2, // Standard tick size
        }
    }
}

/// Live execution backend using rs-clob-client for real Polymarket orders.
///
/// Wraps an authenticated `polymarket_client_sdk::clob::Client` and maps between
/// domain types (`OrderRequest`, `Order`, etc.) and SDK types.
pub struct LiveBackend {
    /// Boxed inner implementation to erase the `Kind` type parameter from
    /// `Client<Authenticated<K>>` (Normal vs Builder mode).
    inner: Box<dyn LiveBackendInner>,
    /// Rounding configuration for order amounts.
    rounding: RoundingConfig,
}

/// Trait object wrapper to erase the `Kind` generic from `Client<Authenticated<K>>`.
#[async_trait]
trait LiveBackendInner: Send + Sync {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult>;
    async fn cancel_order(&self, order_id: &str) -> Result<()>;
    async fn cancel_all_orders(&self) -> Result<()>;
    async fn get_open_orders(&self) -> Result<Vec<Order>>;
    async fn get_positions(&self) -> Result<Vec<Position>>;
    async fn get_balance(&self) -> Result<Decimal>;
}

/// Concrete inner implementation parameterized by the SDK `Kind` and signer type.
struct LiveBackendImpl<K: polymarket_client_sdk::auth::Kind, S: Signer + Send + Sync> {
    client: Client<polymarket_client_sdk::auth::state::Authenticated<K>>,
    signer: S,
}

impl LiveBackend {
    /// Create a new LiveBackend from config.
    ///
    /// Authenticates with Polymarket using the private key from config.
    /// If `safe_address` is set, uses GnosisSafe signature type; otherwise EOA.
    /// Uses default rounding config (5 decimals for size, 2 for price).
    pub async fn new(config: &Config) -> Result<Self> {
        Self::new_with_rounding(config, RoundingConfig::default()).await
    }

    /// Create a new LiveBackend with custom rounding configuration.
    ///
    /// Authenticates with Polymarket using the private key from config.
    /// If `safe_address` is set, uses GnosisSafe signature type; otherwise EOA.
    pub async fn new_with_rounding(
        config: &Config,
        rounding: RoundingConfig,
    ) -> Result<Self> {
        let private_key = config.polymarket.private_key.as_deref().ok_or_else(|| {
            PolyError::Config("POLY_PRIVATE_KEY is required for live trading".into())
        })?;

        let signer = LocalSigner::from_str(private_key)
            .map_err(|e| PolyError::Config(format!("Invalid private key: {e}")))?
            .with_chain_id(Some(POLYGON));

        let sdk_config = SdkConfig::builder().use_server_time(true).build();
        let client = Client::new("https://clob.polymarket.com", sdk_config)
            .map_err(|e| PolyError::Sdk(format!("Failed to create SDK client: {e}")))?;

        let mut auth_builder = client.authentication_builder(&signer);

        if let Some(ref safe_addr) = config.polymarket.safe_address {
            let funder = SdkAddress::from_str(safe_addr)
                .map_err(|e| PolyError::Config(format!("Invalid safe address: {e}")))?;
            auth_builder = auth_builder
                .funder(funder)
                .signature_type(SignatureType::GnosisSafe);
        }

        let authenticated = auth_builder
            .authenticate()
            .await
            .map_err(|e| PolyError::Sdk(format!("Authentication failed: {e}")))?;

        info!(
            address = %authenticated.address(),
            size_decimals = rounding.size_decimals,
            price_decimals = rounding.price_decimals,
            "LiveBackend authenticated with Polymarket"
        );

        Ok(Self {
            inner: Box::new(LiveBackendImpl {
                client: authenticated,
                signer,
            }),
            rounding,
        })
    }
}

#[async_trait]
impl polyrust_core::execution::ExecutionBackend for LiveBackend {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        // Apply rounding with configured precision before passing to inner
        let mut rounded_order = order.clone();
        rounded_order.price = round_price_with_decimals(order.price, self.rounding.price_decimals);
        rounded_order.size = round_size_with_decimals(order.size, self.rounding.size_decimals);
        self.inner.place_order(&rounded_order).await
    }

    async fn cancel_order(&self, order_id: &str) -> Result<()> {
        self.inner.cancel_order(order_id).await
    }

    async fn cancel_all_orders(&self) -> Result<()> {
        self.inner.cancel_all_orders().await
    }

    async fn get_open_orders(&self) -> Result<Vec<Order>> {
        self.inner.get_open_orders().await
    }

    async fn get_positions(&self) -> Result<Vec<Position>> {
        self.inner.get_positions().await
    }

    async fn get_balance(&self) -> Result<Decimal> {
        self.inner.get_balance().await
    }

    async fn place_batch_orders(&self, orders: &[OrderRequest]) -> Result<Vec<OrderResult>> {
        // SDK does not expose a batch endpoint; fall back to sequential placement.
        // Apply rounding with configured precision for each order.
        let mut results = Vec::with_capacity(orders.len());
        for order in orders {
            let mut rounded_order = order.clone();
            rounded_order.price = round_price_with_decimals(order.price, self.rounding.price_decimals);
            rounded_order.size = round_size_with_decimals(order.size, self.rounding.size_decimals);
            results.push(self.inner.place_order(&rounded_order).await?);
        }
        Ok(results)
    }
}

#[async_trait]
impl<K: polymarket_client_sdk::auth::Kind, S: Signer + Send + Sync> LiveBackendInner
    for LiveBackendImpl<K, S>
{
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        let token_id = SdkU256::from_str(&order.token_id)
            .map_err(|e| PolyError::Execution(format!("Invalid token_id: {e}")))?;

        // Set neg_risk on the client's internal cache
        self.client.set_neg_risk(token_id, order.neg_risk);

        // Note: rounding is applied in LiveBackend::place_order before calling inner
        let price = order.price;
        let size = order.size;
        let sdk_side = map_order_side(order.side);
        let sdk_order_type = map_order_type(order.order_type);

        let signable = self
            .client
            .limit_order()
            .token_id(token_id)
            .side(sdk_side)
            .price(price)
            .size(size)
            .order_type(sdk_order_type)
            .build()
            .await
            .map_err(|e| PolyError::Sdk(format!("Failed to build order: {e}")))?;

        debug!(
            token_id = %order.token_id,
            side = ?order.side,
            price = %price,
            size = %size,
            order_type = ?order.order_type,
            "Signing order"
        );

        let signed = self
            .client
            .sign(&self.signer, signable)
            .await
            .map_err(|e| PolyError::Sdk(format!("Failed to sign order: {e}")))?;

        let response = self
            .client
            .post_order(signed)
            .await
            .map_err(|e| PolyError::Sdk(format!("Failed to post order: {e}")))?;

        let result = OrderResult {
            success: response.success,
            order_id: if response.order_id.is_empty() {
                None
            } else {
                Some(response.order_id.clone())
            },
            token_id: order.token_id.clone(),
            price,
            size,
            side: order.side,
            status: Some(format!("{:?}", response.status)),
            message: response.error_msg.unwrap_or_else(|| "ok".to_string()),
        };

        if result.success {
            info!(order_id = ?result.order_id, "Order placed successfully");
        } else {
            warn!(
                order_id = ?result.order_id,
                message = %result.message,
                "Order placement failed"
            );
        }

        Ok(result)
    }

    async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let response = self
            .client
            .cancel_order(order_id)
            .await
            .map_err(|e| PolyError::Sdk(format!("Failed to cancel order: {e}")))?;

        if !response.not_canceled.is_empty() {
            let reasons: Vec<String> = response
                .not_canceled
                .iter()
                .map(|(id, reason)| format!("{id}: {reason}"))
                .collect();
            return Err(PolyError::Execution(format!(
                "Failed to cancel order(s): {}",
                reasons.join(", ")
            )));
        }

        debug!(order_id, "Order cancelled");
        Ok(())
    }

    async fn cancel_all_orders(&self) -> Result<()> {
        let response = self
            .client
            .cancel_all_orders()
            .await
            .map_err(|e| PolyError::Sdk(format!("Failed to cancel all orders: {e}")))?;

        info!(
            cancelled = response.canceled.len(),
            failed = response.not_canceled.len(),
            "Cancel all orders complete"
        );

        if !response.not_canceled.is_empty() {
            let reasons: Vec<String> = response
                .not_canceled
                .iter()
                .map(|(id, reason)| format!("{id}: {reason}"))
                .collect();
            return Err(PolyError::Execution(format!(
                "Failed to cancel {} order(s): {}",
                response.not_canceled.len(),
                reasons.join(", ")
            )));
        }

        Ok(())
    }

    async fn get_open_orders(&self) -> Result<Vec<Order>> {
        let request = OrdersRequest::default();
        let mut all_orders = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let page = self
                .client
                .orders(&request, cursor)
                .await
                .map_err(|e| PolyError::Sdk(format!("Failed to get orders: {e}")))?;

            for sdk_order in &page.data {
                all_orders.push(map_sdk_order_to_domain(sdk_order));
            }

            if page.next_cursor == "LTE=" || page.data.is_empty() {
                break;
            }
            cursor = Some(page.next_cursor);
        }

        Ok(all_orders)
    }

    async fn get_positions(&self) -> Result<Vec<Position>> {
        // The CLOB API doesn't expose a direct "positions" endpoint.
        // Positions are tracked locally by the engine via StrategyContext.
        debug!("get_positions: positions tracked via StrategyContext, returning empty");
        Ok(vec![])
    }

    async fn get_balance(&self) -> Result<Decimal> {
        let request = BalanceAllowanceRequest::default();
        let response = self
            .client
            .balance_allowance(request)
            .await
            .map_err(|e| PolyError::Sdk(format!("Failed to get balance: {e}")))?;

        debug!(balance = %response.balance, "Retrieved USDC balance");
        Ok(response.balance)
    }
}

// --- Type mapping helpers ---

/// Map domain OrderSide to SDK Side
fn map_order_side(side: OrderSide) -> SdkSide {
    match side {
        OrderSide::Buy => SdkSide::Buy,
        OrderSide::Sell => SdkSide::Sell,
    }
}

/// Map domain OrderType to SDK OrderType
fn map_order_type(order_type: OrderType) -> SdkOrderType {
    match order_type {
        OrderType::Gtc => SdkOrderType::GTC,
        OrderType::Gtd => SdkOrderType::GTD,
        OrderType::Fok => SdkOrderType::FOK,
    }
}

/// Map SDK Side to domain OrderSide
fn map_sdk_side(side: &SdkSide) -> OrderSide {
    match side {
        SdkSide::Buy => OrderSide::Buy,
        SdkSide::Sell => OrderSide::Sell,
        other => {
            tracing::warn!(?other, "Unknown SDK side variant, defaulting to Sell");
            OrderSide::Sell
        }
    }
}

/// Map SDK OrderStatusType to domain OrderStatus
fn map_sdk_order_status(
    status: &polymarket_client_sdk::clob::types::OrderStatusType,
) -> OrderStatus {
    use polymarket_client_sdk::clob::types::OrderStatusType;
    match status {
        OrderStatusType::Live => OrderStatus::Open,
        OrderStatusType::Matched => OrderStatus::Filled,
        OrderStatusType::Canceled => OrderStatus::Cancelled,
        OrderStatusType::Delayed => OrderStatus::Open,
        OrderStatusType::Unmatched => OrderStatus::Expired,
        _ => OrderStatus::Open,
    }
}

/// Map an SDK OpenOrderResponse to domain Order
fn map_sdk_order_to_domain(
    sdk_order: &polymarket_client_sdk::clob::types::response::OpenOrderResponse,
) -> Order {
    Order {
        id: sdk_order.id.clone(),
        token_id: sdk_order.asset_id.to_string(),
        side: map_sdk_side(&sdk_order.side),
        price: sdk_order.price,
        size: sdk_order.original_size,
        filled_size: sdk_order.size_matched,
        status: map_sdk_order_status(&sdk_order.status),
        created_at: sdk_order.created_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymarket_client_sdk::clob::types::OrderStatusType;
    use rust_decimal_macros::dec;

    #[test]
    fn order_side_mapping_buy() {
        assert!(matches!(map_order_side(OrderSide::Buy), SdkSide::Buy));
    }

    #[test]
    fn order_side_mapping_sell() {
        assert!(matches!(map_order_side(OrderSide::Sell), SdkSide::Sell));
    }

    #[test]
    fn order_type_mapping_gtc() {
        assert!(matches!(map_order_type(OrderType::Gtc), SdkOrderType::GTC));
    }

    #[test]
    fn order_type_mapping_gtd() {
        assert!(matches!(map_order_type(OrderType::Gtd), SdkOrderType::GTD));
    }

    #[test]
    fn order_type_mapping_fok() {
        assert!(matches!(map_order_type(OrderType::Fok), SdkOrderType::FOK));
    }

    #[test]
    fn sdk_side_mapping() {
        assert_eq!(map_sdk_side(&SdkSide::Buy), OrderSide::Buy);
        assert_eq!(map_sdk_side(&SdkSide::Sell), OrderSide::Sell);
    }

    #[test]
    fn sdk_order_status_mapping() {
        assert_eq!(
            map_sdk_order_status(&OrderStatusType::Live),
            OrderStatus::Open
        );
        assert_eq!(
            map_sdk_order_status(&OrderStatusType::Matched),
            OrderStatus::Filled
        );
        assert_eq!(
            map_sdk_order_status(&OrderStatusType::Canceled),
            OrderStatus::Cancelled
        );
        assert_eq!(
            map_sdk_order_status(&OrderStatusType::Unmatched),
            OrderStatus::Expired
        );
    }

    #[test]
    fn sdk_order_to_domain_mapping() {
        use polymarket_client_sdk::auth::ApiKey;
        use polymarket_client_sdk::clob::types::response::OpenOrderResponse;
        use polymarket_client_sdk::types::{Address, B256, U256};

        let sdk_order = OpenOrderResponse::builder()
            .id("0xabc123")
            .status(OrderStatusType::Live)
            .owner(ApiKey::nil())
            .maker_address(Address::ZERO)
            .market(B256::ZERO)
            .asset_id(U256::from(12345))
            .side(SdkSide::Buy)
            .original_size(dec!(100))
            .size_matched(dec!(25))
            .price(dec!(0.45))
            .outcome("Yes")
            .associate_trades(vec![])
            .created_at(chrono::Utc::now())
            .expiration(chrono::Utc::now())
            .order_type(SdkOrderType::GTC)
            .build();

        let domain_order = map_sdk_order_to_domain(&sdk_order);

        assert_eq!(domain_order.id, "0xabc123");
        assert_eq!(domain_order.token_id, "12345");
        assert_eq!(domain_order.side, OrderSide::Buy);
        assert_eq!(domain_order.price, dec!(0.45));
        assert_eq!(domain_order.size, dec!(100));
        assert_eq!(domain_order.filled_size, dec!(25));
        assert_eq!(domain_order.status, OrderStatus::Open);
    }

    #[test]
    fn order_result_mapping_success() {
        let result = OrderResult {
            success: true,
            order_id: Some("0xdef456".to_string()),
            token_id: "token1".to_string(),
            price: dec!(0.50),
            size: dec!(10),
            side: OrderSide::Buy,
            status: Some("Live".to_string()),
            message: "ok".to_string(),
        };
        assert!(result.success);
        assert_eq!(result.order_id.as_deref(), Some("0xdef456"));
    }

    #[test]
    fn order_result_mapping_failure() {
        let result = OrderResult {
            success: false,
            order_id: None,
            token_id: "token1".to_string(),
            price: dec!(0.50),
            size: dec!(10),
            side: OrderSide::Buy,
            status: None,
            message: "insufficient balance".to_string(),
        };
        assert!(!result.success);
        assert!(result.order_id.is_none());
    }
}
