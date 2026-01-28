//! Chainlink Historical Price Client
//!
//! Queries on-chain Chainlink aggregator contracts on Polygon to get historical
//! prices at specific timestamps. Used to get the exact settlement reference
//! price for Polymarket's 15-minute Up/Down crypto markets.
//!
//! Polymarket resolves these markets using the Chainlink price closest to
//! `eventStartTime`. This client performs a backward search + forward refinement
//! (with optional binary search) to find that exact round.

use std::collections::HashMap;
use std::fmt;
use std::sync::RwLock;
use std::time::Duration;

use rust_decimal::Decimal;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Chainlink aggregator proxy addresses on Polygon mainnet.
/// Source: <https://docs.chain.link/data-feeds/price-feeds/addresses?network=polygon>
const AGGREGATORS: &[(&str, &str)] = &[
    ("BTC", "0xc907E116054Ad103354f2D350FD2514433D57F6f"),
    ("ETH", "0xF9680D99D6C9589e2a93a78A04A279e509205945"),
    ("SOL", "0x10C8264C0935b3B9870013e057f330Ff3e9C56dC"),
    ("XRP", "0x785ba89291f676b5386652eB12b30cF361020694"),
];

/// Default public RPC endpoint for Polygon PoS.
const DEFAULT_RPC_URL: &str = "https://polygon-rpc.com";

// Solidity function selectors (first 4 bytes of keccak256 of signature).
const SEL_LATEST_ROUND_DATA: [u8; 4] = [0xfe, 0xaf, 0x96, 0x8c]; // latestRoundData()
const SEL_GET_ROUND_DATA: [u8; 4] = [0x9a, 0x6f, 0xc8, 0xf5]; // getRoundData(uint80)
const SEL_DECIMALS: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67]; // decimals()

/// Maximum forward search steps when refining the closest round.
const MAX_FORWARD_CHECKS: u32 = 30;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors returned by the Chainlink client.
#[derive(Debug)]
pub enum ChainlinkError {
    UnsupportedCoin(String),
    Rpc(String),
    Network(String),
    Decode(String),
    NotFound,
}

impl fmt::Display for ChainlinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChainlinkError::UnsupportedCoin(c) => write!(f, "unsupported coin: {c}"),
            ChainlinkError::Rpc(msg) => write!(f, "RPC error: {msg}"),
            ChainlinkError::Network(msg) => write!(f, "network error: {msg}"),
            ChainlinkError::Decode(msg) => write!(f, "decode error: {msg}"),
            ChainlinkError::NotFound => write!(f, "price not found for target timestamp"),
        }
    }
}

impl std::error::Error for ChainlinkError {}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Price data from a single Chainlink aggregator round.
#[derive(Debug, Clone)]
pub struct ChainlinkPrice {
    pub price: Decimal,
    /// Unix timestamp of the round's `updatedAt` field.
    pub timestamp: u64,
    pub round_id: u128,
    pub decimals: u8,
}

/// Raw round data decoded from ABI response.
#[derive(Debug)]
struct RoundData {
    round_id: u128,
    answer: i128,
    updated_at: u64,
}

/// Mutable state shared between forward search and binary refinement.
struct SearchState {
    target_ts: u64,
    before: Option<(u128, i128, u64)>, // (round_id, answer, timestamp)
    after: Option<(u128, i128, u64)>,
    forward_checks: u32,
}

// ---------------------------------------------------------------------------
// JSON-RPC response
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct JsonRpcResponse {
    result: Option<String>,
    error: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Async client for querying historical prices from Chainlink aggregators
/// on Polygon via JSON-RPC `eth_call`.
///
/// Supports multiple RPC endpoints with automatic failover on rate limits
/// and exponential backoff on transient errors.
pub struct ChainlinkHistoricalClient {
    http: reqwest::Client,
    rpc_urls: Vec<String>,
    /// Index into `rpc_urls` for the currently preferred RPC.
    current_rpc: RwLock<usize>,
    /// Cached `decimals()` per coin (immutable once fetched).
    decimals_cache: RwLock<HashMap<String, u8>>,
    /// Coin → checksummed aggregator address.
    addresses: HashMap<String, String>,
}

impl ChainlinkHistoricalClient {
    /// Create a new client with the given RPC endpoints (tried in order on failover).
    pub fn new(rpc_urls: Vec<String>) -> Self {
        let urls = if rpc_urls.is_empty() {
            vec![DEFAULT_RPC_URL.to_string()]
        } else {
            rpc_urls
        };

        let addresses: HashMap<String, String> = AGGREGATORS
            .iter()
            .map(|(coin, addr)| (coin.to_string(), addr.to_string()))
            .collect();

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");

        Self {
            http,
            rpc_urls: urls,
            current_rpc: RwLock::new(0),
            decimals_cache: RwLock::new(HashMap::new()),
            addresses,
        }
    }

    /// Create a client using the default public Polygon RPC.
    pub fn with_default_rpcs() -> Self {
        Self::new(vec![DEFAULT_RPC_URL.to_string()])
    }

    /// Coins with known Chainlink aggregator addresses.
    pub fn supported_coins() -> &'static [&'static str] {
        &["BTC", "ETH", "SOL", "XRP"]
    }

    // -- Public API ---------------------------------------------------------

    /// Get the Chainlink price closest to `target_timestamp` (unix seconds).
    ///
    /// Performs a backward search from the latest round, then a forward
    /// refinement with binary search, to find the round nearest to the target.
    /// This matches how Polymarket resolves 15-min markets.
    ///
    /// `max_rounds` limits total backward search steps (default: 100).
    pub async fn get_price_at_timestamp(
        &self,
        coin: &str,
        target_timestamp: u64,
        max_rounds: u32,
    ) -> Result<ChainlinkPrice, ChainlinkError> {
        let coin = coin.to_uppercase();
        let decimals = self.fetch_decimals(&coin).await?;

        // Get latest round as starting point
        let latest = self.call_latest_round_data(&coin).await?;

        // If target is in the future or very recent, return latest
        if target_timestamp >= latest.updated_at {
            debug!(
                coin = %coin,
                target = target_timestamp,
                latest_ts = latest.updated_at,
                "Target >= latest round, returning latest price"
            );
            return Ok(Self::make_price(latest, decimals));
        }

        // Decompose composite round ID: (phaseId << 64) | aggregatorRoundId
        let phase_id = latest.round_id >> 64;
        let aggregator_round = latest.round_id & ((1u128 << 64) - 1);

        let mut state = SearchState {
            target_ts: target_timestamp,
            before: None,
            after: None,
            forward_checks: 0,
        };

        let mut current_round = aggregator_round;
        let mut rounds_checked: u32 = 0;

        // --- Backward search: find the latest round AT or BEFORE target ---
        while rounds_checked < max_rounds && current_round > 0 {
            let round_id = (phase_id << 64) | current_round;

            match self.call_get_round_data(&coin, round_id).await {
                Ok(rd) => {
                    if rd.updated_at <= target_timestamp {
                        // Found a round at or before target
                        if state.before.is_none_or(|b| rd.updated_at > b.2) {
                            state.before = Some((round_id, rd.answer, rd.updated_at));
                            debug!(
                                coin = %coin,
                                round = current_round,
                                ts = rd.updated_at,
                                diff_s = target_timestamp - rd.updated_at,
                                "Found BEFORE round"
                            );
                        }

                        // Forward search + binary refinement
                        self.refine_forward(&coin, phase_id, current_round, &mut state)
                            .await;

                        break; // Done with backward search
                    } else {
                        // This round is AFTER target — record and skip back
                        if state.after.is_none_or(|a| rd.updated_at < a.2) {
                            state.after = Some((round_id, rd.answer, rd.updated_at));
                            debug!(
                                coin = %coin,
                                round = current_round,
                                diff_s = rd.updated_at - target_timestamp,
                                "Round is AFTER target"
                            );
                        }
                        // Skip backwards proportionally (~30s per round estimate)
                        let time_diff = rd.updated_at - target_timestamp;
                        let skip = (time_diff / 30).max(1) as u128;
                        current_round = current_round.saturating_sub(skip);
                    }
                }
                Err(e) => {
                    debug!(
                        coin = %coin,
                        round = current_round,
                        error = %e,
                        "Round lookup failed, trying previous"
                    );
                    current_round = current_round.saturating_sub(1);
                }
            }
            rounds_checked += 1;
        }

        // --- Select the closest round ---
        self.select_closest(coin, target_timestamp, state.before, state.after, decimals, rounds_checked)
    }

    // -- Private helpers ----------------------------------------------------

    /// Forward search from `start_round + 1` to find the first round AFTER target,
    /// then binary-search to narrow down the exact boundary.
    async fn refine_forward(
        &self,
        coin: &str,
        phase_id: u128,
        start_round: u128,
        state: &mut SearchState,
    ) {
        let mut search_round = start_round + 1;
        let mut last_before_round = start_round;

        while state.forward_checks < MAX_FORWARD_CHECKS {
            let next_id = (phase_id << 64) | search_round;
            match self.call_get_round_data(coin, next_id).await {
                Ok(rd) => {
                    state.forward_checks += 1;

                    if rd.updated_at > state.target_ts {
                        // Found an AFTER round
                        if state.after.is_none_or(|a| rd.updated_at < a.2) {
                            state.after = Some((next_id, rd.answer, rd.updated_at));
                        }

                        // Binary search between last_before and this round
                        if search_round > last_before_round + 1 {
                            self.binary_refine(
                                coin,
                                phase_id,
                                (last_before_round + 1, search_round - 1),
                                state,
                            )
                            .await;
                        }

                        debug!(
                            coin = %coin,
                            before_diff = state.before.map(|b| state.target_ts - b.2),
                            after_diff = state.after.map(|a| a.2 - state.target_ts),
                            "Bracketing rounds found"
                        );
                        break;
                    } else {
                        // Still before target — update best BEFORE
                        if state.before.is_none_or(|b| rd.updated_at > b.2) {
                            state.before = Some((next_id, rd.answer, rd.updated_at));
                        }
                        last_before_round = search_round;
                        // Skip forward proportionally
                        let time_to_target = state.target_ts.saturating_sub(rd.updated_at);
                        let skip = (time_to_target / 30).max(1) as u128;
                        search_round += skip;
                    }
                }
                Err(_) => {
                    search_round += 1;
                    state.forward_checks += 1;
                }
            }
        }
    }

    /// Binary search between `lo` and `hi` (aggregator round numbers) to find
    /// the tightest BEFORE/AFTER bracket around `target_ts`.
    async fn binary_refine(
        &self,
        coin: &str,
        phase_id: u128,
        range: (u128, u128),
        state: &mut SearchState,
    ) {
        let (mut lo, mut hi) = range;
        while lo <= hi && state.forward_checks < MAX_FORWARD_CHECKS {
            let mid = lo + (hi - lo) / 2;
            let mid_id = (phase_id << 64) | mid;
            match self.call_get_round_data(coin, mid_id).await {
                Ok(rd) => {
                    state.forward_checks += 1;
                    if rd.updated_at <= state.target_ts {
                        if state.before.is_none_or(|b| rd.updated_at > b.2) {
                            state.before = Some((mid_id, rd.answer, rd.updated_at));
                        }
                        lo = mid + 1;
                    } else {
                        if state.after.is_none_or(|a| rd.updated_at < a.2) {
                            state.after = Some((mid_id, rd.answer, rd.updated_at));
                        }
                        if mid == 0 {
                            break;
                        }
                        hi = mid - 1;
                    }
                }
                Err(_) => {
                    lo = mid + 1; // skip bad round
                }
            }
        }
    }

    /// Pick the round closest to `target_ts` from the BEFORE/AFTER candidates.
    fn select_closest(
        &self,
        coin: String,
        target_ts: u64,
        before: Option<(u128, i128, u64)>,
        after: Option<(u128, i128, u64)>,
        decimals: u8,
        rounds_checked: u32,
    ) -> Result<ChainlinkPrice, ChainlinkError> {
        match (before, after) {
            (None, None) => {
                warn!(
                    coin = %coin,
                    target = target_ts,
                    rounds_checked = rounds_checked,
                    "Could not find price at target timestamp"
                );
                Err(ChainlinkError::NotFound)
            }
            (Some((rid, answer, ts)), None) | (None, Some((rid, answer, ts))) => {
                Ok(ChainlinkPrice {
                    price: Self::raw_to_decimal(answer, decimals),
                    timestamp: ts,
                    round_id: rid,
                    decimals,
                })
            }
            (Some((b_rid, b_ans, b_ts)), Some((a_rid, a_ans, a_ts))) => {
                let before_diff = target_ts - b_ts;
                let after_diff = a_ts - target_ts;

                let (rid, answer, ts, side) = if before_diff <= after_diff {
                    (b_rid, b_ans, b_ts, "before")
                } else {
                    (a_rid, a_ans, a_ts, "after")
                };

                debug!(
                    coin = %coin,
                    selected = side,
                    before_diff_s = before_diff,
                    after_diff_s = after_diff,
                    "Selected closest round"
                );

                Ok(ChainlinkPrice {
                    price: Self::raw_to_decimal(answer, decimals),
                    timestamp: ts,
                    round_id: rid,
                    decimals,
                })
            }
        }
    }

    // -- Contract calls -----------------------------------------------------

    async fn fetch_decimals(&self, coin: &str) -> Result<u8, ChainlinkError> {
        {
            let cache = self.decimals_cache.read().unwrap();
            if let Some(&d) = cache.get(coin) {
                return Ok(d);
            }
        }

        let result = self.eth_call(coin, &SEL_DECIMALS).await?;
        if result.len() < 32 {
            return Err(ChainlinkError::Decode("decimals response too short".into()));
        }
        let decimals = result[31]; // last byte of 32-byte uint8 slot

        self.decimals_cache.write().unwrap().insert(coin.to_string(), decimals);
        Ok(decimals)
    }

    async fn call_latest_round_data(&self, coin: &str) -> Result<RoundData, ChainlinkError> {
        let result = self.eth_call(coin, &SEL_LATEST_ROUND_DATA).await?;
        Self::decode_round_data(&result)
    }

    async fn call_get_round_data(
        &self,
        coin: &str,
        round_id: u128,
    ) -> Result<RoundData, ChainlinkError> {
        let mut calldata = Vec::with_capacity(36);
        calldata.extend_from_slice(&SEL_GET_ROUND_DATA);
        // ABI-encode uint80: right-aligned in 32 bytes
        let mut arg = [0u8; 32];
        arg[16..32].copy_from_slice(&round_id.to_be_bytes());
        calldata.extend_from_slice(&arg);

        let result = self.eth_call(coin, &calldata).await?;
        Self::decode_round_data(&result)
    }

    // -- Low-level RPC ------------------------------------------------------

    /// Execute `eth_call` against the aggregator contract for `coin`.
    /// Retries with exponential backoff and switches RPC on rate limits.
    async fn eth_call(&self, coin: &str, data: &[u8]) -> Result<Vec<u8>, ChainlinkError> {
        let address = self
            .addresses
            .get(coin)
            .ok_or_else(|| ChainlinkError::UnsupportedCoin(coin.to_string()))?;

        let data_hex = encode_hex(data);
        let num_rpcs = self.rpc_urls.len();
        let max_retries = 3u32;
        let mut last_error = ChainlinkError::Rpc("no RPCs configured".into());

        for rpc_offset in 0..num_rpcs {
            let rpc_idx = {
                let idx = *self.current_rpc.read().unwrap();
                (idx + rpc_offset) % num_rpcs
            };
            let rpc_url = &self.rpc_urls[rpc_idx];

            for attempt in 0..max_retries {
                let body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "eth_call",
                    "params": [{"to": address, "data": &data_hex}, "latest"],
                    "id": 1
                });

                let resp = match self.http.post(rpc_url).json(&body).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        last_error = ChainlinkError::Network(e.to_string());
                        if attempt < max_retries - 1 {
                            tokio::time::sleep(backoff_delay(attempt)).await;
                        }
                        continue;
                    }
                };

                // Rate limited — switch to next RPC
                if resp.status().as_u16() == 429 {
                    if let Ok(mut idx) = self.current_rpc.write() {
                        *idx = (rpc_idx + 1) % num_rpcs;
                    }
                    last_error = ChainlinkError::Rpc("rate limited".into());
                    break; // Try next RPC
                }

                let json_resp: JsonRpcResponse = match resp.json().await {
                    Ok(r) => r,
                    Err(e) => {
                        last_error = ChainlinkError::Network(e.to_string());
                        if attempt < max_retries - 1 {
                            tokio::time::sleep(backoff_delay(attempt)).await;
                        }
                        continue;
                    }
                };

                if let Some(err) = json_resp.error {
                    last_error = ChainlinkError::Rpc(err.to_string());
                    if attempt < max_retries - 1 {
                        tokio::time::sleep(backoff_delay(attempt)).await;
                    }
                    continue;
                }

                if let Some(result_hex) = json_resp.result {
                    return decode_hex(&result_hex);
                }

                last_error = ChainlinkError::Rpc("null result from RPC".into());
            }
        }

        Err(last_error)
    }

    // -- ABI decoding -------------------------------------------------------

    /// Decode the 5-slot ABI return from `latestRoundData` / `getRoundData`.
    ///
    /// Layout (each 32 bytes):
    /// - slot 0: uint80  roundId
    /// - slot 1: int256  answer
    /// - slot 2: uint256 startedAt
    /// - slot 3: uint256 updatedAt
    /// - slot 4: uint80  answeredInRound
    fn decode_round_data(data: &[u8]) -> Result<RoundData, ChainlinkError> {
        if data.len() < 160 {
            return Err(ChainlinkError::Decode(format!(
                "round data too short: {} bytes (need 160)",
                data.len()
            )));
        }

        // slot 0 — roundId (uint80, fits in u128)
        let round_id = u128::from_be_bytes(
            data[16..32].try_into().expect("16 bytes for u128"),
        );

        // slot 1 — answer (int256; for prices always positive, fits in i128)
        let answer = i128::from_be_bytes(
            data[48..64].try_into().expect("16 bytes for i128"),
        );

        // slot 3 — updatedAt (uint256, fits in u64)
        let updated_at = u64::from_be_bytes(
            data[120..128].try_into().expect("8 bytes for u64"),
        );

        // Sanity: zero timestamp means the round doesn't exist
        if updated_at == 0 {
            return Err(ChainlinkError::Rpc("round not found (zero timestamp)".into()));
        }
        if answer <= 0 {
            return Err(ChainlinkError::Decode("invalid price (non-positive answer)".into()));
        }

        Ok(RoundData {
            round_id,
            answer,
            updated_at,
        })
    }

    // -- Price conversion ---------------------------------------------------

    fn raw_to_decimal(raw_answer: i128, decimals: u8) -> Decimal {
        Decimal::from_i128_with_scale(raw_answer, decimals as u32)
    }

    fn make_price(rd: RoundData, decimals: u8) -> ChainlinkPrice {
        ChainlinkPrice {
            price: Self::raw_to_decimal(rd.answer, decimals),
            timestamp: rd.updated_at,
            round_id: rd.round_id,
            decimals,
        }
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Exponential backoff delay: 500ms, 1s, 2s, …
fn backoff_delay(attempt: u32) -> Duration {
    Duration::from_millis(500 * 2u64.pow(attempt))
}

/// Encode bytes as a `0x`-prefixed hex string.
fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2 + 2);
    s.push_str("0x");
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Decode a `0x`-prefixed hex string into bytes.
fn decode_hex(s: &str) -> Result<Vec<u8>, ChainlinkError> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if !s.len().is_multiple_of(2) {
        return Err(ChainlinkError::Decode("odd hex string length".into()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| ChainlinkError::Decode(format!("bad hex: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let bytes = [0xfe, 0xaf, 0x96, 0x8c];
        let hex = encode_hex(&bytes);
        assert_eq!(hex, "0xfeaf968c");
        let decoded = decode_hex(&hex).unwrap();
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn decode_round_data_valid() {
        // Build a fake 160-byte response
        let mut data = vec![0u8; 160];

        // slot 0: roundId = (18 << 64) | 42
        let round_id: u128 = (18u128 << 64) | 42;
        data[16..32].copy_from_slice(&round_id.to_be_bytes());

        // slot 1: answer = 5_000_000_000_000 (BTC at $50,000 with 8 decimals)
        let answer: i128 = 5_000_000_000_000;
        data[48..64].copy_from_slice(&answer.to_be_bytes());

        // slot 3: updatedAt = 1706000000
        let updated_at: u64 = 1706000000;
        data[120..128].copy_from_slice(&updated_at.to_be_bytes());

        let rd = ChainlinkHistoricalClient::decode_round_data(&data).unwrap();
        assert_eq!(rd.round_id, round_id);
        assert_eq!(rd.answer, answer);
        assert_eq!(rd.updated_at, updated_at);
    }

    #[test]
    fn decode_round_data_zero_timestamp_is_error() {
        let mut data = vec![0u8; 160];
        // Non-zero answer but zero updatedAt
        let answer: i128 = 1000;
        data[48..64].copy_from_slice(&answer.to_be_bytes());
        // updatedAt stays 0

        let result = ChainlinkHistoricalClient::decode_round_data(&data);
        assert!(result.is_err());
    }

    #[test]
    fn raw_to_decimal_btc() {
        // BTC at $50,000.12345678 with 8 decimals
        let raw: i128 = 5_000_012_345_678;
        let price = ChainlinkHistoricalClient::raw_to_decimal(raw, 8);
        assert_eq!(price, Decimal::new(5_000_012_345_678, 8));
        assert_eq!(price.to_string(), "50000.12345678");
    }

    #[test]
    fn raw_to_decimal_eth() {
        // ETH at $3,500.00 with 8 decimals
        let raw: i128 = 350_000_000_000;
        let price = ChainlinkHistoricalClient::raw_to_decimal(raw, 8);
        assert_eq!(price.to_string(), "3500.00000000");
    }

    #[test]
    fn supported_coins_list() {
        let coins = ChainlinkHistoricalClient::supported_coins();
        assert!(coins.contains(&"BTC"));
        assert!(coins.contains(&"ETH"));
        assert!(coins.contains(&"SOL"));
        assert!(coins.contains(&"XRP"));
    }

    #[test]
    fn encode_get_round_data_calldata() {
        // Verify ABI encoding for getRoundData(uint80)
        let round_id: u128 = (18u128 << 64) | 12345;
        let mut calldata = Vec::with_capacity(36);
        calldata.extend_from_slice(&SEL_GET_ROUND_DATA);
        let mut arg = [0u8; 32];
        arg[16..32].copy_from_slice(&round_id.to_be_bytes());
        calldata.extend_from_slice(&arg);

        assert_eq!(calldata.len(), 36);
        assert_eq!(&calldata[0..4], &SEL_GET_ROUND_DATA);
        // The round_id should be right-aligned in the 32-byte arg
        assert_eq!(calldata[4..20], [0u8; 16]);
    }

    // -----------------------------------------------------------------------
    // Live RPC integration tests — require network access.
    // Run with: cargo test -p polyrust-market -- --ignored
    // -----------------------------------------------------------------------

    #[tokio::test]
    #[ignore]
    async fn live_latest_round_data_btc() {
        let client = ChainlinkHistoricalClient::with_default_rpcs();
        let rd = client.call_latest_round_data("BTC").await.unwrap();

        // Sanity: BTC price should be > $1,000 and < $1,000,000
        let decimals = client.fetch_decimals("BTC").await.unwrap();
        assert_eq!(decimals, 8, "BTC Chainlink feed uses 8 decimals");

        let price = ChainlinkHistoricalClient::raw_to_decimal(rd.answer, decimals);
        assert!(price > Decimal::new(1_000, 0), "BTC price {price} too low");
        assert!(price < Decimal::new(1_000_000, 0), "BTC price {price} too high");

        // Timestamp should be recent (within last hour)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            now - rd.updated_at < 3600,
            "Latest round timestamp too old: {}s ago",
            now - rd.updated_at
        );

        println!("BTC latest: ${price} at ts={} (round_id={})", rd.updated_at, rd.round_id);
    }

    #[tokio::test]
    #[ignore]
    async fn live_latest_round_data_eth() {
        let client = ChainlinkHistoricalClient::with_default_rpcs();
        let decimals = client.fetch_decimals("ETH").await.unwrap();
        assert_eq!(decimals, 8);

        let rd = client.call_latest_round_data("ETH").await.unwrap();
        let price = ChainlinkHistoricalClient::raw_to_decimal(rd.answer, decimals);
        assert!(price > Decimal::new(100, 0), "ETH price {price} too low");
        assert!(price < Decimal::new(100_000, 0), "ETH price {price} too high");

        println!("ETH latest: ${price} at ts={}", rd.updated_at);
    }

    #[tokio::test]
    #[ignore]
    async fn live_latest_round_data_sol() {
        let client = ChainlinkHistoricalClient::with_default_rpcs();
        let rd = client.call_latest_round_data("SOL").await.unwrap();
        let decimals = client.fetch_decimals("SOL").await.unwrap();
        let price = ChainlinkHistoricalClient::raw_to_decimal(rd.answer, decimals);
        assert!(price > Decimal::new(1, 0), "SOL price {price} too low");
        assert!(price < Decimal::new(10_000, 0), "SOL price {price} too high");

        println!("SOL latest: ${price} at ts={}", rd.updated_at);
    }

    #[tokio::test]
    #[ignore]
    async fn live_historical_price_btc() {
        let client = ChainlinkHistoricalClient::with_default_rpcs();

        // Query price 15 minutes ago
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let target = now - 900; // 15 min ago

        let price = client
            .get_price_at_timestamp("BTC", target, 100)
            .await
            .unwrap();

        // Sanity checks
        assert!(price.price > Decimal::new(1_000, 0), "BTC price {0} too low", price.price);
        assert!(price.price < Decimal::new(1_000_000, 0), "BTC price {0} too high", price.price);

        // The returned timestamp should be within ~60s of target
        let diff = price.timestamp.abs_diff(target);
        assert!(
            diff < 60,
            "Historical price timestamp {diff}s away from target (expected <60s)"
        );

        println!(
            "BTC at -15m: ${} (ts={}, diff={}s, round={})",
            price.price, price.timestamp, diff, price.round_id
        );
    }

    #[tokio::test]
    #[ignore]
    async fn live_historical_price_btc_1h_ago() {
        let client = ChainlinkHistoricalClient::with_default_rpcs();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let target = now - 3600; // 1 hour ago

        let price = client
            .get_price_at_timestamp("BTC", target, 100)
            .await
            .unwrap();

        assert!(price.price > Decimal::new(1_000, 0));
        assert!(price.price < Decimal::new(1_000_000, 0));

        let diff = price.timestamp.abs_diff(target);
        assert!(
            diff < 120,
            "Historical price timestamp {diff}s away from target (expected <120s)"
        );

        println!(
            "BTC at -1h: ${} (ts={}, diff={}s, round={})",
            price.price, price.timestamp, diff, price.round_id
        );
    }

    #[tokio::test]
    #[ignore]
    async fn live_historical_price_all_coins() {
        let client = ChainlinkHistoricalClient::with_default_rpcs();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let target = now - 900; // 15 min ago

        for coin in ChainlinkHistoricalClient::supported_coins() {
            let result = client.get_price_at_timestamp(coin, target, 100).await;
            let price = result.unwrap_or_else(|e| panic!("{coin} lookup failed: {e}"));
            let diff = price.timestamp.abs_diff(target);

            println!(
                "{coin}: ${} (diff={}s, decimals={}, round={})",
                price.price, diff, price.decimals, price.round_id
            );

            assert!(price.price > Decimal::ZERO, "{coin} price should be positive");
            assert!(diff < 120, "{coin} diff {diff}s too large");
        }
    }
}
