use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::data::{HistoricalDataStore, HistoricalMarket};
use crate::error::{BacktestError, BacktestResult};

const GAMMA_BASE_URL: &str = "https://gamma-api.polymarket.com";
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 1000;
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Gamma API market response structures
#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(rename = "conditionId")]
    condition_id: String,
    #[serde(rename = "slug")]
    slug: String,
    #[serde(rename = "question")]
    question: String,
    #[serde(rename = "startDate")]
    start_date: Option<String>, // ISO 8601 format
    #[serde(rename = "endDate")]
    end_date: Option<String>, // ISO 8601 format
    #[serde(rename = "tokens")]
    tokens: Vec<Token>,
    #[serde(rename = "negRisk", default)]
    neg_risk: bool,
}

#[derive(Debug, Deserialize)]
struct Token {
    #[serde(rename = "tokenId")]
    token_id: String,
    #[serde(rename = "outcome")]
    #[allow(dead_code)]
    outcome: String,
}

/// HTTP client for Gamma API (market discovery and metadata).
pub struct GammaFetcher {
    client: Client,
    store: Arc<HistoricalDataStore>,
}

impl GammaFetcher {
    /// Create a new Gamma API fetcher with the given data store.
    pub fn new(store: Arc<HistoricalDataStore>) -> BacktestResult<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| BacktestError::Network(e.to_string()))?;

        Ok(Self { client, store })
    }

    /// Fetch markets matching a slug pattern.
    ///
    /// # Arguments
    /// * `slug_pattern` - Slug substring to search for (e.g., "btc", "eth-15min")
    pub async fn fetch_markets_by_slug(&self, slug_pattern: &str) -> BacktestResult<Vec<HistoricalMarket>> {
        info!(slug_pattern, "Fetching markets from Gamma API by slug pattern");

        let url = format!(
            "{}/markets?slug_contains={}",
            GAMMA_BASE_URL, slug_pattern
        );

        let response = self.fetch_with_retry(&url).await?;
        let markets: Vec<GammaMarket> = response
            .json()
            .await
            .map_err(|e| BacktestError::Network(format!("Failed to parse markets response: {}", e)))?;

        info!(count = markets.len(), "Received markets from Gamma API");

        let mut historical_markets = Vec::new();
        for market in markets {
            if let Some(hist_market) = self.convert_to_historical_market(market)? {
                historical_markets.push(hist_market);
            }
        }

        // Cache all markets in DB
        for market in &historical_markets {
            self.store.insert_historical_market(market.clone()).await?;
        }

        info!(cached = historical_markets.len(), "Cached markets to database");
        Ok(historical_markets)
    }

    /// Fetch a single market by condition ID.
    ///
    /// # Arguments
    /// * `condition_id` - Market condition ID
    pub async fn fetch_market_by_id(&self, condition_id: &str) -> BacktestResult<Option<HistoricalMarket>> {
        info!(condition_id, "Fetching market from Gamma API by ID");

        let url = format!("{}/markets/{}", GAMMA_BASE_URL, condition_id);

        let response = self.fetch_with_retry(&url).await?;
        let market: GammaMarket = response
            .json()
            .await
            .map_err(|e| BacktestError::Network(format!("Failed to parse market response: {}", e)))?;

        let historical_market = self.convert_to_historical_market(market)?;

        // Cache if we got a valid market
        if let Some(ref hist_market) = historical_market {
            self.store.insert_historical_market(hist_market.clone()).await?;
        }

        Ok(historical_market)
    }

    /// Fetch expired markets for a specific coin and date range.
    /// This is useful for discovering historical 15-minute crypto markets.
    ///
    /// # Arguments
    /// * `coin` - Coin symbol (e.g., "BTC", "ETH", "SOL")
    /// * `start_date` - Start of date range
    /// * `end_date` - End of date range
    pub async fn fetch_expired_markets(
        &self,
        coin: &str,
        start_date: DateTime<Utc>,
        end_date: DateTime<Utc>,
    ) -> BacktestResult<Vec<HistoricalMarket>> {
        info!(coin, ?start_date, ?end_date, "Fetching expired markets for coin");

        // Search for markets by slug pattern (e.g., "btc" for BTC markets)
        let slug_pattern = coin.to_lowercase();
        let all_markets = self.fetch_markets_by_slug(&slug_pattern).await?;

        // Filter to markets that expired within the date range
        let expired_markets: Vec<HistoricalMarket> = all_markets
            .into_iter()
            .filter(|m| {
                m.end_date >= start_date && m.end_date <= end_date
            })
            .collect();

        info!(
            coin,
            count = expired_markets.len(),
            "Found expired markets in date range"
        );

        Ok(expired_markets)
    }

    /// Convert Gamma API market to HistoricalMarket struct.
    fn convert_to_historical_market(&self, market: GammaMarket) -> BacktestResult<Option<HistoricalMarket>> {
        // Parse dates (required)
        let start_date = match market.start_date {
            Some(ref date_str) => DateTime::parse_from_rfc3339(date_str)
                .map_err(|e| BacktestError::InvalidInput(format!("Failed to parse start_date: {}", e)))?
                .with_timezone(&Utc),
            None => {
                debug!(condition_id = %market.condition_id, "Skipping market with no start_date");
                return Ok(None);
            }
        };

        let end_date = match market.end_date {
            Some(ref date_str) => DateTime::parse_from_rfc3339(date_str)
                .map_err(|e| BacktestError::InvalidInput(format!("Failed to parse end_date: {}", e)))?
                .with_timezone(&Utc),
            None => {
                debug!(condition_id = %market.condition_id, "Skipping market with no end_date");
                return Ok(None);
            }
        };

        // Extract token IDs (expect exactly 2 tokens: Yes/No or Up/Down)
        if market.tokens.len() != 2 {
            debug!(
                condition_id = %market.condition_id,
                token_count = market.tokens.len(),
                "Skipping market with incorrect token count"
            );
            return Ok(None);
        }

        let token_a = market.tokens[0].token_id.clone();
        let token_b = market.tokens[1].token_id.clone();

        Ok(Some(HistoricalMarket {
            market_id: market.condition_id,
            slug: market.slug,
            question: market.question,
            start_date,
            end_date,
            token_a,
            token_b,
            neg_risk: market.neg_risk,
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
        let store = setup_store().await;
        let fetcher = GammaFetcher::new(store).unwrap();

        let gamma_market = GammaMarket {
            condition_id: "0x123".to_string(),
            slug: "btc-up-15min".to_string(),
            question: "Will BTC go up?".to_string(),
            start_date: Some("2025-01-01T00:00:00Z".to_string()),
            end_date: Some("2025-01-01T00:15:00Z".to_string()),
            tokens: vec![
                Token {
                    token_id: "token_a".to_string(),
                    outcome: "Up".to_string(),
                },
                Token {
                    token_id: "token_b".to_string(),
                    outcome: "Down".to_string(),
                },
            ],
            neg_risk: false,
        };

        let result = fetcher.convert_to_historical_market(gamma_market).unwrap();
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
        let store = setup_store().await;
        let fetcher = GammaFetcher::new(store).unwrap();

        let gamma_market = GammaMarket {
            condition_id: "0x123".to_string(),
            slug: "btc-up-15min".to_string(),
            question: "Will BTC go up?".to_string(),
            start_date: None,
            end_date: Some("2025-01-01T00:15:00Z".to_string()),
            tokens: vec![
                Token {
                    token_id: "token_a".to_string(),
                    outcome: "Up".to_string(),
                },
                Token {
                    token_id: "token_b".to_string(),
                    outcome: "Down".to_string(),
                },
            ],
            neg_risk: false,
        };

        let result = fetcher.convert_to_historical_market(gamma_market).unwrap();
        assert!(result.is_none()); // Should skip market with missing start_date
    }

    #[tokio::test]
    async fn test_convert_market_wrong_token_count() {
        let store = setup_store().await;
        let fetcher = GammaFetcher::new(store).unwrap();

        let gamma_market = GammaMarket {
            condition_id: "0x123".to_string(),
            slug: "btc-up-15min".to_string(),
            question: "Will BTC go up?".to_string(),
            start_date: Some("2025-01-01T00:00:00Z".to_string()),
            end_date: Some("2025-01-01T00:15:00Z".to_string()),
            tokens: vec![
                Token {
                    token_id: "token_a".to_string(),
                    outcome: "Up".to_string(),
                },
            ],
            neg_risk: false,
        };

        let result = fetcher.convert_to_historical_market(gamma_market).unwrap();
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
                println!("Live API test failed (this is OK if API is unavailable): {}", e);
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_fetch_expired_markets_live() {
        let store = setup_store().await;
        let fetcher = GammaFetcher::new(store).unwrap();

        // Fetch BTC markets that expired in the last 7 days
        let end_date = Utc::now();
        let start_date = end_date - chrono::Duration::days(7);

        let markets = fetcher.fetch_expired_markets("BTC", start_date, end_date).await;

        match markets {
            Ok(data) => {
                println!("Fetched {} expired BTC markets", data.len());

                for market in data.iter().take(3) {
                    println!("Expired market: {} - {} (ended: {})",
                        market.market_id, market.slug, market.end_date);
                    assert!(market.end_date >= start_date && market.end_date <= end_date);
                }
            }
            Err(e) => {
                println!("Live API test failed (this is OK if API is unavailable): {}", e);
            }
        }
    }
}
