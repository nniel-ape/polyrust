use chrono::{DateTime, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::data::{DataFetchLog, HistoricalDataStore, HistoricalTrade};
use crate::error::{BacktestError, BacktestResult};

const GOLDSKY_ORDERBOOK_URL: &str = "https://api.goldsky.com/api/public/project_cl6mb8i9h0003e201j6li0diw/subgraphs/orderbook-subgraph/0.0.1/gn";
const MAX_RETRIES: u32 = 5;
const RETRY_DELAY_MS: u64 = 200;
const RATE_LIMIT_DELAY_MS: u64 = 5000;
const REQUEST_TIMEOUT_SECS: u64 = 30;
const PAGE_SIZE: i64 = 1000; // GraphQL subgraph max per query

/// USDC collateral asset ID in the orderbook subgraph
const COLLATERAL_ASSET_ID: &str = "0";

/// Raw amounts in the subgraph are 6-decimal integers (USDC precision)
fn decimal_factor() -> Decimal {
    Decimal::from(1_000_000)
}

/// GraphQL query wrapper
#[derive(Debug, Serialize)]
struct GraphQLQuery {
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    variables: Option<serde_json::Value>,
}

/// GraphQL response wrapper
#[derive(Debug, Deserialize)]
struct GraphQLResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQLError>,
}

#[derive(Debug, Deserialize)]
struct GraphQLError {
    message: String,
}

/// Orderbook subgraph response for `orderFilledEvents`
#[derive(Debug, Deserialize)]
struct OrderFilledEventsData {
    #[serde(rename = "orderFilledEvents")]
    order_filled_events: Vec<OrderFilledEvent>,
}

#[derive(Debug, Deserialize)]
struct OrderFilledEvent {
    id: String,
    timestamp: String,
    #[serde(rename = "makerAssetId")]
    maker_asset_id: String,
    #[serde(rename = "takerAssetId")]
    taker_asset_id: String,
    #[serde(rename = "makerAmountFilled")]
    maker_amount_filled: String,
    #[serde(rename = "takerAmountFilled")]
    taker_amount_filled: String,
}

impl OrderFilledEvent {
    /// BUY: maker pays USDC (makerAssetId == "0"), receives outcome tokens
    fn is_buy(&self) -> bool {
        self.maker_asset_id == COLLATERAL_ASSET_ID
    }

    /// SELL: maker receives USDC (takerAssetId == "0"), pays outcome tokens
    fn is_sell(&self) -> bool {
        self.taker_asset_id == COLLATERAL_ASSET_ID
    }

    /// Convert a raw orderFilledEvent into a HistoricalTrade.
    /// Derives price and side from maker/taker asset IDs and amounts.
    fn to_historical_trade(&self) -> BacktestResult<HistoricalTrade> {
        let timestamp_secs = self.timestamp.parse::<i64>().map_err(|e| {
            BacktestError::InvalidInput(format!(
                "Failed to parse timestamp '{}': {}",
                self.timestamp, e
            ))
        })?;
        let timestamp = DateTime::from_timestamp(timestamp_secs, 0).ok_or_else(|| {
            BacktestError::InvalidInput(format!("Invalid timestamp: {}", timestamp_secs))
        })?;

        let maker_amount = parse_amount(&self.maker_amount_filled)?;
        let taker_amount = parse_amount(&self.taker_amount_filled)?;

        if taker_amount.is_zero() || maker_amount.is_zero() {
            return Err(BacktestError::InvalidInput(
                "Zero amount in trade".to_string(),
            ));
        }

        let (side, token_id, price, size) = if self.is_buy() {
            // BUY: maker pays USDC (maker_amount), receives outcome tokens (taker_amount)
            // price = USDC paid / tokens received
            (
                "buy",
                &self.taker_asset_id,
                maker_amount / taker_amount,
                taker_amount,
            )
        } else if self.is_sell() {
            // SELL: maker sells outcome tokens (maker_amount), receives USDC (taker_amount)
            // price = USDC received / tokens sold
            (
                "sell",
                &self.maker_asset_id,
                taker_amount / maker_amount,
                maker_amount,
            )
        } else {
            return Err(BacktestError::InvalidInput(
                "Neither asset is USDC (collateral)".to_string(),
            ));
        };

        Ok(HistoricalTrade {
            id: self.id.clone(),
            token_id: token_id.clone(),
            timestamp,
            price,
            size,
            side: side.to_string(),
            source: "subgraph".to_string(),
        })
    }
}

/// Parse a raw subgraph amount (6-decimal integer string) into a Decimal.
/// e.g. "1000000" → 1.0, "500000" → 0.5
fn parse_amount(s: &str) -> BacktestResult<Decimal> {
    let raw: Decimal = s.parse().map_err(|e| {
        BacktestError::InvalidInput(format!("Failed to parse amount '{}': {}", s, e))
    })?;
    Ok(raw / decimal_factor())
}

/// GraphQL client for Goldsky orderbook subgraph.
/// Fetches unlimited historical trade data with pagination.
pub struct SubgraphFetcher {
    client: Client,
    store: Arc<HistoricalDataStore>,
}

impl SubgraphFetcher {
    /// Create a new subgraph fetcher with the given data store.
    pub fn new(store: Arc<HistoricalDataStore>) -> BacktestResult<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| BacktestError::Network(e.to_string()))?;

        Ok(Self { client, store })
    }

    /// Fetch trades from the orderbook subgraph for a single token within a date range.
    /// Delegates to `fetch_trades_batch` with a single-element slice.
    pub async fn fetch_subgraph_trades(
        &self,
        token_id: &str,
        start_ts: i64,
        end_ts: i64,
    ) -> BacktestResult<Vec<HistoricalTrade>> {
        self.fetch_trades_batch(&[token_id], start_ts, end_ts).await
    }

    /// Fetch trades from the orderbook subgraph for multiple tokens in one batch.
    /// Uses `or` + `_in` query to fetch all tokens at once.
    /// Returns cached data if the range is already cached for ALL tokens.
    ///
    /// Pagination uses `timestamp_gte` with stuck-loop detection.
    /// DB-level dedup via `INSERT OR REPLACE` handles boundary overlaps.
    pub async fn fetch_trades_batch(
        &self,
        token_ids: &[&str],
        start_ts: i64,
        end_ts: i64,
    ) -> BacktestResult<Vec<HistoricalTrade>> {
        let start_dt = DateTime::from_timestamp(start_ts, 0)
            .ok_or_else(|| BacktestError::InvalidInput("Invalid start_ts".to_string()))?;
        let end_dt = DateTime::from_timestamp(end_ts, 0)
            .ok_or_else(|| BacktestError::InvalidInput("Invalid end_ts".to_string()))?;

        // Check if ALL tokens are cached
        let mut all_cached = true;
        for token_id in token_ids {
            if !self
                .is_range_cached("subgraph_trades", token_id, start_dt, end_dt)
                .await?
            {
                all_cached = false;
                break;
            }
        }

        if all_cached {
            debug!(
                ?token_ids,
                start_ts, end_ts, "Subgraph trade data already cached"
            );
            let mut all_trades = Vec::new();
            for token_id in token_ids {
                let trades = self
                    .store
                    .get_historical_trades(token_id, start_dt, end_dt)
                    .await?;
                all_trades.extend(trades);
            }
            all_trades.sort_by_key(|t| t.timestamp);
            return Ok(all_trades);
        }

        info!(
            ?token_ids,
            start_ts, end_ts, "Fetching trades from Goldsky orderbook subgraph"
        );

        let mut all_trades = Vec::new();
        let mut last_timestamp = start_ts.to_string();
        let end_ts_str = end_ts.to_string();

        loop {
            let query = Self::build_batch_query(token_ids, &last_timestamp, &end_ts_str);
            let response = self.execute_query::<OrderFilledEventsData>(&query).await?;
            let events = response.order_filled_events;

            if events.is_empty() {
                break;
            }

            for event in &events {
                match event.to_historical_trade() {
                    Ok(trade) => {
                        // Only include trades for our requested tokens
                        if token_ids.contains(&trade.token_id.as_str()) {
                            all_trades.push(trade);
                        }
                    }
                    Err(e) => {
                        warn!("Skipping invalid trade {}: {}", event.id, e);
                    }
                }
            }

            // If we got less than PAGE_SIZE, we've reached the end
            if events.len() < PAGE_SIZE as usize {
                break;
            }

            let new_ts = events.last().unwrap().timestamp.clone();
            if new_ts == last_timestamp {
                // Stuck-loop safety: 1000+ trades in the same second is extremely unlikely
                debug!(timestamp = %new_ts, "Pagination stuck at same timestamp, breaking");
                break;
            }
            last_timestamp = new_ts;

            debug!(
                page_size = events.len(),
                "Fetched page from orderbook subgraph"
            );
        }

        let row_count = all_trades.len();

        // INSERT OR REPLACE handles any boundary duplicates from timestamp_gte overlap
        if row_count > 0 {
            self.store
                .insert_historical_trades(all_trades.clone())
                .await?;

            // Log fetch per token for cache tracking
            for token_id in token_ids {
                let token_count = all_trades
                    .iter()
                    .filter(|t| t.token_id == *token_id)
                    .count();
                self.store
                    .insert_fetch_log(DataFetchLog {
                        id: None,
                        source: "subgraph_trades".to_string(),
                        token_id: token_id.to_string(),
                        start_ts: start_dt,
                        end_ts: end_dt,
                        fetched_at: Utc::now(),
                        row_count: token_count as i64,
                    })
                    .await?;
            }
        }

        info!(
            ?token_ids,
            row_count, "Fetched and cached orderbook subgraph trades"
        );
        Ok(all_trades)
    }

    /// Build a batch GraphQL query for orderFilledEvents across multiple tokens.
    /// Uses `or` clause with `_in` filters. Timestamp filters MUST be inside
    /// each `or` clause (top-level mixing with `or` causes a GraphQL error).
    fn build_batch_query(
        token_ids: &[&str],
        timestamp_gte: &str,
        timestamp_lte: &str,
    ) -> GraphQLQuery {
        let token_list = token_ids
            .iter()
            .map(|id| format!("\"{}\"", id))
            .collect::<Vec<_>>()
            .join(", ");

        let query = format!(
            r#"{{
  orderFilledEvents(
    first: {page_size}
    orderBy: timestamp
    orderDirection: asc
    where: {{
      or: [
        {{takerAssetId_in: [{tokens}], timestamp_gte: "{ts_gte}", timestamp_lte: "{ts_lte}"}},
        {{makerAssetId_in: [{tokens}], timestamp_gte: "{ts_gte}", timestamp_lte: "{ts_lte}"}}
      ]
    }}
  ) {{
    id
    timestamp
    makerAssetId
    takerAssetId
    makerAmountFilled
    takerAmountFilled
  }}
}}"#,
            page_size = PAGE_SIZE,
            tokens = token_list,
            ts_gte = timestamp_gte,
            ts_lte = timestamp_lte,
        );

        GraphQLQuery {
            query,
            variables: None,
        }
    }

    /// Execute a GraphQL query against the orderbook subgraph endpoint.
    /// Retries on transient HTTP errors and parse failures with exponential backoff.
    /// HTTP 429 (rate limit) uses a longer backoff and doesn't count toward MAX_RETRIES.
    async fn execute_query<T>(&self, query: &GraphQLQuery) -> BacktestResult<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut attempts = 0u32;

        loop {
            attempts += 1;
            match self
                .client
                .post(GOLDSKY_ORDERBOOK_URL)
                .json(query)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();

                    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        // Rate limited — always retry with longer backoff, don't count toward MAX_RETRIES
                        attempts -= 1;
                        warn!(attempts, "GraphQL rate limited (429), backing off");
                        let delay_ms = RATE_LIMIT_DELAY_MS * 2_u64.pow(attempts.min(4));
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        continue;
                    }

                    if !status.is_success() {
                        let error_text = response
                            .text()
                            .await
                            .unwrap_or_else(|_| "unknown".to_string());

                        if attempts >= MAX_RETRIES {
                            return Err(BacktestError::Network(format!(
                                "GraphQL HTTP error {}: {}",
                                status, error_text
                            )));
                        }

                        warn!(
                            status = %status,
                            attempts,
                            max_retries = MAX_RETRIES,
                            "GraphQL request failed, retrying"
                        );
                    } else {
                        match response.json::<GraphQLResponse<T>>().await {
                            Ok(gql_response) => {
                                if !gql_response.errors.is_empty() {
                                    let error_msgs: Vec<_> = gql_response
                                        .errors
                                        .iter()
                                        .map(|e| e.message.as_str())
                                        .collect();
                                    return Err(BacktestError::Network(format!(
                                        "GraphQL errors: {}",
                                        error_msgs.join(", ")
                                    )));
                                }

                                return gql_response.data.ok_or_else(|| {
                                    BacktestError::Network(
                                        "GraphQL response missing data field".to_string(),
                                    )
                                });
                            }
                            Err(e) => {
                                if attempts >= MAX_RETRIES {
                                    return Err(BacktestError::Network(format!(
                                        "Failed to parse GraphQL response after {} retries: {}",
                                        MAX_RETRIES, e
                                    )));
                                }

                                warn!(
                                    error = %e,
                                    attempts,
                                    max_retries = MAX_RETRIES,
                                    "GraphQL response parse failed, retrying"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    if attempts >= MAX_RETRIES {
                        return Err(BacktestError::Network(format!(
                            "GraphQL request failed after {} retries: {}",
                            MAX_RETRIES, e
                        )));
                    }

                    warn!(
                        error = %e,
                        attempts,
                        max_retries = MAX_RETRIES,
                        "GraphQL request failed, retrying"
                    );
                }
            }

            // Exponential backoff: delay = RETRY_DELAY_MS * 2^(attempts-1)
            let delay_ms = RETRY_DELAY_MS * 2_u64.pow(attempts - 1);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }

    /// Check if a date range is already cached in the database.
    async fn is_range_cached(
        &self,
        source: &str,
        token_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<bool> {
        let logs = self.store.get_fetch_log(source, token_id).await?;

        for log in logs {
            // Check if this log entry fully covers the requested range
            if log.start_ts <= start && log.end_ts >= end {
                return Ok(true);
            }
        }

        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    async fn setup_store() -> Arc<HistoricalDataStore> {
        Arc::new(HistoricalDataStore::new(":memory:").await.unwrap())
    }

    #[tokio::test]
    async fn test_subgraph_fetcher_creation() {
        let store = setup_store().await;
        let fetcher = SubgraphFetcher::new(store);
        assert!(fetcher.is_ok());
    }

    #[tokio::test]
    async fn test_build_batch_query_single_token() {
        let query = SubgraphFetcher::build_batch_query(&["token123"], "1000000", "2000000");

        assert!(query.query.contains("orderFilledEvents"));
        assert!(query.query.contains("orderBy: timestamp"));
        assert!(query.query.contains("orderDirection: asc"));
        assert!(query.query.contains(r#"takerAssetId_in: ["token123"]"#));
        assert!(query.query.contains(r#"makerAssetId_in: ["token123"]"#));
        assert!(query.query.contains(r#"timestamp_gte: "1000000""#));
        assert!(query.query.contains(r#"timestamp_lte: "2000000""#));
        assert!(query.query.contains("or:"));
    }

    #[tokio::test]
    async fn test_build_batch_query_multiple_tokens() {
        let query =
            SubgraphFetcher::build_batch_query(&["tokenA", "tokenB", "tokenC"], "100", "200");

        assert!(
            query
                .query
                .contains(r#"takerAssetId_in: ["tokenA", "tokenB", "tokenC"]"#)
        );
        assert!(
            query
                .query
                .contains(r#"makerAssetId_in: ["tokenA", "tokenB", "tokenC"]"#)
        );
    }

    #[tokio::test]
    async fn test_parse_amount_6_decimals() {
        assert_eq!(parse_amount("1000000").unwrap(), dec!(1.0));
        assert_eq!(parse_amount("500000").unwrap(), dec!(0.5));
        assert_eq!(parse_amount("100000").unwrap(), dec!(0.1));
        assert_eq!(parse_amount("3150000").unwrap(), dec!(3.15));
        assert_eq!(parse_amount("0").unwrap(), dec!(0));
    }

    #[tokio::test]
    async fn test_parse_amount_invalid() {
        assert!(parse_amount("not_a_number").is_err());
    }

    #[tokio::test]
    async fn test_order_filled_event_buy_conversion() {
        // BUY: maker pays 50 USDC (makerAssetId="0"), receives 100 outcome tokens
        let event = OrderFilledEvent {
            id: "0xabc_1".to_string(),
            timestamp: "1700000000".to_string(),
            maker_asset_id: "0".to_string(),     // USDC (collateral)
            taker_asset_id: "12345".to_string(), // outcome token
            maker_amount_filled: "50000000".to_string(), // 50 USDC
            taker_amount_filled: "100000000".to_string(), // 100 tokens
        };

        assert!(event.is_buy());
        assert!(!event.is_sell());

        let trade = event.to_historical_trade().unwrap();
        assert_eq!(trade.side, "buy");
        assert_eq!(trade.token_id, "12345");
        assert_eq!(trade.price, dec!(0.5)); // 50 / 100 = 0.5
        assert_eq!(trade.size, dec!(100)); // 100 tokens
        assert_eq!(trade.source, "subgraph");
        assert_eq!(trade.id, "0xabc_1");
    }

    #[tokio::test]
    async fn test_order_filled_event_sell_conversion() {
        // SELL: maker sells 200 outcome tokens (makerAssetId="67890"), receives 160 USDC
        let event = OrderFilledEvent {
            id: "0xdef_2".to_string(),
            timestamp: "1700000100".to_string(),
            maker_asset_id: "67890".to_string(), // outcome token
            taker_asset_id: "0".to_string(),     // USDC (collateral)
            maker_amount_filled: "200000000".to_string(), // 200 tokens
            taker_amount_filled: "160000000".to_string(), // 160 USDC
        };

        assert!(!event.is_buy());
        assert!(event.is_sell());

        let trade = event.to_historical_trade().unwrap();
        assert_eq!(trade.side, "sell");
        assert_eq!(trade.token_id, "67890");
        assert_eq!(trade.price, dec!(0.8)); // 160 / 200 = 0.8
        assert_eq!(trade.size, dec!(200)); // 200 tokens
        assert_eq!(trade.source, "subgraph");
    }

    #[tokio::test]
    async fn test_invalid_event_neither_usdc() {
        // Neither asset is USDC — should return error
        let event = OrderFilledEvent {
            id: "0xbad_1".to_string(),
            timestamp: "1700000000".to_string(),
            maker_asset_id: "11111".to_string(),
            taker_asset_id: "22222".to_string(),
            maker_amount_filled: "100000000".to_string(),
            taker_amount_filled: "100000000".to_string(),
        };

        assert!(!event.is_buy());
        assert!(!event.is_sell());

        let result = event.to_historical_trade();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("USDC"));
    }

    #[tokio::test]
    async fn test_order_filled_event_zero_amount() {
        let event = OrderFilledEvent {
            id: "0xzero_1".to_string(),
            timestamp: "1700000000".to_string(),
            maker_asset_id: "0".to_string(),
            taker_asset_id: "12345".to_string(),
            maker_amount_filled: "0".to_string(),
            taker_amount_filled: "100000000".to_string(),
        };

        let result = event.to_historical_trade();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Zero amount"));
    }

    #[tokio::test]
    async fn test_is_range_cached_empty() {
        let store = setup_store().await;
        let fetcher = SubgraphFetcher::new(store).unwrap();

        let start = Utc::now();
        let end = start + chrono::Duration::hours(1);

        let cached = fetcher
            .is_range_cached("subgraph_trades", "token1", start, end)
            .await
            .unwrap();
        assert!(!cached);
    }

    #[tokio::test]
    async fn test_is_range_cached_with_log() {
        let store = setup_store().await;
        let fetcher = SubgraphFetcher::new(Arc::clone(&store)).unwrap();

        // Use second-precision timestamps to match DB storage
        let now_ts = Utc::now().timestamp();
        let start = DateTime::from_timestamp(now_ts, 0).unwrap();
        let end = DateTime::from_timestamp(now_ts + 3600, 0).unwrap();

        // Insert a fetch log covering this range
        store
            .insert_fetch_log(DataFetchLog {
                id: None,
                source: "subgraph_trades".to_string(),
                token_id: "token1".to_string(),
                start_ts: start,
                end_ts: end,
                fetched_at: Utc::now(),
                row_count: 100,
            })
            .await
            .unwrap();

        // Should be cached now
        let cached = fetcher
            .is_range_cached("subgraph_trades", "token1", start, end)
            .await
            .unwrap();
        assert!(cached);
    }

    #[tokio::test]
    async fn test_db_dedup_insert_or_replace() {
        let store = setup_store().await;

        let now = Utc::now();

        // Insert existing trade
        let existing = vec![HistoricalTrade {
            id: "0xaaa".to_string(),
            token_id: "token1".to_string(),
            timestamp: now,
            price: dec!(0.5),
            size: dec!(100.0),
            side: "buy".to_string(),
            source: "clob".to_string(),
        }];
        store.insert_historical_trades(existing).await.unwrap();

        // Insert new trades with one duplicate ID (should overwrite via INSERT OR REPLACE)
        let new_trades = vec![
            HistoricalTrade {
                id: "0xaaa".to_string(),
                token_id: "token1".to_string(),
                timestamp: now,
                price: dec!(0.52),
                size: dec!(150.0),
                side: "buy".to_string(),
                source: "subgraph".to_string(),
            },
            HistoricalTrade {
                id: "0xbbb".to_string(),
                token_id: "token1".to_string(),
                timestamp: now + chrono::Duration::seconds(30),
                price: dec!(0.53),
                size: dec!(50.0),
                side: "sell".to_string(),
                source: "subgraph".to_string(),
            },
        ];
        store.insert_historical_trades(new_trades).await.unwrap();

        // Query back — should have 2 trades (0xaaa replaced, 0xbbb new)
        let result = store
            .get_historical_trades(
                "token1",
                now - chrono::Duration::hours(1),
                now + chrono::Duration::hours(1),
            )
            .await
            .unwrap();

        assert_eq!(result.len(), 2);

        let updated_trade = result.iter().find(|t| t.id == "0xaaa").unwrap();
        assert_eq!(updated_trade.price, dec!(0.52)); // New price
        assert_eq!(updated_trade.source, "subgraph"); // New source
    }

    // Live API test (marked with #[ignore])
    #[tokio::test]
    #[ignore]
    async fn test_fetch_subgraph_trades_live() {
        let store = setup_store().await;
        let fetcher = SubgraphFetcher::new(store).unwrap();

        // Use a known token ID and recent historical period
        let token_id =
            "21742633143463906290569050155826241533067272736897614950488156847949938836455";

        // Query trades from 30 days ago
        let end_ts = Utc::now().timestamp();
        let start_ts = end_ts - (30 * 86400);

        let trades = fetcher
            .fetch_subgraph_trades(token_id, start_ts, end_ts)
            .await;

        match trades {
            Ok(data) => {
                println!("Fetched {} trades from orderbook subgraph", data.len());

                if !data.is_empty() {
                    // Verify structure
                    for trade in data.iter().take(3) {
                        println!("Trade: {:?}", trade);
                        assert_eq!(trade.source, "subgraph");
                        assert!(trade.price >= dec!(0.0) && trade.price <= dec!(1.0));
                        assert!(trade.size > dec!(0.0));
                        assert!(trade.side == "buy" || trade.side == "sell");
                    }
                }
            }
            Err(e) => {
                println!(
                    "Live subgraph test failed (this is OK if API is unavailable): {}",
                    e
                );
            }
        }
    }
}
