use chrono::{DateTime, Utc};
use indicatif::{ProgressBar, ProgressStyle};
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::{info, warn};

use crate::data::store::DataFetchLog;
use crate::data::{
    GammaFetcher, HistoricalCryptoPrice, HistoricalDataStore, HistoricalMarket, HistoricalPrice,
    HistoricalTrade, SubgraphFetcher,
};
use crate::error::{BacktestError, BacktestResult};

/// Configuration for data fetching behavior.
#[derive(Debug, Clone)]
pub struct DataFetchConfig {
    /// Price resolution in seconds (e.g., 60 = 1min, 300 = 5min)
    pub fidelity_secs: u64,
}

impl Default for DataFetchConfig {
    fn default() -> Self {
        Self { fidelity_secs: 60 }
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
    gamma_fetcher: Arc<GammaFetcher>,
    subgraph_fetcher: SubgraphFetcher,
    _config: DataFetchConfig,
}

impl DataFetcher {
    /// Create a new DataFetcher with the given store and config.
    pub fn new(store: Arc<HistoricalDataStore>, config: DataFetchConfig) -> BacktestResult<Self> {
        let gamma_fetcher = Arc::new(GammaFetcher::new(Arc::clone(&store))?);
        let subgraph_fetcher = SubgraphFetcher::new(Arc::clone(&store))?;

        Ok(Self {
            store,
            gamma_fetcher,
            subgraph_fetcher,
            _config: config,
        })
    }

    /// Fetch market data for a market within a date range.
    /// Looks up market metadata (token_a, token_b) from the store,
    /// then batch-fetches trades for both tokens from the orderbook subgraph.
    /// PriceChange events are synthesized from trades in the backtest engine.
    ///
    /// # Arguments
    /// * `market_id` - Market condition ID (maps to token_a + token_b)
    /// * `start` - Start of date range
    /// * `end` - End of date range
    pub async fn fetch_market_data(
        &self,
        market_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<CachedMarketData> {
        // Look up market to get both token IDs
        let market = self.store.get_historical_market(market_id).await?;

        let token_ids: Vec<String> = match market {
            Some(m) => vec![m.token_a, m.token_b],
            None => {
                // Fallback: treat market_id as a single token ID (backwards compat)
                info!(
                    market_id,
                    "Market not found in store, using as token_id directly"
                );
                vec![market_id.to_string()]
            }
        };

        let token_refs: Vec<&str> = token_ids.iter().map(|s| s.as_str()).collect();
        info!(
            market_id,
            ?token_refs,
            ?start,
            ?end,
            "Fetching market data from orderbook subgraph"
        );

        let trades = self
            .subgraph_fetcher
            .fetch_trades_batch(&token_refs, start.timestamp(), end.timestamp())
            .await?;

        info!(market_id, trades = trades.len(), "Market data fetched");

        Ok(CachedMarketData {
            prices: Vec::new(),
            trades,
        })
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
        let prices = self
            .store
            .get_historical_prices(token_id, start, end)
            .await?;
        let trades = self
            .store
            .get_historical_trades(token_id, start, end)
            .await?;

        Ok(CachedMarketData { prices, trades })
    }

    /// Discover markets by slug pattern using Gamma API.
    /// Caches discovered markets in the database.
    ///
    /// # Arguments
    /// * `slug_pattern` - Slug substring to search for (e.g., "btc", "eth-15min")
    pub async fn discover_markets(
        &self,
        slug_pattern: &str,
    ) -> BacktestResult<Vec<HistoricalMarket>> {
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
        self.gamma_fetcher
            .fetch_expired_markets(coin, start, end, duration_filter)
            .await
    }

    /// Fetch historical crypto prices (Binance 1m klines) for the given coins.
    ///
    /// Fetches both spot and futures klines. Cache-aware: checks `data_fetch_log`
    /// before fetching to avoid duplicate API calls.
    pub async fn fetch_crypto_prices(
        &self,
        coins: &[String],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<()> {
        let client = reqwest::Client::new();

        for coin in coins {
            let symbol = format!("{}USDT", coin.to_uppercase());

            // Fetch spot klines
            let spot_source = format!("binance-spot-klines-{}", coin.to_uppercase());
            if !self.is_fetched(&spot_source, coin, start, end).await? {
                let count = fetch_binance_klines(KlinesFetchParams {
                    client: &client,
                    store: &self.store,
                    base_url: &format!(
                        "https://api.binance.com/api/v3/klines?symbol={symbol}&interval=1m"
                    ),
                    coin,
                    source: "binance-spot",
                    start,
                    end,
                    page_limit: 1000,
                })
                .await?;

                self.store
                    .insert_fetch_log(DataFetchLog {
                        id: None,
                        source: spot_source,
                        token_id: coin.to_uppercase(),
                        start_ts: start,
                        end_ts: end,
                        fetched_at: Utc::now(),
                        row_count: count as i64,
                    })
                    .await?;
            } else {
                info!(
                    coin,
                    source = "binance-spot",
                    "Crypto klines already cached, skipping"
                );
            }

            // Fetch futures klines
            let futures_source = format!("binance-futures-klines-{}", coin.to_uppercase());
            if !self.is_fetched(&futures_source, coin, start, end).await? {
                let count = fetch_binance_klines(KlinesFetchParams {
                    client: &client,
                    store: &self.store,
                    base_url: &format!(
                        "https://fapi.binance.com/fapi/v1/klines?symbol={symbol}&interval=1m"
                    ),
                    coin,
                    source: "binance-futures",
                    start,
                    end,
                    page_limit: 1500,
                })
                .await?;

                self.store
                    .insert_fetch_log(DataFetchLog {
                        id: None,
                        source: futures_source,
                        token_id: coin.to_uppercase(),
                        start_ts: start,
                        end_ts: end,
                        fetched_at: Utc::now(),
                        row_count: count as i64,
                    })
                    .await?;
            } else {
                info!(
                    coin,
                    source = "binance-futures",
                    "Crypto klines already cached, skipping"
                );
            }
        }

        Ok(())
    }

    /// Check if data for a given source/coin/range has already been fetched.
    async fn is_fetched(
        &self,
        source: &str,
        coin: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<bool> {
        let logs = self
            .store
            .get_fetch_log(source, &coin.to_uppercase())
            .await?;
        // Consider fetched if any log entry covers the entire requested range
        Ok(logs
            .iter()
            .any(|log| log.start_ts <= start && log.end_ts >= end))
    }
}

/// Parameters for a Binance klines fetch request.
struct KlinesFetchParams<'a> {
    client: &'a reqwest::Client,
    store: &'a Arc<HistoricalDataStore>,
    base_url: &'a str,
    coin: &'a str,
    source: &'a str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    page_limit: u32,
}

/// Fetch Binance klines from a REST endpoint, paginating and storing to DB.
///
/// Returns the total number of klines fetched.
async fn fetch_binance_klines(params: KlinesFetchParams<'_>) -> BacktestResult<usize> {
    let KlinesFetchParams {
        client,
        store,
        base_url,
        coin,
        source,
        start,
        end,
        page_limit,
    } = params;
    let mut current_start_ms = start.timestamp_millis();
    let end_ms = end.timestamp_millis();
    let mut total = 0usize;

    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());
    pb.set_message(format!("{coin} {source}: fetching klines..."));

    loop {
        if current_start_ms >= end_ms {
            break;
        }

        let url =
            format!("{base_url}&startTime={current_start_ms}&endTime={end_ms}&limit={page_limit}");

        let resp =
            client.get(&url).send().await.map_err(|e| {
                BacktestError::DataFetch(format!("Binance klines request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!(
                coin,
                source,
                status = %status,
                body = %body,
                "Binance klines API error"
            );
            // Rate limit — wait and retry
            if status.as_u16() == 429 {
                tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                continue;
            }
            return Err(BacktestError::DataFetch(format!(
                "Binance klines API error: {status}"
            )));
        }

        let klines: Vec<Vec<serde_json::Value>> = resp.json().await.map_err(|e| {
            BacktestError::DataFetch(format!("Failed to parse klines response: {e}"))
        })?;

        if klines.is_empty() {
            break;
        }

        let mut batch = Vec::with_capacity(klines.len());
        let mut last_close_time_ms = current_start_ms;

        for kline in &klines {
            // Binance kline format: [open_time, open, high, low, close, volume, close_time, ...]
            if kline.len() < 7 {
                continue;
            }

            let open_time_ms = kline[0].as_i64().unwrap_or(0);
            let close_time_ms = kline[6].as_i64().unwrap_or(0);

            let parse_dec = |v: &serde_json::Value| -> Option<Decimal> {
                v.as_str().and_then(|s| s.parse().ok())
            };

            let Some(open) = parse_dec(&kline[1]) else {
                continue;
            };
            let Some(high) = parse_dec(&kline[2]) else {
                continue;
            };
            let Some(low) = parse_dec(&kline[3]) else {
                continue;
            };
            let Some(close) = parse_dec(&kline[4]) else {
                continue;
            };
            let Some(volume) = parse_dec(&kline[5]) else {
                continue;
            };

            let timestamp = DateTime::from_timestamp_millis(open_time_ms);
            let Some(ts) = timestamp else { continue };

            batch.push(HistoricalCryptoPrice {
                symbol: coin.to_uppercase(),
                timestamp: ts,
                open,
                high,
                low,
                close,
                volume,
                source: source.to_string(),
            });

            last_close_time_ms = close_time_ms;
        }

        let batch_size = batch.len();
        store.insert_crypto_prices(batch).await?;
        total += batch_size;

        pb.set_message(format!("{coin} {source}: {total} klines fetched"));
        pb.tick();

        // Advance past the last candle's close time
        current_start_ms = last_close_time_ms + 1;

        // If we got fewer than the limit, we've exhausted the data
        if batch_size < page_limit as usize {
            break;
        }

        // Respect rate limits — small delay between pages
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }

    pb.finish_with_message(format!("{coin} {source}: {total} klines complete"));
    Ok(total)
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

        store
            .insert_historical_prices(test_prices.clone())
            .await
            .unwrap();
        store
            .insert_historical_trades(test_trades.clone())
            .await
            .unwrap();

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
