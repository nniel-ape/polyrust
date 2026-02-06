#![allow(clippy::too_many_arguments)]

use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, Bytes, FixedBytes, U256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use alloy::sol;
use alloy::sol_types::SolCall;
use polyrust_core::error::{PolyError, Result};
use tracing::{debug, info, warn};

// Polymarket contract addresses on Polygon mainnet
const CTF_ADDRESS: Address = address!("4D97DCd97eC945f40cF65F87097ACe5EA0476045");
const NEG_RISK_ADAPTER: Address = address!("d91E80cF2E7be2e162c6513ceD06f1dD0dA35296");
const USDC_ADDRESS: Address = address!("2791Bca1f2de4661ED88A30C99A7a9449Aa84174");
const CTF_EXCHANGE: Address = address!("4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E");
const NEG_RISK_EXCHANGE: Address = address!("C5d563A36AE78145C45a50134d48A1215220f80a");

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

/// On-chain CTF position redeemer for Polymarket markets.
///
/// Executes redemptions through the Safe wallet via `execTransaction`.
/// Standard markets use CTF `redeemPositions` directly.
/// Neg-risk markets query CTF balances then call the NegRiskAdapter.
pub struct CtfRedeemer {
    rpc_url: String,
    signer: PrivateKeySigner,
    safe_address: Address,
}

impl CtfRedeemer {
    /// Create a new CtfRedeemer with the given RPC endpoint and credentials.
    pub fn new(rpc_url: &str, private_key: &str, safe_address: &str) -> Result<Self> {
        let safe_address = safe_address.parse::<Address>().map_err(|e| {
            PolyError::Config(format!("Invalid Safe address: {e}"))
        })?;

        let signer: PrivateKeySigner = private_key.parse().map_err(|e| {
            PolyError::Config(format!("Invalid private key: {e}"))
        })?;

        info!(
            eoa = %signer.address(),
            safe = %safe_address,
            "CtfRedeemer initialized"
        );

        Ok(Self {
            rpc_url: rpc_url.to_string(),
            signer,
            safe_address,
        })
    }

    /// Check and set ERC-1155 (CTF) and ERC-20 (USDC) approvals for Polymarket
    /// exchange contracts. Without these, SELL orders fail with "not enough balance / allowance".
    ///
    /// Checks 3 contracts: CTF Exchange, Neg Risk Exchange, Neg Risk Adapter.
    /// Each needs both CTF `setApprovalForAll` and USDC `approve(MAX)`.
    pub async fn ensure_approvals(&self) -> Result<()> {
        let targets: &[(&str, Address)] = &[
            ("CTF Exchange", CTF_EXCHANGE),
            ("Neg Risk Exchange", NEG_RISK_EXCHANGE),
            ("Neg Risk Adapter", NEG_RISK_ADAPTER),
        ];

        let provider = ProviderBuilder::new().connect_http(parse_rpc_url(&self.rpc_url)?);
        let ctf = IConditionalTokens::new(CTF_ADDRESS, &provider);
        let usdc = IERC20::new(USDC_ADDRESS, &provider);

        for (name, target) in targets {
            // Check CTF approval (ERC-1155 setApprovalForAll)
            match ctf.isApprovedForAll(self.safe_address, *target).call().await {
                Ok(approved) => {
                    if !approved {
                        info!(contract = name, "CTF not approved, setting approval...");
                        let calldata = IConditionalTokens::setApprovalForAllCall {
                            operator: *target,
                            approved: true,
                        }
                        .abi_encode();
                        match self
                            .execute_safe_tx(CTF_ADDRESS, Bytes::from(calldata))
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

            // Check USDC approval (ERC-20 approve)
            match usdc.allowance(self.safe_address, *target).call().await {
                Ok(allowance) => {
                    if allowance.is_zero() {
                        info!(contract = name, "USDC not approved, setting approval...");
                        let calldata = IERC20::approveCall {
                            spender: *target,
                            value: U256::MAX,
                        }
                        .abi_encode();
                        match self
                            .execute_safe_tx(USDC_ADDRESS, Bytes::from(calldata))
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

        let provider = ProviderBuilder::new().connect_http(parse_rpc_url(&self.rpc_url)?);
        let ctf = IConditionalTokens::new(CTF_ADDRESS, &provider);

        let result = ctf.payoutDenominator(cid).call().await.map_err(|e| {
            PolyError::Execution(format!("payoutDenominator call failed: {e}"))
        })?;

        let resolved = result > U256::ZERO;
        debug!(condition_id, resolved, "Checked market resolution");
        Ok(resolved)
    }

    /// Check if the Safe holds any CTF balance for the given token IDs.
    ///
    /// Returns `true` if any token has a non-zero balance.
    pub async fn has_ctf_balance(&self, token_ids: &[String]) -> Result<bool> {
        if token_ids.is_empty() {
            return Ok(false);
        }

        let provider = ProviderBuilder::new().connect_http(parse_rpc_url(&self.rpc_url)?);
        let ctf = IConditionalTokens::new(CTF_ADDRESS, &provider);

        for tid in token_ids {
            let token_u256 = tid.parse::<U256>().map_err(|e| {
                PolyError::Execution(format!("Invalid token_id '{tid}': {e}"))
            })?;
            let bal = ctf
                .balanceOf(self.safe_address, token_u256)
                .call()
                .await
                .map_err(|e| {
                    PolyError::Execution(format!("balanceOf({tid}) failed: {e}"))
                })?;
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

        self.execute_safe_tx(target, calldata).await.map(Some)
    }

    /// Execute a transaction through the Safe wallet.
    ///
    /// Signs the Safe transaction hash with EIP-191 (eth_sign) and adjusts v += 4
    /// per Safe's signature encoding convention.
    async fn execute_safe_tx(
        &self,
        to: Address,
        data: Bytes,
    ) -> Result<FixedBytes<32>> {
        let wallet = EthereumWallet::from(self.signer.clone());
        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect_http(parse_rpc_url(&self.rpc_url)?);
        let safe = ISafe::new(self.safe_address, &provider);

        // 1. Get Safe nonce
        let nonce = safe.nonce().call().await.map_err(|e| {
            PolyError::Execution(format!("Safe nonce() failed: {e}"))
        })?;

        // 2. Compute Safe transaction hash
        let tx_hash = safe
            .getTransactionHash(
                to,
                U256::ZERO,
                data.clone(),
                0u8,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                Address::ZERO,
                Address::ZERO,
                nonce,
            )
            .call()
            .await
            .map_err(|e| {
                PolyError::Execution(format!("getTransactionHash failed: {e}"))
            })?;

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

        // 4. Execute through Safe
        let receipt = safe
            .execTransaction(
                to,
                U256::ZERO,
                data,
                0u8,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                Address::ZERO,
                Address::ZERO,
                Bytes::from(sig_bytes),
            )
            .send()
            .await
            .map_err(|e| PolyError::Execution(format!("execTransaction send failed: {e}")))?
            .get_receipt()
            .await
            .map_err(|e| PolyError::Execution(format!("Failed to get tx receipt: {e}")))?;

        if !receipt.status() {
            return Err(PolyError::Execution(format!(
                "execTransaction reverted (tx: {:#x})",
                receipt.transaction_hash
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
        let provider = ProviderBuilder::new().connect_http(parse_rpc_url(&self.rpc_url)?);
        let ctf = IConditionalTokens::new(CTF_ADDRESS, &provider);

        let mut amounts = Vec::with_capacity(token_ids.len());
        for tid in token_ids {
            let token_u256 = tid.parse::<U256>().map_err(|e| {
                PolyError::Execution(format!("Invalid token_id '{tid}': {e}"))
            })?;
            let bal = ctf
                .balanceOf(self.safe_address, token_u256)
                .call()
                .await
                .map_err(|e| {
                    PolyError::Execution(format!("balanceOf({tid}) failed: {e}"))
                })?;
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
