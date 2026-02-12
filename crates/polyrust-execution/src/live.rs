use std::str::FromStr;

use async_trait::async_trait;
use polymarket_client_sdk::POLYGON;
use polymarket_client_sdk::auth::{LocalSigner, Signer};
use polymarket_client_sdk::clob::types::request::{BalanceAllowanceRequest, OrdersRequest};
use polymarket_client_sdk::clob::types::{
    AssetType, OrderStatusType, Side as SdkSide, SignatureType,
};
use polymarket_client_sdk::clob::{Client, Config as SdkConfig};
use polymarket_client_sdk::types::{Address as SdkAddress, U256 as SdkU256};
use rust_decimal::Decimal;
use tracing::{debug, error, info, warn};

use polyrust_core::config::Config;
use polyrust_core::error::{PolyError, Result};
use polyrust_core::types::*;

use crate::ctf_redeemer::CtfRedeemer;
use crate::relayer::RelayerClient;
use crate::rounding::{build_signable_order, round_down};
use polyrust_core::execution::{RedeemRequest, RedeemResult};

/// Live execution backend using rs-clob-client for real Polymarket orders.
///
/// Wraps an authenticated `polymarket_client_sdk::clob::Client` and maps between
/// domain types (`OrderRequest`, `Order`, etc.) and SDK types.
pub struct LiveBackend {
    /// Boxed inner implementation to erase the `Kind` type parameter from
    /// `Client<Authenticated<K>>` (Normal vs Builder mode).
    inner: Box<dyn LiveBackendInner>,
}

/// Trait object wrapper to erase the `Kind` generic from `Client<Authenticated<K>>`.
#[async_trait]
trait LiveBackendInner: Send + Sync {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult>;
    async fn place_orders_batch(&self, orders: &[OrderRequest]) -> Result<Vec<OrderResult>>;
    async fn cancel_order(&self, order_id: &str) -> Result<()>;
    async fn cancel_all_orders(&self) -> Result<()>;
    async fn get_open_orders(&self) -> Result<Vec<Order>>;
    async fn get_positions(&self) -> Result<Vec<Position>>;
    async fn get_balance(&self) -> Result<Decimal>;
    async fn is_market_resolved(&self, condition_id: &str) -> Result<bool>;
    async fn redeem_positions(&self, request: &RedeemRequest) -> Result<RedeemResult>;
    async fn redeem_positions_batch(&self, requests: &[RedeemRequest])
    -> Result<Vec<RedeemResult>>;
}

/// Concrete inner implementation parameterized by the SDK `Kind` and signer type.
struct LiveBackendImpl<K: polymarket_client_sdk::auth::Kind, S: Signer + Send + Sync> {
    client: Client<polymarket_client_sdk::auth::state::Authenticated<K>>,
    signer: S,
    /// Signer's on-chain address (or Safe address as funder).
    signer_address: SdkAddress,
    /// Optional Safe address used as `maker` in orders.
    funder: Option<SdkAddress>,
    /// Signature type (EOA or GnosisSafe).
    signature_type: SignatureType,
    /// CTF redeemer for on-chain position redemption
    ctf_redeemer: Option<CtfRedeemer>,
}

impl<K: polymarket_client_sdk::auth::Kind, S: Signer + Send + Sync> LiveBackendImpl<K, S> {
    #[allow(clippy::too_many_arguments)]
    async fn sign_and_post_order(
        &self,
        token_id: SdkU256,
        price: Decimal,
        size: Decimal,
        side: OrderSide,
        order_type: OrderType,
        tick_size: Decimal,
        fee_rate_bps: u32,
        post_only: bool,
    ) -> Result<polymarket_client_sdk::clob::types::response::PostOrderResponse> {
        let signable = build_signable_order(
            token_id,
            price,
            size,
            side,
            order_type,
            tick_size,
            fee_rate_bps,
            post_only,
            self.signer_address,
            self.funder,
            self.signature_type,
        );

        debug!(
            token_id = %token_id,
            side = ?side,
            price = %price,
            size = %size,
            order_type = ?order_type,
            fee_rate_bps = fee_rate_bps,
            "Signing order (direct construction)"
        );

        let signed = self
            .client
            .sign(&self.signer, signable)
            .await
            .map_err(|e| PolyError::Sdk(format!("Failed to sign order: {e}")))?;

        self.client
            .post_order(signed)
            .await
            .map_err(|e| PolyError::Sdk(format!("Failed to post order: {e}")))
    }

    /// Refresh the CLOB's conditional token balance/allowance cache and return
    /// the reported balance. Returns `None` if the query fails.
    async fn refresh_conditional_balance_allowance(&self, token_id: SdkU256) -> Option<Decimal> {
        let request = BalanceAllowanceRequest::builder()
            .asset_type(AssetType::Conditional)
            .token_id(token_id)
            .signature_type(self.signature_type)
            .build();

        if let Err(e) = self.client.update_balance_allowance(request.clone()).await {
            warn!(
                token_id = %token_id,
                error = %e,
                "Failed to force conditional balance/allowance refresh"
            );
        }

        match self.client.balance_allowance(request).await {
            Ok(resp) => {
                let allowance_details: Vec<String> = resp
                    .allowances
                    .iter()
                    .map(|(target, amount)| format!("{target}={amount}"))
                    .collect();
                warn!(
                    token_id = %token_id,
                    conditional_balance = %resp.balance,
                    allowances = %allowance_details.join(", "),
                    "Conditional balance/allowance snapshot"
                );
                if resp.balance.is_zero() {
                    error!(
                        token_id = %token_id,
                        "CLOB reports ZERO conditional token balance — sells will fail"
                    );
                }
                Some(resp.balance)
            }
            Err(e) => {
                warn!(
                    token_id = %token_id,
                    error = %e,
                    "Failed to read conditional balance/allowance"
                );
                None
            }
        }
    }
}

impl LiveBackend {
    /// Create a new LiveBackend from config.
    ///
    /// Authenticates with Polymarket using the private key from config.
    /// If `safe_address` is set, uses GnosisSafe signature type; otherwise EOA.
    pub async fn new(config: &Config) -> Result<Self> {
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

        let (funder, sig_type) = if let Some(ref safe_addr) = config.polymarket.safe_address {
            let funder_addr = SdkAddress::from_str(safe_addr)
                .map_err(|e| PolyError::Config(format!("Invalid safe address: {e}")))?;
            auth_builder = auth_builder
                .funder(funder_addr)
                .signature_type(SignatureType::GnosisSafe);
            (Some(funder_addr), SignatureType::GnosisSafe)
        } else {
            (None, SignatureType::Eoa)
        };

        let authenticated = auth_builder
            .authenticate()
            .await
            .map_err(|e| PolyError::Sdk(format!("Authentication failed: {e}")))?;

        let signer_address = authenticated.address();

        info!(
            address = %signer_address,
            "LiveBackend authenticated with Polymarket"
        );

        // Build gasless relayer if builder credentials are available
        let relayer = if config.polymarket.use_relayer
            && config.polymarket.builder_api_key.is_some()
            && config.polymarket.builder_api_secret.is_some()
            && config.polymarket.builder_api_passphrase.is_some()
        {
            let safe_addr_str = config.polymarket.safe_address.as_deref().unwrap_or("");
            let safe_addr: alloy::primitives::Address = safe_addr_str
                .parse()
                .map_err(|e| PolyError::Config(format!("Invalid Safe address for relayer: {e}")))?;
            let relayer_signer: alloy::signers::local::PrivateKeySigner = private_key
                .parse()
                .map_err(|e| PolyError::Config(format!("Invalid private key for relayer: {e}")))?;
            match RelayerClient::new(&config.polymarket, &relayer_signer, safe_addr) {
                Ok(r) => {
                    info!("Gasless relayer enabled (builder API)");
                    Some(r)
                }
                Err(e) => {
                    warn!(error = %e, "Failed to initialize relayer, will use direct RPC only");
                    None
                }
            }
        } else {
            debug!("Relayer not configured (missing builder credentials or disabled)");
            None
        };

        // Initialize CtfRedeemer if we have RPC URLs
        let ctf_redeemer = if !config.polymarket.rpc_urls.is_empty() {
            match CtfRedeemer::new_with_relayer(
                &config.polymarket.rpc_urls,
                private_key,
                config.polymarket.safe_address.as_deref().unwrap_or(""),
                relayer,
            ) {
                Ok(redeemer) => {
                    info!(
                        rpc_count = config.polymarket.rpc_urls.len(),
                        "CtfRedeemer initialized"
                    );
                    if let Err(e) = redeemer.ensure_approvals().await {
                        warn!("Token approval check failed: {e} (sells may fail)");
                    }
                    Some(redeemer)
                }
                Err(e) => {
                    warn!(
                        "Failed to initialize CtfRedeemer: {} (redemption disabled)",
                        e
                    );
                    None
                }
            }
        } else {
            warn!("No RPC URLs configured, redemption disabled");
            None
        };

        // Pre-flight USDC balance check (non-fatal)
        match authenticated
            .balance_allowance(BalanceAllowanceRequest::default())
            .await
        {
            Ok(resp) => {
                if resp.balance.is_zero() {
                    warn!("USDC balance is zero — live trading will fail until funded");
                } else {
                    info!(balance = %resp.balance, "USDC balance available");
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to query USDC balance (non-fatal)");
            }
        }

        Ok(Self {
            inner: Box::new(LiveBackendImpl {
                client: authenticated,
                signer,
                signer_address,
                funder,
                signature_type: sig_type,
                ctf_redeemer,
            }),
        })
    }
}

#[async_trait]
impl polyrust_core::execution::ExecutionBackend for LiveBackend {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        // Rounding is handled inside build_signable_order
        self.inner.place_order(order).await
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
        self.inner.place_orders_batch(orders).await
    }

    async fn is_market_resolved(&self, condition_id: &str) -> Result<bool> {
        self.inner.is_market_resolved(condition_id).await
    }

    async fn redeem_positions(&self, request: &RedeemRequest) -> Result<RedeemResult> {
        self.inner.redeem_positions(request).await
    }

    async fn redeem_positions_batch(
        &self,
        requests: &[RedeemRequest],
    ) -> Result<Vec<RedeemResult>> {
        self.inner.redeem_positions_batch(requests).await
    }
}

#[async_trait]
impl<K: polymarket_client_sdk::auth::Kind, S: Signer + Send + Sync> LiveBackendInner
    for LiveBackendImpl<K, S>
{
    async fn place_orders_batch(&self, orders: &[OrderRequest]) -> Result<Vec<OrderResult>> {
        if orders.is_empty() {
            return Ok(vec![]);
        }

        // Build + sign all orders
        let mut signed_orders = Vec::with_capacity(orders.len());
        for order in orders {
            let token_id = SdkU256::from_str(&order.token_id)
                .map_err(|e| PolyError::Execution(format!("Invalid token_id: {e}")))?;
            self.client.set_neg_risk(token_id, order.neg_risk);

            let fee_rate_bps = self
                .client
                .fee_rate_bps(token_id)
                .await
                .map(|r| r.base_fee)
                .unwrap_or(order.fee_rate_bps);

            let signable = build_signable_order(
                token_id,
                order.price,
                order.size,
                order.side,
                order.order_type,
                order.tick_size,
                fee_rate_bps,
                order.post_only,
                self.signer_address,
                self.funder,
                self.signature_type,
            );

            let signed = self
                .client
                .sign(&self.signer, signable)
                .await
                .map_err(|e| PolyError::Sdk(format!("Failed to sign order: {e}")))?;
            signed_orders.push((signed, order));
        }

        // Post all orders in a single batch request
        let sdk_signed: Vec<_> = signed_orders.into_iter().map(|(s, _)| s).collect();
        match self.client.post_orders(sdk_signed).await {
            Ok(responses) => {
                let results: Vec<OrderResult> = responses
                    .into_iter()
                    .zip(orders.iter())
                    .map(|(resp, order)| {
                        let result = OrderResult {
                            success: resp.success,
                            order_id: if resp.order_id.is_empty() {
                                None
                            } else {
                                Some(resp.order_id.clone())
                            },
                            token_id: order.token_id.clone(),
                            price: order.price,
                            size: order.size,
                            side: order.side,
                            status: Some(match resp.status {
                                polymarket_client_sdk::clob::types::OrderStatusType::Matched => {
                                    "Filled".to_string()
                                }
                                other => format!("{:?}", other),
                            }),
                            message: resp.error_msg.unwrap_or_else(|| "ok".to_string()),
                        };
                        if result.success {
                            info!(order_id = ?result.order_id, "Batch order placed successfully");
                        } else {
                            warn!(
                                order_id = ?result.order_id,
                                message = %result.message,
                                "Batch order placement failed"
                            );
                        }
                        result
                    })
                    .collect();
                Ok(results)
            }
            Err(e) => {
                warn!(
                    error = %e,
                    count = orders.len(),
                    "Batch post_orders failed, falling back to sequential"
                );
                // Fallback: sequential placement
                let mut results = Vec::with_capacity(orders.len());
                for order in orders {
                    results.push(self.place_order(order).await?);
                }
                Ok(results)
            }
        }
    }

    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        let token_id = SdkU256::from_str(&order.token_id)
            .map_err(|e| PolyError::Execution(format!("Invalid token_id: {e}")))?;

        // Set neg_risk on the client's internal cache (needed by sign())
        self.client.set_neg_risk(token_id, order.neg_risk);

        // Fetch actual fee_rate_bps from CLOB API (cached by SDK)
        let fee_rate_bps = self
            .client
            .fee_rate_bps(token_id)
            .await
            .map(|r| r.base_fee)
            .unwrap_or(order.fee_rate_bps);

        let price = order.price;
        let mut size = order.size;

        if order.side == OrderSide::Sell {
            // 1. Refresh CLOB balance cache and get reported balance
            let clob_balance = self.refresh_conditional_balance_allowance(token_id).await;

            // 2. On-chain balance check — bypasses CLOB cache entirely
            let onchain_shares = if let Some(redeemer) = &self.ctf_redeemer {
                match redeemer.balance_of(&order.token_id).await {
                    Ok(shares) => Some(shares),
                    Err(e) => {
                        warn!(
                            token_id = %order.token_id,
                            error = %e,
                            "On-chain balance check failed (proceeding with CLOB balance only)"
                        );
                        None
                    }
                }
            } else {
                None
            };

            warn!(
                token_id = %order.token_id,
                clob_balance = ?clob_balance,
                onchain_shares = ?onchain_shares,
                sell_size = %size,
                "Pre-sell balance diagnostic"
            );

            // 3. If on-chain says 0, settlement hasn't happened — skip sell
            if let Some(onchain) = onchain_shares
                && onchain.is_zero()
            {
                return Err(PolyError::Execution(format!(
                    "No on-chain CTF balance for token {} — settlement pending",
                    order.token_id
                )));
            }

            // 4. Clamp sell size to actual balance (handles fee deduction from tokens)
            let actual = onchain_shares.or(clob_balance).unwrap_or(Decimal::ZERO);
            if actual > Decimal::ZERO && actual < size {
                warn!(
                    token_id = %order.token_id,
                    original_size = %size,
                    clamped_size = %actual,
                    "Clamping sell size to actual token balance (fee deduction)"
                );
                size = round_down(actual, 2);
            }

            // 5. If CLOB says 0 but on-chain has tokens, wait for cache sync
            if clob_balance == Some(Decimal::ZERO)
                && onchain_shares.is_some_and(|s| s > Decimal::ZERO)
            {
                warn!(
                    token_id = %order.token_id,
                    "CLOB cache stale: on-chain has tokens but CLOB says 0, waiting 2s"
                );
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                self.refresh_conditional_balance_allowance(token_id).await;
            }
        }

        let response = match self
            .sign_and_post_order(
                token_id,
                price,
                size,
                order.side,
                order.order_type,
                order.tick_size,
                fee_rate_bps,
                order.post_only,
            )
            .await
        {
            Ok(r) => r,
            Err(e)
                if order.side == OrderSide::Sell && is_balance_allowance_error(&e.to_string()) =>
            {
                warn!(
                    token_id = %order.token_id,
                    side = ?order.side,
                    order_type = ?order.order_type,
                    error = %e,
                    "Sell rejected with balance/allowance; re-querying and retrying once"
                );

                // Wait for CLOB to settle
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;

                // Re-query on-chain balance and re-clamp size
                let mut retry_size = size;
                if let Some(redeemer) = &self.ctf_redeemer {
                    match redeemer.balance_of(&order.token_id).await {
                        Ok(onchain) if onchain > Decimal::ZERO && onchain < retry_size => {
                            let clamped = round_down(onchain, 2);
                            warn!(
                                token_id = %order.token_id,
                                original_size = %retry_size,
                                onchain_balance = %onchain,
                                clamped_size = %clamped,
                                "Retry: re-clamping sell size to on-chain balance"
                            );
                            retry_size = clamped;
                        }
                        Ok(onchain) if onchain.is_zero() => {
                            return Err(PolyError::Execution(format!(
                                "Retry: no on-chain CTF balance for token {} — settlement pending",
                                order.token_id
                            )));
                        }
                        Err(e) => {
                            warn!(
                                token_id = %order.token_id,
                                error = %e,
                                "Retry: on-chain balance check failed, proceeding with original size"
                            );
                        }
                        _ => {}
                    }
                }

                self.refresh_conditional_balance_allowance(token_id).await;
                self.sign_and_post_order(
                    token_id,
                    price,
                    retry_size,
                    order.side,
                    order.order_type,
                    order.tick_size,
                    fee_rate_bps,
                    order.post_only,
                )
                .await?
            }
            Err(e) => return Err(e),
        };

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
            status: Some(match response.status {
                OrderStatusType::Matched => "Filled".to_string(),
                other => format!("{:?}", other),
            }),
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

    async fn is_market_resolved(&self, condition_id: &str) -> Result<bool> {
        match &self.ctf_redeemer {
            Some(redeemer) => redeemer.is_resolved(condition_id).await,
            None => Err(PolyError::Execution(
                "CtfRedeemer not initialized (no RPC URL configured)".into(),
            )),
        }
    }

    async fn redeem_positions(&self, request: &RedeemRequest) -> Result<RedeemResult> {
        match &self.ctf_redeemer {
            Some(redeemer) => {
                match redeemer
                    .redeem(&request.condition_id, request.neg_risk, &request.token_ids)
                    .await?
                {
                    Some(tx_hash) => Ok(RedeemResult {
                        market_id: request.market_id.clone(),
                        tx_hash: format!("{:#x}", tx_hash),
                        success: true,
                        message: "Position redeemed successfully".to_string(),
                    }),
                    None => Ok(RedeemResult {
                        market_id: request.market_id.clone(),
                        tx_hash: String::new(),
                        success: true,
                        message: "No CTF balance".to_string(),
                    }),
                }
            }
            None => Err(PolyError::Execution(
                "CtfRedeemer not initialized (no RPC URL configured)".into(),
            )),
        }
    }

    async fn redeem_positions_batch(
        &self,
        requests: &[RedeemRequest],
    ) -> Result<Vec<RedeemResult>> {
        let redeemer = self.ctf_redeemer.as_ref().ok_or_else(|| {
            PolyError::Execution("CtfRedeemer not initialized (no RPC URL configured)".into())
        })?;

        let claims: Vec<(String, bool, Vec<String>)> = requests
            .iter()
            .map(|r| (r.condition_id.clone(), r.neg_risk, r.token_ids.clone()))
            .collect();

        let batch_results = redeemer.redeem_batch(&claims).await?;

        // Map batch results back to RedeemResult, matching by condition_id
        let mut results = Vec::with_capacity(requests.len());
        for request in requests {
            let tx_hash = batch_results
                .iter()
                .find(|(cid, _)| *cid == request.condition_id)
                .and_then(|(_, hash)| hash.as_ref());

            results.push(match tx_hash {
                Some(hash) => RedeemResult {
                    market_id: request.market_id.clone(),
                    tx_hash: format!("{:#x}", hash),
                    success: true,
                    message: "Position redeemed successfully".to_string(),
                },
                None => RedeemResult {
                    market_id: request.market_id.clone(),
                    tx_hash: String::new(),
                    success: true,
                    message: "No CTF balance".to_string(),
                },
            });
        }
        Ok(results)
    }
}

// --- Type mapping helpers ---

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

fn is_balance_allowance_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("not enough balance") || lower.contains("allowance")
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymarket_client_sdk::clob::types::{OrderStatusType, OrderType as SdkOrderType};
    use rust_decimal_macros::dec;

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
