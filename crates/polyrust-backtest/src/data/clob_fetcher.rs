use chrono::{DateTime, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::data::{DataFetchLog, HistoricalDataStore, HistoricalPrice, HistoricalTrade};
use crate::error::{BacktestError, BacktestResult};

const CLOB_BASE_URL: &str = "https://clob.polymarket.com";
const DATA_API_BASE_URL: &str = "https://data-api.polymarket.com";
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 1000;
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// CLOB API price history response
#[derive(Debug, Deserialize)]
struct PriceHistoryResponse {
    history: Vec<PricePoint>,
}

#[derive(Debug, Deserialize)]
struct PricePoint {
    t: i64,     // timestamp (seconds)
    p: String,  // price (decimal string)
}

/// Data API trade event
#[derive(Debug, Deserialize)]
struct TradeEvent {
    #[serde(rename = "id")]
    trade_id: String,
    #[serde(rename = "asset_id")]
    token_id: String,
    #[serde(rename = "timestamp")]
    timestamp: i64, // Unix timestamp
    price: String,
    size: String,
    side: String,
}

/// HTTP client for CLOB REST API and Data API.
pub struct ClobFetcher {
    client: Client,
    store: Arc<HistoricalDataStore>,
}

impl ClobFetcher {
    /// Create a new CLOB fetcher with the given data store.
    pub fn new(store: Arc<HistoricalDataStore>) -> BacktestResult<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| BacktestError::Network(e.to_string()))?;

        Ok(Self { client, store })
    }

    /// Fetch price history for a token within a date range.
    /// Returns cached data if available; otherwise fetches from API.
    ///
    /// # Arguments
    /// * `token_id` - Token ID (market condition ID)
    /// * `start_ts` - Start timestamp (Unix seconds)
    /// * `end_ts` - End timestamp (Unix seconds)
    /// * `fidelity_mins` - Price resolution in minutes (1, 5, 15, 60, etc.)
    pub async fn fetch_price_history(
        &self,
        token_id: &str,
        start_ts: i64,
        end_ts: i64,
        fidelity_mins: u64,
    ) -> BacktestResult<Vec<HistoricalPrice>> {
        // Check if data is already cached
        let start_dt = DateTime::from_timestamp(start_ts, 0)
            .ok_or_else(|| BacktestError::InvalidInput("Invalid start_ts".to_string()))?;
        let end_dt = DateTime::from_timestamp(end_ts, 0)
            .ok_or_else(|| BacktestError::InvalidInput("Invalid end_ts".to_string()))?;

        if self.is_range_cached("clob_prices", token_id, start_dt, end_dt).await? {
            debug!(token_id, start_ts, end_ts, "Price data already cached, skipping fetch");
            return self.store.get_historical_prices(token_id, start_dt, end_dt).await;
        }

        info!(token_id, start_ts, end_ts, fidelity_mins, "Fetching price history from CLOB API");

        let url = format!(
            "{}/prices-history?market={}&startTs={}&endTs={}&fidelity={}",
            CLOB_BASE_URL, token_id, start_ts, end_ts, fidelity_mins
        );

        let response = self.fetch_with_retry(&url).await?;
        let price_data: PriceHistoryResponse = response
            .json()
            .await
            .map_err(|e| BacktestError::Network(format!("Failed to parse price history response: {}", e)))?;

        // Convert to HistoricalPrice structs
        let mut prices = Vec::new();
        for point in price_data.history {
            let timestamp = DateTime::from_timestamp(point.t, 0)
                .ok_or_else(|| BacktestError::InvalidInput(format!("Invalid timestamp: {}", point.t)))?;
            let price = point.p.parse::<Decimal>()
                .map_err(|e| BacktestError::InvalidInput(format!("Failed to parse price '{}': {}", point.p, e)))?;

            prices.push(HistoricalPrice {
                token_id: token_id.to_string(),
                timestamp,
                price,
                source: "clob".to_string(),
            });
        }

        let row_count = prices.len();

        // Cache the results
        if row_count > 0 {
            self.store.insert_historical_prices(prices.clone()).await?;
            self.store.insert_fetch_log(DataFetchLog {
                id: None,
                source: "clob_prices".to_string(),
                token_id: token_id.to_string(),
                start_ts: start_dt,
                end_ts: end_dt,
                fetched_at: Utc::now(),
                row_count: row_count as i64,
            }).await?;
        }

        info!(token_id, row_count, "Fetched and cached price history");
        Ok(prices)
    }

    /// Fetch trades for a market with pagination support.
    /// Returns cached data if available; otherwise fetches from API.
    ///
    /// # Arguments
    /// * `market_id` - Market condition ID
    /// * `start_ts` - Optional start timestamp for filtering
    /// * `end_ts` - Optional end timestamp for filtering
    /// * `limit` - Max results per request (default 1000, max 10000)
    pub async fn fetch_trades(
        &self,
        market_id: &str,
        start_ts: Option<i64>,
        end_ts: Option<i64>,
        limit: Option<u32>,
    ) -> BacktestResult<Vec<HistoricalTrade>> {
        // Check cache if we have date bounds
        if let (Some(start), Some(end)) = (start_ts, end_ts) {
            let start_dt = DateTime::from_timestamp(start, 0)
                .ok_or_else(|| BacktestError::InvalidInput("Invalid start_ts".to_string()))?;
            let end_dt = DateTime::from_timestamp(end, 0)
                .ok_or_else(|| BacktestError::InvalidInput("Invalid end_ts".to_string()))?;

            if self.is_range_cached("clob_trades", market_id, start_dt, end_dt).await? {
                debug!(market_id, start_ts, end_ts, "Trade data already cached, skipping fetch");
                return self.store.get_historical_trades(market_id, start_dt, end_dt).await;
            }
        }

        let limit = limit.unwrap_or(1000).min(10000);
        info!(market_id, limit, "Fetching trades from Data API");

        let mut all_trades = Vec::new();
        let mut offset = 0;

        loop {
            let mut url = format!(
                "{}/trades?market={}&limit={}&offset={}",
                DATA_API_BASE_URL, market_id, limit, offset
            );

            if let Some(start) = start_ts {
                url.push_str(&format!("&start={}", start));
            }
            if let Some(end) = end_ts {
                url.push_str(&format!("&end={}", end));
            }

            let response = self.fetch_with_retry(&url).await?;
            let trades: Vec<TradeEvent> = response
                .json()
                .await
                .map_err(|e| BacktestError::Network(format!("Failed to parse trades response: {}", e)))?;

            if trades.is_empty() {
                break;
            }

            // Convert to HistoricalTrade structs
            for trade in trades.iter() {
                let timestamp = DateTime::from_timestamp(trade.timestamp, 0)
                    .ok_or_else(|| BacktestError::InvalidInput(format!("Invalid timestamp: {}", trade.timestamp)))?;
                let price = trade.price.parse::<Decimal>()
                    .map_err(|e| BacktestError::InvalidInput(format!("Failed to parse price '{}': {}", trade.price, e)))?;
                let size = trade.size.parse::<Decimal>()
                    .map_err(|e| BacktestError::InvalidInput(format!("Failed to parse size '{}': {}", trade.size, e)))?;

                all_trades.push(HistoricalTrade {
                    id: trade.trade_id.clone(),
                    token_id: trade.token_id.clone(),
                    timestamp,
                    price,
                    size,
                    side: trade.side.clone(),
                    source: "clob".to_string(),
                });
            }

            // Check if we got a full page (more data may exist)
            if trades.len() < limit as usize {
                break;
            }

            offset += limit;
        }

        let row_count = all_trades.len();

        // Cache the results if we have date bounds
        if row_count > 0 && let (Some(start), Some(end)) = (start_ts, end_ts) {
            let start_dt = DateTime::from_timestamp(start, 0)
                .ok_or_else(|| BacktestError::InvalidInput("Invalid start_ts".to_string()))?;
            let end_dt = DateTime::from_timestamp(end, 0)
                .ok_or_else(|| BacktestError::InvalidInput("Invalid end_ts".to_string()))?;

            self.store.insert_historical_trades(all_trades.clone()).await?;
            self.store.insert_fetch_log(DataFetchLog {
                id: None,
                source: "clob_trades".to_string(),
                token_id: market_id.to_string(),
                start_ts: start_dt,
                end_ts: end_dt,
                fetched_at: Utc::now(),
                row_count: row_count as i64,
            }).await?;
        }

        info!(market_id, row_count, "Fetched and cached trades");
        Ok(all_trades)
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

    /// Fetch URL with exponential backoff retry logic.
    async fn fetch_with_retry(&self, url: &str) -> BacktestResult<reqwest::Response> {
        let mut attempts = 0;

        loop {
            attempts += 1;
            match self.client.get(url).send().await {
                Ok(response) => {
                    if response.status().is_success() {
                        return Ok(response);
                    } else {
                        let status = response.status();
                        let error_text = response.text().await.unwrap_or_else(|_| "unknown".to_string());

                        if attempts >= MAX_RETRIES {
                            return Err(BacktestError::Network(format!(
                                "HTTP error {}: {}",
                                status, error_text
                            )));
                        }

                        warn!(
                            url,
                            status = %status,
                            attempts,
                            max_retries = MAX_RETRIES,
                            "HTTP request failed, retrying"
                        );
                    }
                }
                Err(e) => {
                    if attempts >= MAX_RETRIES {
                        return Err(BacktestError::Network(format!("Request failed after {} retries: {}", MAX_RETRIES, e)));
                    }

                    warn!(
                        url,
                        error = %e,
                        attempts,
                        max_retries = MAX_RETRIES,
                        "Request failed, retrying"
                    );
                }
            }

            // Exponential backoff: delay = RETRY_DELAY_MS * 2^(attempts-1)
            let delay_ms = RETRY_DELAY_MS * 2_u64.pow(attempts - 1);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
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
    async fn test_clob_fetcher_creation() {
        let store = setup_store().await;
        let fetcher = ClobFetcher::new(store);
        assert!(fetcher.is_ok());
    }

    #[tokio::test]
    async fn test_is_range_cached_empty() {
        let store = setup_store().await;
        let fetcher = ClobFetcher::new(store).unwrap();

        let start = Utc::now();
        let end = start + chrono::Duration::hours(1);

        let cached = fetcher.is_range_cached("clob_prices", "token1", start, end).await.unwrap();
        assert!(!cached);
    }

    #[tokio::test]
    async fn test_is_range_cached_with_log() {
        let store = setup_store().await;
        let fetcher = ClobFetcher::new(Arc::clone(&store)).unwrap();

        // Use second-precision timestamps to match DB storage
        let now_ts = Utc::now().timestamp();
        let start = DateTime::from_timestamp(now_ts, 0).unwrap();
        let end = DateTime::from_timestamp(now_ts + 3600, 0).unwrap();

        // Insert a fetch log covering this range
        store.insert_fetch_log(DataFetchLog {
            id: None,
            source: "clob_prices".to_string(),
            token_id: "token1".to_string(),
            start_ts: start,
            end_ts: end,
            fetched_at: Utc::now(),
            row_count: 100,
        }).await.unwrap();

        // Should be cached now
        let cached = fetcher.is_range_cached("clob_prices", "token1", start, end).await.unwrap();
        assert!(cached);
    }

    #[tokio::test]
    async fn test_is_range_cached_partial_overlap() {
        let store = setup_store().await;
        let fetcher = ClobFetcher::new(Arc::clone(&store)).unwrap();

        // Use second-precision timestamps
        let base_ts = Utc::now().timestamp();
        let base = DateTime::from_timestamp(base_ts, 0).unwrap();
        let log_start = base;
        let log_end = DateTime::from_timestamp(base_ts + 3600, 0).unwrap();

        // Insert a fetch log
        store.insert_fetch_log(DataFetchLog {
            id: None,
            source: "clob_prices".to_string(),
            token_id: "token1".to_string(),
            start_ts: log_start,
            end_ts: log_end,
            fetched_at: Utc::now(),
            row_count: 100,
        }).await.unwrap();

        // Query a range that extends beyond the cached range
        let query_start = DateTime::from_timestamp(base_ts - 1800, 0).unwrap();
        let query_end = DateTime::from_timestamp(base_ts + 7200, 0).unwrap();
        let cached = fetcher.is_range_cached("clob_prices", "token1", query_start, query_end).await.unwrap();
        assert!(!cached); // Not fully cached
    }

    #[tokio::test]
    async fn test_is_range_cached_subset() {
        let store = setup_store().await;
        let fetcher = ClobFetcher::new(Arc::clone(&store)).unwrap();

        // Use second-precision timestamps
        let base_ts = Utc::now().timestamp();
        let base = DateTime::from_timestamp(base_ts, 0).unwrap();
        let log_start = base;
        let log_end = DateTime::from_timestamp(base_ts + 7200, 0).unwrap();

        // Insert a fetch log
        store.insert_fetch_log(DataFetchLog {
            id: None,
            source: "clob_prices".to_string(),
            token_id: "token1".to_string(),
            start_ts: log_start,
            end_ts: log_end,
            fetched_at: Utc::now(),
            row_count: 200,
        }).await.unwrap();

        // Query a smaller range within the cached range
        let query_start = DateTime::from_timestamp(base_ts + 1800, 0).unwrap();
        let query_end = DateTime::from_timestamp(base_ts + 3600, 0).unwrap();
        let cached = fetcher.is_range_cached("clob_prices", "token1", query_start, query_end).await.unwrap();
        assert!(cached); // Fully cached
    }

    // Live API tests (marked with #[ignore])

    #[tokio::test]
    #[ignore]
    async fn test_fetch_price_history_live() {
        let store = setup_store().await;
        let fetcher = ClobFetcher::new(store).unwrap();

        // Use a known BTC market token ID and recent timestamp
        let token_id = "21742633143463906290569050155826241533067272736897614950488156847949938836455";
        let end_ts = Utc::now().timestamp();
        let start_ts = end_ts - 3600; // 1 hour ago

        let prices = fetcher.fetch_price_history(token_id, start_ts, end_ts, 1).await;

        match prices {
            Ok(data) => {
                println!("Fetched {} price points", data.len());
                assert!(!data.is_empty());

                // Verify structure
                for price in data.iter().take(3) {
                    println!("Price: {:?}", price);
                    assert_eq!(price.token_id, token_id);
                    assert_eq!(price.source, "clob");
                    assert!(price.price >= dec!(0.0) && price.price <= dec!(1.0));
                }
            }
            Err(e) => {
                println!("Live API test failed (this is OK if API is unavailable): {}", e);
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_fetch_trades_live() {
        let store = setup_store().await;
        let fetcher = ClobFetcher::new(store).unwrap();

        // Use a known market condition ID
        let market_id = "21742633143463906290569050155826241533067272736897614950488156847949938836455";
        let end_ts = Utc::now().timestamp();
        let start_ts = end_ts - 86400; // 24 hours ago

        let trades = fetcher.fetch_trades(market_id, Some(start_ts), Some(end_ts), Some(100)).await;

        match trades {
            Ok(data) => {
                println!("Fetched {} trades", data.len());

                if !data.is_empty() {
                    // Verify structure
                    for trade in data.iter().take(3) {
                        println!("Trade: {:?}", trade);
                        assert_eq!(trade.source, "clob");
                        assert!(trade.price >= dec!(0.0) && trade.price <= dec!(1.0));
                        assert!(trade.size > dec!(0.0));
                    }
                }
            }
            Err(e) => {
                println!("Live API test failed (this is OK if API is unavailable): {}", e);
            }
        }
    }
}
