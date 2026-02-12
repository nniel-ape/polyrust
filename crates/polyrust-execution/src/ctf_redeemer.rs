#![allow(clippy::too_many_arguments)]

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Bytes, FixedBytes, U256, address};
use alloy::providers::ProviderBuilder;
use alloy::signers::Signer;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use polyrust_core::error::{PolyError, Result};
use rust_decimal::Decimal;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::relayer::RelayerClient;

// Polymarket contract addresses on Polygon mainnet
const CTF_ADDRESS: Address = address!("4D97DCd97eC945f40cF65F87097ACe5EA0476045");
const NEG_RISK_ADAPTER: Address = address!("d91E80cF2E7be2e162c6513ceD06f1dD0dA35296");
const USDC_ADDRESS: Address = address!("2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
const CTF_EXCHANGE: Address = address!("4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E");
const NEG_RISK_EXCHANGE: Address = address!("C5d563A36AE78145C45a50134d48A1215220f80a");
/// Gnosis Safe MultiSendCallOnly — reverts if any inner tx uses DelegateCall.
const MULTI_SEND_CALL_ONLY: Address = address!("40A2aCCbd92BCA938b02010E17A5b8929b49130D");

const MAX_RETRIES: u32 = 3;

sol! {
    #[sol(rpc)]
    interface IConditionalTokens {
        function payoutDenominator(bytes32 conditionId) external view returns (uint256);
        function redeemPositions(
            address collateralToken,
            bytes32 parentCollectionId,
            bytes32 conditionId,
            uint256[] calldata indexSets
        ) external;
        function balanceOf(address owner, uint256 id) external view returns (uint256);
        function setApprovalForAll(address operator, bool approved) external;
        function isApprovedForAll(address account, address operator) external view returns (bool);
    }

    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 value) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
    }

    #[sol(rpc)]
    interface INegRiskAdapter {
        function redeemPositions(
            bytes32 conditionId,
            uint256[] calldata amounts
        ) external;
    }

    #[sol(rpc)]
    interface IMultiSend {
        function multiSend(bytes memory transactions) external payable;
    }

    #[sol(rpc)]
    interface ISafe {
        function nonce() external view returns (uint256);
        function getTransactionHash(
            address to,
            uint256 value,
            bytes calldata data,
            uint8 operation,
            uint256 safeTxGas,
            uint256 baseGas,
            uint256 gasPrice,
            address gasToken,
            address refundReceiver,
            uint256 _nonce
        ) external view returns (bytes32);
        function execTransaction(
            address to,
            uint256 value,
            bytes calldata data,
            uint8 operation,
            uint256 safeTxGas,
            uint256 baseGas,
            uint256 gasPrice,
            address gasToken,
            address refundReceiver,
            bytes memory signatures
        ) external payable returns (bool success);
    }
}

/// Result of a single read-only approval check.
#[derive(Debug)]
pub struct ApprovalStatus {
    pub name: &'static str,
    pub approved: bool,
}

/// Perform read-only on-chain approval checks for Polymarket contracts.
///
/// Checks 3 CTF operator approvals (ERC-1155 `isApprovedForAll`) and
/// 4 USDC spender allowances (ERC-20 `allowance`). No on-chain writes.
pub async fn check_approvals_readonly(
    rpc_url: &str,
    owner: Address,
) -> Result<Vec<ApprovalStatus>> {
    let url: reqwest::Url = rpc_url
        .parse()
        .map_err(|e| PolyError::Config(format!("Invalid RPC URL '{rpc_url}': {e}")))?;
    let provider = ProviderBuilder::new().connect_http(url);

    let ctf_operators: &[(&'static str, Address)] = &[
        ("CTF Exchange (ERC-1155)", CTF_EXCHANGE),
        ("Neg Risk Exchange (ERC-1155)", NEG_RISK_EXCHANGE),
        ("Neg Risk Adapter (ERC-1155)", NEG_RISK_ADAPTER),
    ];
    let usdc_spenders: &[(&'static str, Address)] = &[
        ("CTF Exchange (USDC)", CTF_EXCHANGE),
        ("Neg Risk Exchange (USDC)", NEG_RISK_EXCHANGE),
        ("Neg Risk Adapter (USDC)", NEG_RISK_ADAPTER),
        ("Conditional Tokens (USDC)", CTF_ADDRESS),
    ];

    let mut results = Vec::with_capacity(7);

    let ctf = IConditionalTokens::new(CTF_ADDRESS, &provider);
    for (name, operator) in ctf_operators {
        let approved = ctf
            .isApprovedForAll(owner, *operator)
            .call()
            .await
            .map_err(|e| {
                PolyError::Execution(format!("isApprovedForAll check for {name} failed: {e}"))
            })?;
        results.push(ApprovalStatus { name, approved });
    }

    let usdc = IERC20::new(USDC_ADDRESS, &provider);
    for (name, spender) in usdc_spenders {
        let allowance = usdc.allowance(owner, *spender).call().await.map_err(|e| {
            PolyError::Execution(format!("USDC allowance check for {name} failed: {e}"))
        })?;
        results.push(ApprovalStatus {
            name,
            approved: !allowance.is_zero(),
        });
    }

    Ok(results)
}

/// On-chain CTF position redeemer for Polymarket markets.
///
/// Executes redemptions through the Safe wallet via `execTransaction`.
/// Standard markets use CTF `redeemPositions` directly.
/// Neg-risk markets query CTF balances then call the NegRiskAdapter.
///
/// Supports multiple RPC endpoints with automatic rotation on rate limits
/// and exponential backoff on transient errors.
pub struct CtfRedeemer {
    rpc_urls: Vec<String>,
    current_rpc: AtomicUsize,
    signer: PrivateKeySigner,
    safe_address: Address,
    relayer: Option<RelayerClient>,
}

impl CtfRedeemer {
    /// Create a new CtfRedeemer with the given RPC endpoints and credentials.
    ///
    /// Multiple RPC URLs enable automatic failover on rate limits.
    pub fn new(rpc_urls: &[String], private_key: &str, safe_address: &str) -> Result<Self> {
        if rpc_urls.is_empty() {
            return Err(PolyError::Config("No RPC URLs provided".into()));
        }

        let safe_address = safe_address
            .parse::<Address>()
            .map_err(|e| PolyError::Config(format!("Invalid Safe address: {e}")))?;

        let signer: PrivateKeySigner = private_key
            .parse()
            .map_err(|e| PolyError::Config(format!("Invalid private key: {e}")))?;

        info!(
            eoa = %signer.address(),
            safe = %safe_address,
            rpc_count = rpc_urls.len(),
            "CtfRedeemer initialized"
        );

        Ok(Self {
            rpc_urls: rpc_urls.to_vec(),
            current_rpc: AtomicUsize::new(0),
            signer,
            safe_address,
            relayer: None,
        })
    }

    /// Create a new CtfRedeemer with an optional relayer for gasless transactions.
    ///
    /// When a relayer is provided, `execute_safe_tx` tries the relayer first
    /// and falls back to direct RPC on failure.
    pub fn new_with_relayer(
        rpc_urls: &[String],
        private_key: &str,
        safe_address: &str,
        relayer: Option<RelayerClient>,
    ) -> Result<Self> {
        let mut redeemer = Self::new(rpc_urls, private_key, safe_address)?;
        if relayer.is_some() {
            info!("CtfRedeemer using gasless relayer (direct RPC as fallback)");
        }
        redeemer.relayer = relayer;
        Ok(redeemer)
    }

    /// Get the current RPC URL.
    fn rpc_url(&self) -> &str {
        let idx = self.current_rpc.load(Ordering::Relaxed) % self.rpc_urls.len();
        &self.rpc_urls[idx]
    }

    /// Rotate to the next RPC URL on rate limit.
    fn rotate_rpc(&self) {
        if self.rpc_urls.len() > 1 {
            let old = self.current_rpc.fetch_add(1, Ordering::Relaxed);
            let new_idx = (old + 1) % self.rpc_urls.len();
            debug!(new_idx, "Rotated to next RPC endpoint");
        }
    }

    /// Check and set ERC-1155 (CTF) and ERC-20 (USDC) approvals required by
    /// Polymarket contracts. Without these, SELL orders fail with
    /// "not enough balance / allowance".
    ///
    /// ERC-1155 operator approvals (`setApprovalForAll`) are checked for exchange
    /// operators only. USDC approvals (`approve(MAX)`) are needed for all USDC
    /// spenders, including Conditional Tokens.
    pub async fn ensure_approvals(&self) -> Result<()> {
        let ctf_operators: &[(&str, Address)] = &[
            ("CTF Exchange", CTF_EXCHANGE),
            ("Neg Risk Exchange", NEG_RISK_EXCHANGE),
            ("Neg Risk Adapter", NEG_RISK_ADAPTER),
        ];
        let usdc_spenders: &[(&str, Address)] = &[
            ("CTF Exchange", CTF_EXCHANGE),
            ("Neg Risk Exchange", NEG_RISK_EXCHANGE),
            ("Neg Risk Adapter", NEG_RISK_ADAPTER),
            ("Conditional Tokens", CTF_ADDRESS),
        ];
        let safe = self.safe_address;

        // Check CTF approval (ERC-1155 setApprovalForAll)
        for (name, operator) in ctf_operators {
            let operator = *operator;
            let approved = with_retry("isApprovedForAll", self, || async {
                let provider = ProviderBuilder::new().connect_http(parse_rpc_url(self.rpc_url())?);
                let ctf = IConditionalTokens::new(CTF_ADDRESS, &provider);
                ctf.isApprovedForAll(safe, operator)
                    .call()
                    .await
                    .map_err(|e| {
                        PolyError::Execution(format!("isApprovedForAll check failed: {e}"))
                    })
            })
            .await;

            match approved {
                Ok(is_approved) => {
                    if !is_approved {
                        info!(contract = name, "CTF not approved, setting approval...");
                        let calldata = IConditionalTokens::setApprovalForAllCall {
                            operator,
                            approved: true,
                        }
                        .abi_encode();
                        match self
                            .execute_safe_tx(CTF_ADDRESS, Bytes::from(calldata), 0)
                            .await
                        {
                            Ok(hash) => info!(contract = name, tx = %hash, "CTF approval set"),
                            Err(e) => warn!(contract = name, error = %e, "CTF approval tx failed"),
                        }
                    } else {
                        debug!(contract = name, "CTF already approved");
                    }
                }
                Err(e) => warn!(contract = name, error = %e, "CTF isApprovedForAll check failed"),
            }
        }

        // Check USDC approval (ERC-20 approve)
        for (name, spender) in usdc_spenders {
            let spender = *spender;
            let allowance = with_retry("allowance", self, || async {
                let provider = ProviderBuilder::new().connect_http(parse_rpc_url(self.rpc_url())?);
                let usdc = IERC20::new(USDC_ADDRESS, &provider);
                usdc.allowance(safe, spender)
                    .call()
                    .await
                    .map_err(|e| PolyError::Execution(format!("USDC allowance check failed: {e}")))
            })
            .await;

            match allowance {
                Ok(value) => {
                    if value.is_zero() {
                        info!(contract = name, "USDC not approved, setting approval...");
                        let calldata = IERC20::approveCall {
                            spender,
                            value: U256::MAX,
                        }
                        .abi_encode();
                        match self
                            .execute_safe_tx(USDC_ADDRESS, Bytes::from(calldata), 0)
                            .await
                        {
                            Ok(hash) => info!(contract = name, tx = %hash, "USDC approval set"),
                            Err(e) => {
                                warn!(contract = name, error = %e, "USDC approval tx failed")
                            }
                        }
                    } else {
                        debug!(contract = name, "USDC already approved");
                    }
                }
                Err(e) => warn!(contract = name, error = %e, "USDC allowance check failed"),
            }
        }

        info!("Token approval check complete");
        Ok(())
    }

    /// Check if a market has resolved on-chain (payoutDenominator > 0).
    pub async fn is_resolved(&self, condition_id: &str) -> Result<bool> {
        let cid = parse_condition_id(condition_id)?;

        let result = with_retry("payoutDenominator", self, || async {
            let provider = ProviderBuilder::new().connect_http(parse_rpc_url(self.rpc_url())?);
            let ctf = IConditionalTokens::new(CTF_ADDRESS, &provider);
            ctf.payoutDenominator(cid)
                .call()
                .await
                .map_err(|e| PolyError::Execution(format!("payoutDenominator call failed: {e}")))
        })
        .await?;

        let resolved = result > U256::ZERO;
        debug!(condition_id, resolved, "Checked market resolution");
        Ok(resolved)
    }

    /// Get the CTF token balance for a single token ID, in shares.
    ///
    /// Queries on-chain `IConditionalTokens::balanceOf(safe, tokenId)` and
    /// converts from raw 6-decimal integer to share-unit `Decimal`.
    /// This bypasses the CLOB balance cache entirely.
    pub async fn balance_of(&self, token_id: &str) -> Result<Decimal> {
        let token_u256 = token_id
            .parse::<U256>()
            .map_err(|e| PolyError::Execution(format!("Invalid token_id '{token_id}': {e}")))?;
        let safe = self.safe_address;
        let raw = with_retry("balanceOf", self, || async {
            let provider = ProviderBuilder::new().connect_http(parse_rpc_url(self.rpc_url())?);
            let ctf = IConditionalTokens::new(CTF_ADDRESS, &provider);
            ctf.balanceOf(safe, token_u256)
                .call()
                .await
                .map_err(|e| PolyError::Execution(format!("balanceOf({token_id}) failed: {e}")))
        })
        .await?;

        // CTF tokens use 6 decimals (matching USDC collateral precision)
        let raw_str = raw.to_string();
        let raw_decimal = Decimal::from_str_exact(&raw_str).unwrap_or(Decimal::MAX);
        Ok(raw_decimal / Decimal::new(1_000_000, 0))
    }

    /// Check if the Safe holds any CTF balance for the given token IDs.
    ///
    /// Returns `true` if any token has a non-zero balance.
    pub async fn has_ctf_balance(&self, token_ids: &[String]) -> Result<bool> {
        if token_ids.is_empty() {
            return Ok(false);
        }

        for tid in token_ids {
            let token_u256 = tid
                .parse::<U256>()
                .map_err(|e| PolyError::Execution(format!("Invalid token_id '{tid}': {e}")))?;
            let safe = self.safe_address;
            let bal = with_retry("balanceOf", self, || async {
                let provider = ProviderBuilder::new().connect_http(parse_rpc_url(self.rpc_url())?);
                let ctf = IConditionalTokens::new(CTF_ADDRESS, &provider);
                ctf.balanceOf(safe, token_u256)
                    .call()
                    .await
                    .map_err(|e| PolyError::Execution(format!("balanceOf({tid}) failed: {e}")))
            })
            .await?;
            if bal > U256::ZERO {
                return Ok(true);
            }
        }

        debug!("No CTF balance found for any outcome token");
        Ok(false)
    }

    /// Redeem winning positions for a resolved market via Safe `execTransaction`.
    ///
    /// Returns `None` if the Safe holds no CTF balance for the given tokens
    /// (nothing to redeem). Otherwise returns `Some(tx_hash)`.
    ///
    /// For standard markets: encodes CTF `redeemPositions(USDC, 0x0, conditionId, [1, 2])`.
    /// For neg_risk markets: queries CTF `balanceOf` per token, then encodes
    /// NegRiskAdapter `redeemPositions(conditionId, amounts)`.
    pub async fn redeem(
        &self,
        condition_id: &str,
        neg_risk: bool,
        token_ids: &[String],
    ) -> Result<Option<FixedBytes<32>>> {
        // Check on-chain balance before attempting redemption
        if !self.has_ctf_balance(token_ids).await? {
            info!(condition_id, "No CTF balance found, skipping redemption");
            return Ok(None);
        }

        let cid = parse_condition_id(condition_id)?;

        let (target, calldata) = if neg_risk {
            self.encode_neg_risk_redeem(cid, token_ids).await?
        } else {
            encode_standard_redeem(cid)
        };

        info!(
            condition_id,
            neg_risk,
            target = %target,
            "Executing CTF redemption via Safe"
        );

        self.execute_safe_tx(target, calldata, 0).await.map(Some)
    }

    /// Execute a transaction through the Safe wallet.
    ///
    /// `operation`: 0 = Call, 1 = DelegateCall.
    ///
    /// When a relayer is configured, tries the gasless relayer first.
    /// Falls back to direct RPC `execTransaction` on relayer failure.
    async fn execute_safe_tx(
        &self,
        to: Address,
        data: Bytes,
        operation: u8,
    ) -> Result<FixedBytes<32>> {
        if let Some(relayer) = &self.relayer {
            match relayer.submit_and_wait(to, data.clone(), operation).await {
                Ok(tx_hash) => {
                    info!(tx = %tx_hash, "Relayer transaction confirmed (gasless)");
                    return Ok(tx_hash);
                }
                Err(e) => {
                    warn!(error = %e, "Relayer failed, falling back to direct RPC");
                    // If the error suggests a tx was already submitted to the mempool
                    // (contains transactionHash in the raw response), wait briefly for
                    // it to settle before falling back — avoids nonce collision (GS026).
                    if e.to_string().contains("transactionHash") {
                        debug!(
                            "Relayer may have submitted tx to mempool, waiting before direct RPC fallback"
                        );
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        }

        self.execute_safe_tx_direct(to, data, operation).await
    }

    /// Execute a transaction directly through Safe's on-chain `execTransaction`.
    ///
    /// Signs the Safe transaction hash with EIP-191 (eth_sign) and adjusts v += 4
    /// per Safe's signature encoding convention.
    ///
    /// Individual RPC calls (nonce, getTransactionHash) are retried on rate limits.
    /// The final execTransaction send is also retried.
    async fn execute_safe_tx_direct(
        &self,
        to: Address,
        data: Bytes,
        operation: u8,
    ) -> Result<FixedBytes<32>> {
        let safe_addr = self.safe_address;

        // 1. Get Safe nonce (with retry)
        let nonce = with_retry("nonce", self, || async {
            let wallet = EthereumWallet::from(self.signer.clone());
            let provider = ProviderBuilder::new()
                .wallet(wallet)
                .connect_http(parse_rpc_url(self.rpc_url())?);
            let safe = ISafe::new(safe_addr, &provider);
            safe.nonce()
                .call()
                .await
                .map_err(|e| PolyError::Execution(format!("Safe nonce() failed: {e}")))
        })
        .await?;

        // 2. Compute Safe transaction hash (with retry)
        let data_clone = data.clone();
        let tx_hash = with_retry("getTransactionHash", self, || async {
            let wallet = EthereumWallet::from(self.signer.clone());
            let provider = ProviderBuilder::new()
                .wallet(wallet)
                .connect_http(parse_rpc_url(self.rpc_url())?);
            let safe = ISafe::new(safe_addr, &provider);
            safe.getTransactionHash(
                to,
                U256::ZERO,
                data_clone.clone(),
                operation,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                Address::ZERO,
                Address::ZERO,
                nonce,
            )
            .call()
            .await
            .map_err(|e| PolyError::Execution(format!("getTransactionHash failed: {e}")))
        })
        .await?;

        // 3. Sign with EIP-191 prefix (eth_sign), v += 4 for Safe convention
        let sig = self
            .signer
            .sign_message(tx_hash.as_slice())
            .await
            .map_err(|e| PolyError::Execution(format!("Failed to sign Safe tx: {e}")))?;

        let mut sig_bytes = Vec::with_capacity(65);
        sig_bytes.extend_from_slice(&sig.r().to_be_bytes::<32>());
        sig_bytes.extend_from_slice(&sig.s().to_be_bytes::<32>());
        sig_bytes.push(sig.v() as u8 + 31); // eth_sign: v in {31, 32}

        // 4. Pre-flight simulation (eth_call) — catch reverts before spending gas
        let sig_bytes_for_retry = Bytes::from(sig_bytes);
        {
            let wallet = EthereumWallet::from(self.signer.clone());
            let provider = ProviderBuilder::new()
                .wallet(wallet)
                .connect_http(parse_rpc_url(self.rpc_url())?);
            let safe = ISafe::new(safe_addr, &provider);
            match safe
                .execTransaction(
                    to,
                    U256::ZERO,
                    data.clone(),
                    operation,
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    Address::ZERO,
                    Address::ZERO,
                    sig_bytes_for_retry.clone(),
                )
                .call()
                .await
            {
                Ok(success) => {
                    if !success {
                        return Err(PolyError::Execution(
                            "execTransaction simulation returned false — inner call would revert"
                                .into(),
                        ));
                    }
                    debug!("execTransaction simulation passed");
                }
                Err(e) => {
                    let reason = extract_revert_reason(&e);
                    return Err(PolyError::Execution(format!(
                        "execTransaction simulation reverted: {reason}"
                    )));
                }
            }
        }

        // 5. Execute through Safe (with retry)
        let receipt = with_retry("execTransaction", self, || async {
            let wallet = EthereumWallet::from(self.signer.clone());
            let provider = ProviderBuilder::new()
                .wallet(wallet)
                .connect_http(parse_rpc_url(self.rpc_url())?);
            let safe = ISafe::new(safe_addr, &provider);
            let pending = safe
                .execTransaction(
                    to,
                    U256::ZERO,
                    data.clone(),
                    operation,
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    Address::ZERO,
                    Address::ZERO,
                    sig_bytes_for_retry.clone(),
                )
                .send()
                .await
                .map_err(|e| PolyError::Execution(format!("execTransaction send failed: {e}")))?;
            pending
                .get_receipt()
                .await
                .map_err(|e| PolyError::Execution(format!("Failed to get tx receipt: {e}")))
        })
        .await?;

        if !receipt.status() {
            return Err(PolyError::Execution(format!(
                "execTransaction reverted (tx: {:#x}, gas_used: {})",
                receipt.transaction_hash, receipt.gas_used
            )));
        }

        info!(tx = %receipt.transaction_hash, "Safe execTransaction succeeded");
        Ok(receipt.transaction_hash)
    }

    /// Encode NegRiskAdapter redemption calldata, querying CTF balances first.
    async fn encode_neg_risk_redeem(
        &self,
        condition_id: FixedBytes<32>,
        token_ids: &[String],
    ) -> Result<(Address, Bytes)> {
        let safe = self.safe_address;

        let mut amounts = Vec::with_capacity(token_ids.len());
        for tid in token_ids {
            let token_u256 = tid
                .parse::<U256>()
                .map_err(|e| PolyError::Execution(format!("Invalid token_id '{tid}': {e}")))?;
            let bal = with_retry("balanceOf", self, || async {
                let provider = ProviderBuilder::new().connect_http(parse_rpc_url(self.rpc_url())?);
                let ctf = IConditionalTokens::new(CTF_ADDRESS, &provider);
                ctf.balanceOf(safe, token_u256)
                    .call()
                    .await
                    .map_err(|e| PolyError::Execution(format!("balanceOf({tid}) failed: {e}")))
            })
            .await?;
            amounts.push(bal);
        }

        debug!(?amounts, "CTF balances for neg_risk redemption");

        let data = INegRiskAdapter::redeemPositionsCall {
            conditionId: condition_id,
            amounts,
        }
        .abi_encode();

        Ok((NEG_RISK_ADAPTER, Bytes::from(data)))
    }

    /// Batch-redeem multiple resolved markets in a single Safe multiSend tx.
    ///
    /// - Single claim: delegates to `redeem()` (no multiSend overhead).
    /// - Multiple claims: pre-filters by CTF balance, packs into multiSend,
    ///   executes as DelegateCall. Falls back to individual redemptions on batch failure.
    ///
    /// Returns `(condition_id, Option<tx_hash>)` per claim. `None` means no balance.
    pub async fn redeem_batch(
        &self,
        claims: &[(String, bool, Vec<String>)],
    ) -> Result<Vec<(String, Option<FixedBytes<32>>)>> {
        if claims.is_empty() {
            return Ok(vec![]);
        }

        // Single claim — skip multiSend overhead
        if claims.len() == 1 {
            let (cid, neg_risk, token_ids) = &claims[0];
            let result = self.redeem(cid, *neg_risk, token_ids).await?;
            return Ok(vec![(cid.clone(), result)]);
        }

        // Pre-filter: check balances, encode calldata per claim
        let mut inner_calls: Vec<(String, Address, Bytes)> = Vec::new();
        let mut no_balance: Vec<String> = Vec::new();

        for (cid, neg_risk, token_ids) in claims {
            if !self.has_ctf_balance(token_ids).await? {
                no_balance.push(cid.clone());
                continue;
            }

            let parsed_cid = parse_condition_id(cid)?;
            let (target, calldata) = if *neg_risk {
                self.encode_neg_risk_redeem(parsed_cid, token_ids).await?
            } else {
                encode_standard_redeem(parsed_cid)
            };

            inner_calls.push((cid.clone(), target, calldata));
        }

        // Nothing to redeem after balance check
        if inner_calls.is_empty() {
            return Ok(no_balance.iter().map(|cid| (cid.clone(), None)).collect());
        }

        // Only one claim has balance — use direct call
        if inner_calls.len() == 1 {
            let (cid, target, calldata) = &inner_calls[0];
            let tx_hash = self.execute_safe_tx(*target, calldata.clone(), 0).await?;
            let mut results: Vec<(String, Option<FixedBytes<32>>)> =
                no_balance.iter().map(|c| (c.clone(), None)).collect();
            results.push((cid.clone(), Some(tx_hash)));
            return Ok(results);
        }

        // Pack into multiSend
        let calls: Vec<(Address, Bytes)> = inner_calls
            .iter()
            .map(|(_, target, data)| (*target, data.clone()))
            .collect();

        let multi_send_data = encode_multi_send_data(&calls);
        let multi_send_calldata = IMultiSend::multiSendCall {
            transactions: multi_send_data,
        }
        .abi_encode();

        info!(
            claim_count = inner_calls.len(),
            "Executing batch redemption via multiSend"
        );

        // DelegateCall (operation=1) to MultiSendCallOnly
        match self
            .execute_safe_tx(MULTI_SEND_CALL_ONLY, Bytes::from(multi_send_calldata), 1)
            .await
        {
            Ok(tx_hash) => {
                info!(tx = %tx_hash, count = inner_calls.len(), "Batch redemption succeeded");
                let mut results: Vec<(String, Option<FixedBytes<32>>)> =
                    no_balance.iter().map(|c| (c.clone(), None)).collect();
                for (cid, _, _) in &inner_calls {
                    results.push((cid.clone(), Some(tx_hash)));
                }
                Ok(results)
            }
            Err(batch_err) => {
                // Fallback: individual redemptions (try all, don't bail on first failure)
                warn!(
                    error = %batch_err,
                    count = inner_calls.len(),
                    "Batch multiSend failed, falling back to individual redemptions"
                );
                // Wait for any pending batch tx to settle before retrying individually
                // to avoid nonce collisions if the batch was submitted to the mempool.
                tokio::time::sleep(Duration::from_secs(3)).await;
                let mut results: Vec<(String, Option<FixedBytes<32>>)> =
                    no_balance.iter().map(|c| (c.clone(), None)).collect();
                let mut first_error: Option<PolyError> = None;
                for (cid, target, calldata) in &inner_calls {
                    match self.execute_safe_tx(*target, calldata.clone(), 0).await {
                        Ok(tx_hash) => {
                            info!(condition_id = cid, tx = %tx_hash, "Individual redemption succeeded");
                            results.push((cid.clone(), Some(tx_hash)));
                        }
                        Err(e) => {
                            warn!(condition_id = cid, error = %e, "Individual redemption failed, continuing");
                            if first_error.is_none() {
                                first_error = Some(e);
                            }
                        }
                    }
                }
                if let Some(e) = first_error {
                    return Err(e);
                }
                Ok(results)
            }
        }
    }
}

/// Encode inner calls into the packed bytes format expected by MultiSendCallOnly.
///
/// Per call: `operation(1 byte, 0=Call) || to(20 bytes) || value(32 bytes, 0) || dataLength(32 bytes) || data(N bytes)`
fn encode_multi_send_data(calls: &[(Address, Bytes)]) -> Bytes {
    let total_len: usize = calls
        .iter()
        .map(|(_, data)| 1 + 20 + 32 + 32 + data.len())
        .sum();
    let mut packed = Vec::with_capacity(total_len);

    for (to, data) in calls {
        packed.push(0u8); // operation = Call
        packed.extend_from_slice(to.as_slice()); // to (20 bytes)
        packed.extend_from_slice(&[0u8; 32]); // value (32 bytes, 0)
        let data_len = U256::from(data.len());
        packed.extend_from_slice(&data_len.to_be_bytes::<32>()); // dataLength (32 bytes)
        packed.extend_from_slice(data); // data (N bytes)
    }

    Bytes::from(packed)
}

/// Retry an RPC operation with exponential backoff and RPC rotation on rate limits.
///
/// Retries up to `MAX_RETRIES` times with 2s/4s/8s delays when rate-limited.
/// Rotates to the next RPC URL on each rate limit hit.
async fn with_retry<F, Fut, T>(op_name: &str, redeemer: &CtfRedeemer, f: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    for attempt in 0..MAX_RETRIES {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < MAX_RETRIES - 1 && is_rate_limited(&e) => {
                let delay = Duration::from_secs(2u64.pow(attempt + 1));
                redeemer.rotate_rpc();
                warn!(
                    op = op_name,
                    attempt,
                    delay_secs = delay.as_secs(),
                    rpc = redeemer.rpc_url(),
                    "RPC rate limited, retrying"
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Check if an error indicates RPC rate limiting.
fn is_rate_limited(e: &PolyError) -> bool {
    let s = e.to_string();
    s.contains("-32090") || s.contains("Too many requests") || s.contains("rate limit")
}

/// Encode standard (non-neg-risk) CTF redemption calldata.
fn encode_standard_redeem(condition_id: FixedBytes<32>) -> (Address, Bytes) {
    let data = IConditionalTokens::redeemPositionsCall {
        collateralToken: USDC_ADDRESS,
        parentCollectionId: FixedBytes::ZERO,
        conditionId: condition_id,
        indexSets: vec![U256::from(1), U256::from(2)],
    }
    .abi_encode();
    (CTF_ADDRESS, Bytes::from(data))
}

fn parse_condition_id(s: &str) -> Result<FixedBytes<32>> {
    s.parse()
        .map_err(|e| PolyError::Execution(format!("Invalid condition_id '{s}': {e}")))
}

fn parse_rpc_url(s: &str) -> Result<reqwest::Url> {
    s.parse()
        .map_err(|e| PolyError::Config(format!("Invalid RPC URL '{s}': {e}")))
}

/// Extract a human-readable revert reason from an alloy contract error.
///
/// Common Gnosis Safe error codes: GS025 = bad signature, GS026 = nonce mismatch.
fn extract_revert_reason(err: &alloy::contract::Error) -> String {
    let msg = err.to_string();
    // Try to find a Solidity-style reason string (Error(string))
    if let Some(start) = msg.find("revert: ") {
        return msg[start..].to_string();
    }
    // Try to find a raw revert data hex for known Safe error codes
    for code in &["GS000", "GS001", "GS013", "GS025", "GS026"] {
        if msg.contains(code) {
            return format!(
                "Safe error {code} (see https://github.com/safe-global/safe-smart-account/blob/main/docs/error_codes.md)"
            );
        }
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_multi_send_data_empty() {
        let result = encode_multi_send_data(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn encode_multi_send_data_single_call() {
        let to = address!("4D97DCd97eC945f40cF65F87097ACe5EA0476045");
        let data = Bytes::from(vec![0xAA, 0xBB, 0xCC]);
        let packed = encode_multi_send_data(&[(to, data.clone())]);

        // 1 + 20 + 32 + 32 + 3 = 88 bytes
        assert_eq!(packed.len(), 88);

        // operation byte
        assert_eq!(packed[0], 0u8);

        // to address (20 bytes)
        assert_eq!(&packed[1..21], to.as_slice());

        // value (32 bytes of zeros)
        assert_eq!(&packed[21..53], &[0u8; 32]);

        // dataLength (32 bytes, big-endian 3)
        let mut expected_len = [0u8; 32];
        expected_len[31] = 3;
        assert_eq!(&packed[53..85], &expected_len);

        // data
        assert_eq!(&packed[85..88], &[0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn encode_multi_send_data_two_calls() {
        let to1 = address!("4D97DCd97eC945f40cF65F87097ACe5EA0476045");
        let to2 = address!("d91E80cF2E7be2e162c6513ceD06f1dD0dA35296");
        let data1 = Bytes::from(vec![0x11, 0x22]);
        let data2 = Bytes::from(vec![0x33, 0x44, 0x55, 0x66]);

        let packed = encode_multi_send_data(&[(to1, data1), (to2, data2)]);

        // call1: 1+20+32+32+2 = 87, call2: 1+20+32+32+4 = 89, total = 176
        assert_eq!(packed.len(), 176);

        // Verify second call starts at offset 87
        assert_eq!(packed[87], 0u8); // operation
        assert_eq!(&packed[88..108], to2.as_slice()); // to
    }

    #[test]
    fn encode_multi_send_data_preserves_calldata() {
        // Use a realistic ABI-encoded calldata (standard redeem)
        let cid = FixedBytes::<32>::from([0xAB; 32]);
        let (target, calldata) = encode_standard_redeem(cid);

        let packed = encode_multi_send_data(&[(target, calldata.clone())]);

        // Extract calldata from packed bytes
        let header_len = 1 + 20 + 32 + 32;
        assert_eq!(&packed[header_len..], calldata.as_ref());
    }
}
