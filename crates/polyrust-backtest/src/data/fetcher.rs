use chrono::{DateTime, Utc};
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::data::{
    GammaFetcher, HistoricalDataStore, HistoricalMarket, HistoricalPrice,
    HistoricalTrade, SubgraphFetcher,
};
use crate::error::BacktestResult;

/// Configuration for data fetching behavior.
#[derive(Debug, Clone)]
pub struct DataFetchConfig {
    /// Price resolution in seconds (e.g., 60 = 1min, 300 = 5min)
    pub fidelity_secs: u64,
}

impl Default for DataFetchConfig {
    fn default() -> Self {
        Self {
            fidelity_secs: 60,
        }
    }
}

/// Combined market data (prices + trades) from cache.
#[derive(Debug, Clone)]
pub struct CachedMarketData {
    pub prices: Vec<HistoricalPrice>,
    pub trades: Vec<HistoricalTrade>,
}

/// Unified data fetcher that orchestrates Gamma and Goldsky subgraph fetchers.
/// All trade data comes from the Goldsky subgraph (unlimited historical range).
/// Cache-aware: checks `data_fetch_log` before fetching, avoids re-fetching cached ranges.
pub struct DataFetcher {
    store: Arc<HistoricalDataStore>,
    gamma_fetcher: GammaFetcher,
    subgraph_fetcher: SubgraphFetcher,
    _config: DataFetchConfig,
}

impl DataFetcher {
    /// Create a new DataFetcher with the given store and config.
    pub fn new(
        store: Arc<HistoricalDataStore>,
        config: DataFetchConfig,
    ) -> BacktestResult<Self> {
        let gamma_fetcher = GammaFetcher::new(Arc::clone(&store))?;
        let subgraph_fetcher = SubgraphFetcher::new(Arc::clone(&store))?;

        Ok(Self {
            store,
            gamma_fetcher,
            subgraph_fetcher,
            _config: config,
        })
    }

    /// Fetch market data for a single token within a date range.
    /// All trade data comes from the Goldsky subgraph.
    /// PriceChange events are synthesized from trades in the backtest engine.
    ///
    /// # Arguments
    /// * `token_id` - Token ID to fetch data for
    /// * `start` - Start of date range
    /// * `end` - End of date range
    pub async fn fetch_market_data(
        &self,
        token_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<CachedMarketData> {
        info!(token_id, ?start, ?end, "Fetching market data from subgraph");

        let trades = self
            .subgraph_fetcher
            .fetch_subgraph_trades(token_id, start.timestamp(), end.timestamp())
            .await?;

        info!(token_id, trades = trades.len(), "Market data fetched");

        Ok(CachedMarketData {
            prices: Vec::new(),
            trades,
        })
    }

    /// Bulk fetch and cache data for multiple tokens over a date range.
    /// Use this for backtest preparation — fetches everything needed, stores in DB.
    ///
    /// # Arguments
    /// * `token_ids` - List of token IDs to fetch
    /// * `start` - Start of date range
    /// * `end` - End of date range
    pub async fn fetch_and_cache(
        &self,
        token_ids: &[String],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<()> {
        info!(tokens = token_ids.len(), ?start, ?end, "Bulk fetching and caching market data");

        for (idx, token_id) in token_ids.iter().enumerate() {
            info!(progress = format!("{}/{}", idx + 1, token_ids.len()), token_id, "Fetching token data");

            match self.fetch_market_data(token_id, start, end).await {
                Ok(_data) => {
                    debug!(token_id, "Successfully fetched and cached");
                }
                Err(e) => {
                    warn!(token_id, error = %e, "Failed to fetch token data, continuing");
                }
            }
        }

        info!(tokens = token_ids.len(), "Bulk fetch complete");
        Ok(())
    }

    /// Retrieve cached data for a token from the database.
    /// Does not trigger any API calls.
    ///
    /// # Arguments
    /// * `token_id` - Token ID to query
    /// * `start` - Start of date range
    /// * `end` - End of date range
    pub async fn get_cached_data(
        &self,
        token_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<CachedMarketData> {
        let prices = self.store.get_historical_prices(token_id, start, end).await?;
        let trades = self.store.get_historical_trades(token_id, start, end).await?;

        Ok(CachedMarketData { prices, trades })
    }

    /// Discover markets by slug pattern using Gamma API.
    /// Caches discovered markets in the database.
    ///
    /// # Arguments
    /// * `slug_pattern` - Slug substring to search for (e.g., "btc", "eth-15min")
    pub async fn discover_markets(&self, slug_pattern: &str) -> BacktestResult<Vec<HistoricalMarket>> {
        self.gamma_fetcher.fetch_markets_by_slug(slug_pattern).await
    }

    /// Discover expired markets for a specific coin within a date range.
    /// Uses Gamma API to find historical 15-min crypto markets.
    ///
    /// # Arguments
    /// * `coin` - Coin symbol (e.g., "BTC", "ETH")
    /// * `start` - Start of date range
    /// * `end` - End of date range
    /// * `duration_filter` - Optional filter for exact market duration in seconds
    pub async fn discover_expired_markets(
        &self,
        coin: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        duration_filter: Option<u64>,
    ) -> BacktestResult<Vec<HistoricalMarket>> {
        self.gamma_fetcher.fetch_expired_markets(coin, start, end, duration_filter).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    async fn create_test_store() -> Arc<HistoricalDataStore> {
        Arc::new(HistoricalDataStore::new(":memory:").await.unwrap())
    }

    #[tokio::test]
    async fn test_data_fetcher_new() {
        let store = create_test_store().await;
        let config = DataFetchConfig::default();
        let fetcher = DataFetcher::new(store, config);
        assert!(fetcher.is_ok());
    }

    #[tokio::test]
    async fn test_default_config() {
        let config = DataFetchConfig::default();
        assert_eq!(config.fidelity_secs, 60);
    }

    #[tokio::test]
    async fn test_get_cached_data_empty() {
        let store = create_test_store().await;
        let config = DataFetchConfig::default();
        let fetcher = DataFetcher::new(store, config).unwrap();

        let start = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();

        let result = fetcher.get_cached_data("test_token", start, end).await;
        assert!(result.is_ok());
        let data = result.unwrap();
        assert_eq!(data.prices.len(), 0);
        assert_eq!(data.trades.len(), 0);
    }

    #[tokio::test]
    async fn test_get_cached_data_with_data() {
        let store = create_test_store().await;

        // Insert some test data
        let test_prices = vec![
            HistoricalPrice {
                token_id: "test_token".to_string(),
                timestamp: Utc.with_ymd_and_hms(2025, 1, 1, 12, 0, 0).unwrap(),
                price: dec!(0.50),
                source: "clob".to_string(),
            },
            HistoricalPrice {
                token_id: "test_token".to_string(),
                timestamp: Utc.with_ymd_and_hms(2025, 1, 1, 13, 0, 0).unwrap(),
                price: dec!(0.55),
                source: "clob".to_string(),
            },
        ];

        let test_trades = vec![HistoricalTrade {
            id: "trade1".to_string(),
            token_id: "test_token".to_string(),
            timestamp: Utc.with_ymd_and_hms(2025, 1, 1, 12, 30, 0).unwrap(),
            price: dec!(0.52),
            size: dec!(100.0),
            side: "buy".to_string(),
            source: "clob".to_string(),
        }];

        store.insert_historical_prices(test_prices.clone()).await.unwrap();
        store.insert_historical_trades(test_trades.clone()).await.unwrap();

        let config = DataFetchConfig::default();
        let fetcher = DataFetcher::new(Arc::clone(&store), config).unwrap();

        let start = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();

        let result = fetcher.get_cached_data("test_token", start, end).await;
        assert!(result.is_ok());
        let data = result.unwrap();
        assert_eq!(data.prices.len(), 2);
        assert_eq!(data.trades.len(), 1);
    }

}
