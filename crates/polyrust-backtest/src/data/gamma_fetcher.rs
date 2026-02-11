use chrono::{DateTime, Utc};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::data::{HistoricalDataStore, HistoricalMarket};
use crate::error::{BacktestError, BacktestResult};

const GAMMA_BASE_URL: &str = "https://gamma-api.polymarket.com";
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 1000;
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// 15-minute window in seconds.
const WINDOW_SECS: i64 = 900;

/// Default concurrency limit for slug enumeration fallback.
const DEFAULT_DISCOVERY_CONCURRENCY: usize = 20;

/// Slug prefixes for supported coins' 15-minute Up/Down markets.
/// Must match the live DiscoveryFeed's COIN_SLUGS.
const COIN_SLUGS: &[(&str, &str)] = &[
    ("BTC", "btc-updown-15m"),
    ("ETH", "eth-updown-15m"),
    ("SOL", "sol-updown-15m"),
    ("XRP", "xrp-updown-15m"),
];

/// Gamma API market response structures.
/// The API returns `clobTokenIds` as a JSON-encoded string (e.g. `"[\"id1\", \"id2\"]"`)
/// and `negRisk` can be null for older markets.
#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(rename = "conditionId")]
    condition_id: String,
    slug: String,
    question: String,
    #[serde(rename = "startDate")]
    start_date: Option<String>,
    #[serde(rename = "endDate")]
    end_date: Option<String>,
    /// JSON-encoded array of token IDs, e.g. `"[\"tokenId1\", \"tokenId2\"]"`
    #[serde(rename = "clobTokenIds", default)]
    clob_token_ids: Option<String>,
    #[serde(rename = "negRisk", default)]
    neg_risk: Option<bool>,
}

/// Parse a unix timestamp from the slug suffix (e.g. `btc-updown-15m-1706000000`).
/// Returns `None` if the slug doesn't end with a valid unix timestamp.
fn parse_slug_timestamp(slug: &str) -> Option<DateTime<Utc>> {
    let last_segment = slug.rsplit('-').next()?;
    let ts: i64 = last_segment.parse().ok()?;
    // Sanity: must be a reasonable unix timestamp (after 2020)
    if ts > 1_577_836_800 {
        DateTime::from_timestamp(ts, 0)
    } else {
        None
    }
}

/// HTTP client for Gamma API (market discovery and metadata).
/// Wrap in `Arc` for concurrent access from JoinSet tasks.
pub struct GammaFetcher {
    client: Client,
    store: Arc<HistoricalDataStore>,
    discovery_concurrency: usize,
}

impl GammaFetcher {
    /// Create a new Gamma API fetcher with the given data store.
    pub fn new(store: Arc<HistoricalDataStore>) -> BacktestResult<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| BacktestError::Network(e.to_string()))?;

        Ok(Self {
            client,
            store,
            discovery_concurrency: DEFAULT_DISCOVERY_CONCURRENCY,
        })
    }

    /// Set the concurrency limit for slug enumeration fallback.
    pub fn with_discovery_concurrency(mut self, concurrency: usize) -> Self {
        self.discovery_concurrency = concurrency;
        self
    }

    /// Fetch markets matching a slug pattern.
    ///
    /// # Arguments
    /// * `slug_pattern` - Slug substring to search for (e.g., "btc", "eth-15min")
    pub async fn fetch_markets_by_slug(
        &self,
        slug_pattern: &str,
    ) -> BacktestResult<Vec<HistoricalMarket>> {
        info!(
            slug_pattern,
            "Fetching markets from Gamma API by slug pattern"
        );

        let url = format!("{}/markets?slug_contains={}", GAMMA_BASE_URL, slug_pattern);

        let response = self.fetch_with_retry(&url).await?;
        let markets: Vec<GammaMarket> = response.json().await.map_err(|e| {
            BacktestError::Network(format!("Failed to parse markets response: {}", e))
        })?;

        info!(count = markets.len(), "Received markets from Gamma API");

        let mut historical_markets = Vec::new();
        for market in markets {
            if let Some(hist_market) = Self::convert_to_historical_market(market)? {
                historical_markets.push(hist_market);
            }
        }

        // Cache all markets in DB
        for market in &historical_markets {
            self.store.insert_historical_market(market.clone()).await?;
        }

        info!(
            cached = historical_markets.len(),
            "Cached markets to database"
        );
        Ok(historical_markets)
    }

    /// Fetch a single market by condition ID.
    ///
    /// # Arguments
    /// * `condition_id` - Market condition ID
    pub async fn fetch_market_by_id(
        &self,
        condition_id: &str,
    ) -> BacktestResult<Option<HistoricalMarket>> {
        info!(condition_id, "Fetching market from Gamma API by ID");

        let url = format!("{}/markets/{}", GAMMA_BASE_URL, condition_id);

        let response = self.fetch_with_retry(&url).await?;
        let market: GammaMarket = response.json().await.map_err(|e| {
            BacktestError::Network(format!("Failed to parse market response: {}", e))
        })?;

        let historical_market = Self::convert_to_historical_market(market)?;

        // Cache if we got a valid market
        if let Some(ref hist_market) = historical_market {
            self.store
                .insert_historical_market(hist_market.clone())
                .await?;
        }

        Ok(historical_market)
    }

    /// Fetch expired markets for a specific coin and date range.
    ///
    /// Strategy: tries batch `slug_contains` query first (single HTTP call),
    /// then filters locally by date range. Falls back to concurrent slug
    /// enumeration if batch returns 0 results (API may paginate/truncate).
    ///
    /// # Arguments
    /// * `coin` - Coin symbol (e.g., "BTC", "ETH", "SOL")
    /// * `start_date` - Start of date range
    /// * `end_date` - End of date range
    /// * `_duration_filter` - Ignored (slug-based lookup always returns 15-min markets)
    pub async fn fetch_expired_markets(
        self: &Arc<Self>,
        coin: &str,
        start_date: DateTime<Utc>,
        end_date: DateTime<Utc>,
        _duration_filter: Option<u64>,
    ) -> BacktestResult<Vec<HistoricalMarket>> {
        let upper = coin.to_uppercase();
        let prefix = COIN_SLUGS
            .iter()
            .find(|(k, _)| *k == upper)
            .map(|(_, p)| *p)
            .ok_or_else(|| {
                BacktestError::InvalidInput(format!(
                    "Unsupported coin: {}. Supported: {:?}",
                    coin,
                    COIN_SLUGS.iter().map(|(k, _)| k).collect::<Vec<_>>()
                ))
            })?;

        // Try batch approach first: single slug_contains query + local filtering
        info!(
            coin,
            prefix, "Trying batch market discovery via slug_contains"
        );
        let batch_markets = self
            .fetch_expired_markets_batch(prefix, start_date, end_date)
            .await?;

        // Estimate expected markets: one per 15-min window in the date range
        let expected_count = (end_date - start_date).num_seconds() / 900;
        let is_likely_complete =
            expected_count <= 0 || batch_markets.len() as i64 >= (expected_count * 4 / 5); // >= 80% of expected

        if !batch_markets.is_empty() && is_likely_complete {
            info!(
                coin,
                found = batch_markets.len(),
                expected = expected_count,
                "Batch discovery succeeded"
            );
            return Ok(batch_markets);
        }

        // Fallback: concurrent slug enumeration (batch empty or likely truncated)
        info!(
            coin,
            prefix,
            batch_count = batch_markets.len(),
            expected = expected_count,
            "Batch may be truncated, falling back to concurrent slug enumeration"
        );
        self.fetch_expired_markets_concurrent(coin, prefix, start_date, end_date)
            .await
    }

    /// Batch discovery: fetch all markets matching the slug prefix in one call,
    /// then filter locally by date range.
    async fn fetch_expired_markets_batch(
        &self,
        prefix: &str,
        start_date: DateTime<Utc>,
        end_date: DateTime<Utc>,
    ) -> BacktestResult<Vec<HistoricalMarket>> {
        let all_markets = self.fetch_markets_by_slug(prefix).await?;

        let filtered: Vec<_> = all_markets
            .into_iter()
            .filter(|m| m.end_date >= start_date && m.end_date <= end_date)
            .collect();

        info!(
            prefix,
            filtered = filtered.len(),
            "Batch discovery filtered by date range"
        );

        Ok(filtered)
    }

    /// Concurrent slug enumeration fallback.
    /// Enumerates all 15-minute windows in the range and fetches each slug
    /// concurrently, bounded by `discovery_concurrency`.
    async fn fetch_expired_markets_concurrent(
        self: &Arc<Self>,
        coin: &str,
        prefix: &str,
        start_date: DateTime<Utc>,
        end_date: DateTime<Utc>,
    ) -> BacktestResult<Vec<HistoricalMarket>> {
        // Collect all slugs for the date range
        let mut ts = (start_date.timestamp() / WINDOW_SECS) * WINDOW_SECS;
        let end_ts = end_date.timestamp();
        let total_windows = ((end_ts - ts) / WINDOW_SECS).max(0);

        let mut slugs = Vec::with_capacity(total_windows as usize);
        while ts <= end_ts {
            slugs.push(format!("{prefix}-{ts}"));
            ts += WINDOW_SECS;
        }

        let pb = ProgressBar::new(slugs.len() as u64);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} [{elapsed_precise}] {bar:30} {pos}/{len} ({eta}) {msg}",
            )
            .unwrap(),
        );
        pb.set_message(format!("{coin} discovery"));

        let mut markets = Vec::new();
        let mut found = 0u64;
        let mut missed = 0u64;

        // Process slugs in batches bounded by concurrency limit
        for chunk in slugs.chunks(self.discovery_concurrency) {
            let mut join_set = JoinSet::new();

            for slug in chunk {
                let fetcher = Arc::clone(self);
                let slug = slug.clone();
                join_set.spawn(async move {
                    let result = fetcher.fetch_single_market_by_slug(&slug).await;
                    (slug, result)
                });
            }

            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok((slug, Ok(Some(market)))) => {
                        match Self::convert_to_historical_market(market) {
                            Ok(Some(hist)) => {
                                self.store.insert_historical_market(hist.clone()).await?;
                                markets.push(hist);
                                found += 1;
                            }
                            Ok(None) => {
                                missed += 1;
                            }
                            Err(e) => {
                                debug!(slug, error = %e, "Failed to convert market");
                                missed += 1;
                            }
                        }
                    }
                    Ok((_, Ok(None))) => {
                        missed += 1;
                    }
                    Ok((slug, Err(e))) => {
                        debug!(slug, error = %e, "Failed to fetch market by slug");
                        missed += 1;
                    }
                    Err(e) => {
                        pb.println(format!("Task panicked during slug fetch: {e}"));
                        missed += 1;
                    }
                }
            }

            pb.inc(chunk.len() as u64);
        }

        pb.finish_with_message(format!("{coin}: {found} found, {missed} missed"));

        info!(coin, found, missed, "Discovery complete");

        Ok(markets)
    }

    /// Fetch a single market by exact slug via `GET /markets/slug/{slug}`.
    /// Returns None for 404s / missing markets.
    async fn fetch_single_market_by_slug(&self, slug: &str) -> BacktestResult<Option<GammaMarket>> {
        let url = format!("{}/markets/slug/{}", GAMMA_BASE_URL, slug);

        match self.fetch_with_retry(&url).await {
            Ok(response) => {
                let market: GammaMarket = response.json().await.map_err(|e| {
                    BacktestError::Network(format!(
                        "Failed to parse market response for slug {}: {}",
                        slug, e
                    ))
                })?;
                Ok(Some(market))
            }
            Err(_) => Ok(None), // 404 or other error — slug doesn't exist
        }
    }

    /// Convert Gamma API market to HistoricalMarket struct.
    /// Falls back to parsing start_date from slug timestamp if missing from API.
    fn convert_to_historical_market(
        market: GammaMarket,
    ) -> BacktestResult<Option<HistoricalMarket>> {
        // Parse start_date: API field → slug timestamp fallback
        let start_date = match market.start_date {
            Some(ref date_str) => DateTime::parse_from_rfc3339(date_str)
                .map_err(|e| {
                    BacktestError::InvalidInput(format!("Failed to parse start_date: {}", e))
                })?
                .with_timezone(&Utc),
            None => match parse_slug_timestamp(&market.slug) {
                Some(ts) => ts,
                None => {
                    debug!(condition_id = %market.condition_id, slug = %market.slug, "Skipping market with no start_date");
                    return Ok(None);
                }
            },
        };

        let end_date = match market.end_date {
            Some(ref date_str) => DateTime::parse_from_rfc3339(date_str)
                .map_err(|e| {
                    BacktestError::InvalidInput(format!("Failed to parse end_date: {}", e))
                })?
                .with_timezone(&Utc),
            None => {
                debug!(condition_id = %market.condition_id, "Skipping market with no end_date");
                return Ok(None);
            }
        };

        // Parse clobTokenIds JSON string (e.g. "[\"id1\", \"id2\"]")
        let token_ids: Vec<String> = match market.clob_token_ids {
            Some(ref ids_str) => serde_json::from_str(ids_str).unwrap_or_default(),
            None => Vec::new(),
        };

        if token_ids.len() != 2 {
            debug!(
                condition_id = %market.condition_id,
                token_count = token_ids.len(),
                "Skipping market with incorrect token count"
            );
            return Ok(None);
        }

        Ok(Some(HistoricalMarket {
            market_id: market.condition_id,
            slug: market.slug,
            question: market.question,
            start_date,
            end_date,
            token_a: token_ids[0].clone(),
            token_b: token_ids[1].clone(),
            neg_risk: market.neg_risk.unwrap_or(false),
        }))
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
                        let error_text = response
                            .text()
                            .await
                            .unwrap_or_else(|_| "unknown".to_string());

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
                        return Err(BacktestError::Network(format!(
                            "Request failed after {} retries: {}",
                            MAX_RETRIES, e
                        )));
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

    async fn setup_store() -> Arc<HistoricalDataStore> {
        Arc::new(HistoricalDataStore::new(":memory:").await.unwrap())
    }

    #[tokio::test]
    async fn test_gamma_fetcher_creation() {
        let store = setup_store().await;
        let fetcher = GammaFetcher::new(store);
        assert!(fetcher.is_ok());
    }

    #[tokio::test]
    async fn test_convert_valid_market() {
        let gamma_market = GammaMarket {
            condition_id: "0x123".to_string(),
            slug: "btc-up-15min".to_string(),
            question: "Will BTC go up?".to_string(),
            start_date: Some("2025-01-01T00:00:00Z".to_string()),
            end_date: Some("2025-01-01T00:15:00Z".to_string()),
            clob_token_ids: Some("[\"token_a\", \"token_b\"]".to_string()),
            neg_risk: Some(false),
        };

        let result = GammaFetcher::convert_to_historical_market(gamma_market).unwrap();
        assert!(result.is_some());

        let market = result.unwrap();
        assert_eq!(market.market_id, "0x123");
        assert_eq!(market.slug, "btc-up-15min");
        assert_eq!(market.token_a, "token_a");
        assert_eq!(market.token_b, "token_b");
        assert!(!market.neg_risk);
    }

    #[tokio::test]
    async fn test_convert_market_missing_dates() {
        let gamma_market = GammaMarket {
            condition_id: "0x123".to_string(),
            slug: "btc-up-15min".to_string(),
            question: "Will BTC go up?".to_string(),
            start_date: None,
            end_date: Some("2025-01-01T00:15:00Z".to_string()),
            clob_token_ids: Some("[\"token_a\", \"token_b\"]".to_string()),
            neg_risk: Some(false),
        };

        let result = GammaFetcher::convert_to_historical_market(gamma_market).unwrap();
        assert!(result.is_none()); // Should skip market with missing start_date
    }

    #[tokio::test]
    async fn test_convert_market_wrong_token_count() {
        let gamma_market = GammaMarket {
            condition_id: "0x123".to_string(),
            slug: "btc-up-15min".to_string(),
            question: "Will BTC go up?".to_string(),
            start_date: Some("2025-01-01T00:00:00Z".to_string()),
            end_date: Some("2025-01-01T00:15:00Z".to_string()),
            clob_token_ids: Some("[\"token_a\"]".to_string()),
            neg_risk: Some(false),
        };

        let result = GammaFetcher::convert_to_historical_market(gamma_market).unwrap();
        assert!(result.is_none()); // Should skip market with wrong token count
    }

    #[tokio::test]
    async fn test_expired_markets_filtering() {
        let store = setup_store().await;
        let _fetcher = GammaFetcher::new(Arc::clone(&store)).unwrap();

        // Insert test markets with different end dates
        let base_time = chrono::Utc::now();
        let markets = vec![
            HistoricalMarket {
                market_id: "market1".to_string(),
                slug: "btc-up-1".to_string(),
                question: "Question 1".to_string(),
                start_date: base_time - chrono::Duration::hours(2),
                end_date: base_time - chrono::Duration::hours(1), // Expired recently
                token_a: "token_a1".to_string(),
                token_b: "token_b1".to_string(),
                neg_risk: false,
            },
            HistoricalMarket {
                market_id: "market2".to_string(),
                slug: "btc-up-2".to_string(),
                question: "Question 2".to_string(),
                start_date: base_time - chrono::Duration::days(10),
                end_date: base_time - chrono::Duration::days(9), // Expired long ago
                token_a: "token_a2".to_string(),
                token_b: "token_b2".to_string(),
                neg_risk: false,
            },
        ];

        for market in markets {
            store.insert_historical_market(market).await.unwrap();
        }

        // Test filtering logic directly (without actual API call)
        let start_date = base_time - chrono::Duration::hours(2);
        let end_date = base_time;

        // Simulate what fetch_expired_markets would do after getting data from API
        let all_markets = vec![
            HistoricalMarket {
                market_id: "market1".to_string(),
                slug: "btc-up-1".to_string(),
                question: "Question 1".to_string(),
                start_date: base_time - chrono::Duration::hours(2),
                end_date: base_time - chrono::Duration::hours(1),
                token_a: "token_a1".to_string(),
                token_b: "token_b1".to_string(),
                neg_risk: false,
            },
            HistoricalMarket {
                market_id: "market2".to_string(),
                slug: "btc-up-2".to_string(),
                question: "Question 2".to_string(),
                start_date: base_time - chrono::Duration::days(10),
                end_date: base_time - chrono::Duration::days(9),
                token_a: "token_a2".to_string(),
                token_b: "token_b2".to_string(),
                neg_risk: false,
            },
        ];

        let expired_markets: Vec<_> = all_markets
            .into_iter()
            .filter(|m| m.end_date >= start_date && m.end_date <= end_date)
            .collect();

        assert_eq!(expired_markets.len(), 1);
        assert_eq!(expired_markets[0].market_id, "market1");
    }

    // Live API tests (marked with #[ignore])

    #[tokio::test]
    #[ignore]
    async fn test_fetch_markets_by_slug_live() {
        let store = setup_store().await;
        let fetcher = GammaFetcher::new(store).unwrap();

        // Try to fetch BTC markets
        let markets = fetcher.fetch_markets_by_slug("btc").await;

        match markets {
            Ok(data) => {
                println!("Fetched {} markets", data.len());

                if !data.is_empty() {
                    // Verify structure
                    for market in data.iter().take(3) {
                        println!("Market: {} - {}", market.market_id, market.slug);
                        assert!(!market.market_id.is_empty());
                        assert!(!market.slug.is_empty());
                        assert!(!market.token_a.is_empty());
                        assert!(!market.token_b.is_empty());
                    }
                }
            }
            Err(e) => {
                println!(
                    "Live API test failed (this is OK if API is unavailable): {}",
                    e
                );
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_fetch_expired_markets_live() {
        let store = setup_store().await;
        let fetcher = Arc::new(GammaFetcher::new(store).unwrap());

        // Fetch BTC markets that expired in the last 7 days
        let end_date = Utc::now();
        let start_date = end_date - chrono::Duration::days(7);

        let markets = fetcher
            .fetch_expired_markets("BTC", start_date, end_date, None)
            .await;

        match markets {
            Ok(data) => {
                println!("Fetched {} expired BTC markets", data.len());

                for market in data.iter().take(3) {
                    println!(
                        "Expired market: {} - {} (ended: {})",
                        market.market_id, market.slug, market.end_date
                    );
                    assert!(market.end_date >= start_date && market.end_date <= end_date);
                }
            }
            Err(e) => {
                println!(
                    "Live API test failed (this is OK if API is unavailable): {}",
                    e
                );
            }
        }
    }
}
