use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolStruct;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE;
use hmac::{Hmac, Mac};
use polyrust_core::config::PolymarketConfig;
use polyrust_core::error::{PolyError, Result};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

const DEFAULT_RELAYER_URL: &str = "https://relayer-v2.polymarket.com";
const POLYGON_CHAIN_ID: u64 = 137;
const MAX_POLLS: u32 = 60;
const POLL_INTERVAL_MS: u64 = 2000;

// Builder API auth header names (must match polymarket-client-sdk)
const POLY_BUILDER_API_KEY: &str = "POLY_BUILDER_API_KEY";
const POLY_BUILDER_PASSPHRASE: &str = "POLY_BUILDER_PASSPHRASE";
const POLY_BUILDER_SIGNATURE: &str = "POLY_BUILDER_SIGNATURE";
const POLY_BUILDER_TIMESTAMP: &str = "POLY_BUILDER_TIMESTAMP";

// EIP-712 SafeTx type for local signing
sol! {
    struct SafeTx {
        address to;
        uint256 value;
        bytes data;
        uint8 operation;
        uint256 safeTxGas;
        uint256 baseGas;
        uint256 gasPrice;
        address gasToken;
        address refundReceiver;
        uint256 nonce;
    }
}

/// Transaction states returned by the relayer API.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TxState {
    New,
    Executed,
    Mined,
    Confirmed,
    Failed,
    Invalid,
    Unknown(String),
}

impl From<&str> for TxState {
    fn from(s: &str) -> Self {
        match s {
            "STATE_NEW" => Self::New,
            "STATE_EXECUTED" => Self::Executed,
            "STATE_MINED" => Self::Mined,
            "STATE_CONFIRMED" => Self::Confirmed,
            "STATE_FAILED" => Self::Failed,
            "STATE_INVALID" => Self::Invalid,
            other => Self::Unknown(other.to_string()),
        }
    }
}

impl TxState {
    fn is_terminal_failure(&self) -> bool {
        matches!(self, Self::Failed | Self::Invalid)
    }

    fn is_confirmed(&self) -> bool {
        matches!(self, Self::Confirmed)
    }

    fn is_pending(&self) -> bool {
        matches!(self, Self::New | Self::Executed | Self::Mined)
    }
}

#[derive(Debug, Deserialize)]
struct NonceResponse {
    #[serde(deserialize_with = "deserialize_string_or_number")]
    nonce: u64,
}

fn deserialize_string_or_number<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        String(String),
        Number(u64),
    }
    match StringOrNumber::deserialize(deserializer)? {
        StringOrNumber::String(s) => s.parse().map_err(de::Error::custom),
        StringOrNumber::Number(n) => Ok(n),
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SubmitRequest {
    r#type: String,
    from: String,
    to: String,
    proxy_wallet: String,
    data: String,
    nonce: String,
    signature: String,
    signature_params: SignatureParams,
    metadata: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SignatureParams {
    gas_price: String,
    operation: String,
    safe_txn_gas: String,
    base_gas: String,
    gas_token: String,
    refund_receiver: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmitResponse {
    #[serde(default)]
    transaction_id: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransactionResponse {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    transaction_hash: Option<String>,
}

/// Polymarket Builder Relayer client for gasless Safe transactions.
///
/// Routes on-chain transactions through Polymarket's relayer API which sponsors
/// gas fees. Uses builder HMAC authentication and EIP-712 SafeTx signing.
pub struct RelayerClient {
    http: reqwest::Client,
    relayer_url: String,
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    eoa_address: Address,
    safe_address: Address,
    signer: PrivateKeySigner,
    chain_id: u64,
}

impl RelayerClient {
    /// Create a new RelayerClient from config and existing signer/safe.
    pub fn new(
        config: &PolymarketConfig,
        signer: &PrivateKeySigner,
        safe_address: Address,
    ) -> Result<Self> {
        let api_key = config
            .builder_api_key
            .clone()
            .ok_or_else(|| PolyError::Config("Builder API key required for relayer".into()))?;
        let api_secret = config
            .builder_api_secret
            .clone()
            .ok_or_else(|| PolyError::Config("Builder API secret required for relayer".into()))?;
        let api_passphrase = config.builder_api_passphrase.clone().ok_or_else(|| {
            PolyError::Config("Builder API passphrase required for relayer".into())
        })?;

        let relayer_url = config
            .relayer_url
            .clone()
            .unwrap_or_else(|| DEFAULT_RELAYER_URL.to_string());

        let eoa_address = signer.address();

        info!(
            eoa = %eoa_address,
            safe = %safe_address,
            relayer_url = %relayer_url,
            "RelayerClient initialized"
        );

        Ok(Self {
            http: reqwest::Client::new(),
            relayer_url,
            api_key,
            api_secret,
            api_passphrase,
            eoa_address,
            safe_address,
            signer: signer.clone(),
            chain_id: POLYGON_CHAIN_ID,
        })
    }

    /// Generate HMAC auth headers for the builder relayer API.
    ///
    /// Mirrors `polymarket-client-sdk/src/auth.rs::hmac()`:
    /// message = `{timestamp}{METHOD}{path}{body}`, signed with URL-safe base64 secret.
    fn generate_headers(&self, method: &str, path: &str, body: &str) -> Result<HeaderMap> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| PolyError::Execution(format!("System time error: {e}")))?
            .as_secs();

        let message = format!("{timestamp}{method}{path}{body}");

        let decoded_secret = URL_SAFE.decode(&self.api_secret).map_err(|e| {
            PolyError::Execution(format!("Failed to decode builder API secret: {e}"))
        })?;

        let mut mac = Hmac::<Sha256>::new_from_slice(&decoded_secret)
            .map_err(|e| PolyError::Execution(format!("HMAC init failed: {e}")))?;
        mac.update(message.as_bytes());
        let signature = URL_SAFE.encode(mac.finalize().into_bytes());

        let mut headers = HeaderMap::new();
        headers.insert(
            POLY_BUILDER_API_KEY,
            HeaderValue::from_str(&self.api_key)
                .map_err(|e| PolyError::Execution(format!("Invalid API key header: {e}")))?,
        );
        headers.insert(
            POLY_BUILDER_PASSPHRASE,
            HeaderValue::from_str(&self.api_passphrase)
                .map_err(|e| PolyError::Execution(format!("Invalid passphrase header: {e}")))?,
        );
        headers.insert(
            POLY_BUILDER_SIGNATURE,
            HeaderValue::from_str(&signature)
                .map_err(|e| PolyError::Execution(format!("Invalid signature header: {e}")))?,
        );
        headers.insert(
            POLY_BUILDER_TIMESTAMP,
            HeaderValue::from_str(&timestamp.to_string())
                .map_err(|e| PolyError::Execution(format!("Invalid timestamp header: {e}")))?,
        );

        Ok(headers)
    }

    /// Get the Safe nonce from the relayer.
    async fn get_nonce(&self) -> Result<u64> {
        let path = format!("/nonce?address={}&type=SAFE", self.eoa_address);
        let url = format!("{}{}", self.relayer_url, path);

        let headers = self.generate_headers("GET", &path, "")?;

        let resp = self
            .http
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| PolyError::Execution(format!("Relayer GET /nonce failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(PolyError::Execution(format!(
                "Relayer GET /nonce returned {status}: {body}"
            )));
        }

        let nonce_resp: NonceResponse = resp
            .json()
            .await
            .map_err(|e| PolyError::Execution(format!("Failed to parse nonce response: {e}")))?;

        debug!(nonce = nonce_resp.nonce, "Got relayer nonce");
        Ok(nonce_resp.nonce)
    }

    /// Sign a SafeTx using EIP-712 and return the packed signature.
    ///
    /// 1. Build EIP-712 struct hash with domain `{chainId: 137, verifyingContract: safe_address}`
    /// 2. Sign with `sign_message` (adds EIP-191 prefix)
    /// 3. Adjust v: 0,1 → +31; 27,28 → +4 (relayer SDK convention)
    /// 4. Pack as `abi.encodePacked(r, s, v)`
    fn sign_safe_tx(
        &self,
        to: Address,
        data: &Bytes,
        operation: u8,
        nonce: u64,
    ) -> Result<(FixedBytes<32>, Vec<u8>)> {
        let safe_tx = SafeTx {
            to,
            value: U256::ZERO,
            data: data.to_vec().into(),
            operation,
            safeTxGas: U256::ZERO,
            baseGas: U256::ZERO,
            gasPrice: U256::ZERO,
            gasToken: Address::ZERO,
            refundReceiver: Address::ZERO,
            nonce: U256::from(nonce),
        };

        let domain = alloy::dyn_abi::Eip712Domain {
            chain_id: Some(U256::from(self.chain_id)),
            verifying_contract: Some(self.safe_address),
            ..Default::default()
        };

        let struct_hash = safe_tx.eip712_signing_hash(&domain);

        // Sign with EIP-191 prefix (eth_sign): signer.sign_message adds "\x19Ethereum Signed Message:\n32" prefix
        let sig = self
            .signer
            .sign_message_sync(struct_hash.as_slice())
            .map_err(|e| PolyError::Execution(format!("Failed to sign SafeTx: {e}")))?;

        // Adjust v for relayer convention: {0,1} → +31, {27,28} → +4
        let v_raw = sig.v() as u8;
        let v_adjusted = if v_raw <= 1 { v_raw + 31 } else { v_raw + 4 };

        let mut packed = Vec::with_capacity(65);
        packed.extend_from_slice(&sig.r().to_be_bytes::<32>());
        packed.extend_from_slice(&sig.s().to_be_bytes::<32>());
        packed.push(v_adjusted);

        Ok((struct_hash, packed))
    }

    /// Submit a signed transaction to the relayer.
    async fn submit(&self, to: Address, data: &Bytes, operation: u8, nonce: u64) -> Result<String> {
        let (_, signature) = self.sign_safe_tx(to, data, operation, nonce)?;

        let body = SubmitRequest {
            r#type: "SAFE".to_string(),
            from: format!("{}", self.eoa_address),
            to: format!("{to}"),
            proxy_wallet: format!("{}", self.safe_address),
            data: format!("0x{}", hex::encode(data)),
            nonce: nonce.to_string(),
            signature: format!("0x{}", hex::encode(&signature)),
            signature_params: SignatureParams {
                gas_price: "0".to_string(),
                operation: (operation as u32).to_string(),
                safe_txn_gas: "0".to_string(),
                base_gas: "0".to_string(),
                gas_token: format!("{}", Address::ZERO),
                refund_receiver: format!("{}", Address::ZERO),
            },
            metadata: String::new(),
        };

        let body_json = serde_json::to_string(&body)
            .map_err(|e| PolyError::Execution(format!("Failed to serialize submit body: {e}")))?;

        let path = "/submit";
        let headers = self.generate_headers("POST", path, &body_json)?;
        let url = format!("{}{}", self.relayer_url, path);

        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .header("Content-Type", "application/json")
            .body(body_json)
            .send()
            .await
            .map_err(|e| PolyError::Execution(format!("Relayer POST /submit failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(PolyError::Execution(format!(
                "Relayer POST /submit returned {status}: {body}"
            )));
        }

        let submit_resp: SubmitResponse = resp
            .json()
            .await
            .map_err(|e| PolyError::Execution(format!("Failed to parse submit response: {e}")))?;

        let tx_id = submit_resp.transaction_id.ok_or_else(|| {
            PolyError::Execution("Relayer submit response missing transaction_id".into())
        })?;

        let state = submit_resp.state.unwrap_or_else(|| "unknown".to_string());
        debug!(tx_id = %tx_id, state = %state, "Transaction submitted to relayer");

        Ok(tx_id)
    }

    /// Poll transaction status until confirmed or failed.
    async fn poll_until_confirmed(&self, tx_id: &str) -> Result<FixedBytes<32>> {
        for poll in 0..MAX_POLLS {
            tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;

            let path = format!("/transaction?id={tx_id}");
            let url = format!("{}{}", self.relayer_url, path);
            let headers = self.generate_headers("GET", &path, "")?;

            let resp = self
                .http
                .get(&url)
                .headers(headers)
                .send()
                .await
                .map_err(|e| {
                    PolyError::Execution(format!("Relayer GET /transaction failed: {e}"))
                })?;

            if !resp.status().is_success() {
                let status = resp.status();
                warn!(
                    poll,
                    status = %status,
                    "Relayer poll returned error, retrying"
                );
                continue;
            }

            let tx_resp: TransactionResponse = match resp.json().await {
                Ok(r) => r,
                Err(e) => {
                    warn!(poll, error = %e, "Failed to parse transaction response, retrying");
                    continue;
                }
            };

            let state = tx_resp
                .state
                .as_deref()
                .map(TxState::from)
                .unwrap_or(TxState::Unknown("missing".to_string()));

            debug!(poll, tx_id, ?state, "Polled relayer transaction");

            if state.is_confirmed() {
                let tx_hash_str = tx_resp.transaction_hash.ok_or_else(|| {
                    PolyError::Execution(
                        "Relayer confirmed but no transaction_hash returned".into(),
                    )
                })?;
                let tx_hash: FixedBytes<32> = tx_hash_str.parse().map_err(|e| {
                    PolyError::Execution(format!("Invalid tx hash from relayer: {e}"))
                })?;
                return Ok(tx_hash);
            }

            if state.is_terminal_failure() {
                return Err(PolyError::Execution(format!(
                    "Relayer transaction {tx_id} failed with state: {state:?}"
                )));
            }

            if !state.is_pending() {
                warn!(tx_id, ?state, "Unexpected relayer transaction state");
            }
        }

        Err(PolyError::Execution(format!(
            "Relayer transaction {tx_id} timed out after {} polls",
            MAX_POLLS
        )))
    }

    /// Submit a transaction through the relayer and wait for confirmation.
    ///
    /// Orchestrates: get_nonce → submit → poll_until_confirmed.
    pub async fn submit_and_wait(
        &self,
        to: Address,
        data: Bytes,
        operation: u8,
    ) -> Result<FixedBytes<32>> {
        let nonce = self.get_nonce().await?;
        let tx_id = self.submit(to, &data, operation, nonce).await?;

        info!(
            tx_id = %tx_id,
            to = %to,
            operation,
            "Relayer transaction submitted, polling for confirmation"
        );

        self.poll_until_confirmed(&tx_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_sdk_test_vector() {
        // Reproduces the test case from polymarket-client-sdk/src/auth.rs:563-584
        let secret = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        let decoded_secret = URL_SAFE.decode(secret).unwrap();
        let message = r#"1000000test-sign/orders{"hash":"0x123"}"#;

        let mut mac = Hmac::<Sha256>::new_from_slice(&decoded_secret).unwrap();
        mac.update(message.as_bytes());
        let signature = URL_SAFE.encode(mac.finalize().into_bytes());

        assert_eq!(signature, "4gJVbox-R6XlDK4nlaicig0_ANVL1qdcahiL8CXfXLM=");
    }

    #[test]
    fn tx_state_from_str() {
        assert_eq!(TxState::from("STATE_NEW"), TxState::New);
        assert_eq!(TxState::from("STATE_EXECUTED"), TxState::Executed);
        assert_eq!(TxState::from("STATE_MINED"), TxState::Mined);
        assert_eq!(TxState::from("STATE_CONFIRMED"), TxState::Confirmed);
        assert_eq!(TxState::from("STATE_FAILED"), TxState::Failed);
        assert_eq!(TxState::from("STATE_INVALID"), TxState::Invalid);
        assert!(matches!(TxState::from("UNKNOWN"), TxState::Unknown(_)));
    }

    #[test]
    fn tx_state_predicates() {
        assert!(TxState::Failed.is_terminal_failure());
        assert!(TxState::Invalid.is_terminal_failure());
        assert!(!TxState::Confirmed.is_terminal_failure());

        assert!(TxState::Confirmed.is_confirmed());
        assert!(!TxState::Mined.is_confirmed());

        assert!(TxState::New.is_pending());
        assert!(TxState::Executed.is_pending());
        assert!(TxState::Mined.is_pending());
        assert!(!TxState::Confirmed.is_pending());
    }

    #[test]
    fn sign_safe_tx_produces_65_byte_signature() {
        // Use a deterministic test key
        let signer: PrivateKeySigner =
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
                .parse()
                .unwrap();
        let safe_address: Address = "0x1234567890123456789012345678901234567890"
            .parse()
            .unwrap();

        let client = RelayerClient {
            http: reqwest::Client::new(),
            relayer_url: "https://test.example.com".to_string(),
            api_key: "test-key".to_string(),
            api_secret: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
            api_passphrase: "test-pass".to_string(),
            eoa_address: signer.address(),
            safe_address,
            signer,
            chain_id: 137,
        };

        let to: Address = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045"
            .parse()
            .unwrap();
        let data = Bytes::from(vec![0xAA, 0xBB]);

        let (hash, sig) = client.sign_safe_tx(to, &data, 0, 42).unwrap();

        assert_eq!(sig.len(), 65, "Signature must be 65 bytes (r + s + v)");
        assert!(!hash.is_zero(), "Struct hash should not be zero");

        // v byte should be adjusted: v ∈ {31, 32} for eth_sign
        let v = sig[64];
        assert!(
            v == 31 || v == 32,
            "v should be 31 or 32 for eth_sign, got {v}"
        );
    }

    #[test]
    fn generate_headers_contains_all_required() {
        let signer: PrivateKeySigner =
            "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
                .parse()
                .unwrap();

        let client = RelayerClient {
            http: reqwest::Client::new(),
            relayer_url: "https://test.example.com".to_string(),
            api_key: "test-key".to_string(),
            api_secret: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
            api_passphrase: "test-pass".to_string(),
            eoa_address: signer.address(),
            safe_address: Address::ZERO,
            signer,
            chain_id: 137,
        };

        let headers = client.generate_headers("GET", "/nonce", "").unwrap();

        assert!(headers.contains_key(POLY_BUILDER_API_KEY));
        assert!(headers.contains_key(POLY_BUILDER_PASSPHRASE));
        assert!(headers.contains_key(POLY_BUILDER_SIGNATURE));
        assert!(headers.contains_key(POLY_BUILDER_TIMESTAMP));

        assert_eq!(headers[POLY_BUILDER_API_KEY], "test-key");
        assert_eq!(headers[POLY_BUILDER_PASSPHRASE], "test-pass");
    }
}
