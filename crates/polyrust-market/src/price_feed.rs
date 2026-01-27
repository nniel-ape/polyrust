use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use polymarket_client_sdk::rtds;
use polyrust_core::prelude::*;
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::feed::MarketDataFeed;

/// Cached crypto price entry.
#[derive(Debug, Clone)]
pub struct CachedPrice {
    pub price: Decimal,
    pub source: String,
    pub timestamp: DateTime<Utc>,
}

/// RTDS crypto price feed using the Polymarket Real-Time Data Streams API.
///
/// Subscribes to `crypto_prices` (Binance, fast) and `crypto_prices_chainlink`
/// (Chainlink, used for Polymarket resolution) topics. Publishes
/// `MarketDataEvent::ExternalPrice` events to the EventBus.
pub struct PriceFeed {
    event_bus: Option<EventBus>,
    price_cache: Arc<RwLock<HashMap<String, CachedPrice>>>,
    task_handles: Vec<JoinHandle<()>>,
}

impl PriceFeed {
    pub fn new() -> Self {
        Self {
            event_bus: None,
            price_cache: Arc::new(RwLock::new(HashMap::new())),
            task_handles: Vec::new(),
        }
    }

    /// Get a thread-safe reference to the price cache.
    pub fn price_cache(&self) -> Arc<RwLock<HashMap<String, CachedPrice>>> {
        self.price_cache.clone()
    }

    /// Get the latest cached price for a symbol.
    pub async fn get_price(&self, symbol: &str) -> Option<CachedPrice> {
        let cache = self.price_cache.read().await;
        cache.get(symbol).cloned()
    }
}

impl Default for PriceFeed {
    fn default() -> Self {
        Self::new()
    }
}

/// Normalize a symbol from SDK format to uppercase base currency.
/// e.g., "btcusdt" -> "BTC", "eth/usd" -> "ETH"
fn normalize_symbol(raw: &str) -> String {
    let upper = raw.to_uppercase();
    // Chainlink format: "ETH/USD" -> "ETH"
    if upper.contains('/')
        && let Some(base) = upper.split('/').next()
    {
        return base.trim().to_string();
    }
    // Binance format: "BTCUSDT" -> "BTC"
    // Try longer suffixes first to avoid partial matches (BUSD before USD)
    let stripped = upper
        .strip_suffix("USDT")
        .or_else(|| upper.strip_suffix("BUSD"))
        .or_else(|| upper.strip_suffix("USD"))
        .unwrap_or(&upper);
    stripped.to_string()
}

#[async_trait]
impl MarketDataFeed for PriceFeed {
    async fn start(&mut self, event_bus: EventBus) -> Result<()> {
        info!("starting RTDS crypto price feed");
        self.event_bus = Some(event_bus.clone());

        let rtds_client = Arc::new(rtds::Client::default());

        // Spawn Chainlink price stream
        {
            let client = Arc::clone(&rtds_client);
            let bus = event_bus.clone();
            let cache = self.price_cache.clone();

            self.task_handles.push(tokio::spawn(async move {
                let stream = match client.subscribe_chainlink_prices(None) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "failed to subscribe to chainlink prices");
                        return;
                    }
                };

                let mut stream = std::pin::pin!(stream);
                while let Some(result) = stream.next().await {
                    match result {
                        Ok(price_msg) => {
                            let symbol = normalize_symbol(&price_msg.symbol);
                            let timestamp =
                                DateTime::<Utc>::from_timestamp_millis(price_msg.timestamp)
                                    .unwrap_or_else(Utc::now);

                            debug!(
                                symbol = %symbol,
                                price = %price_msg.value,
                                source = "chainlink",
                                "chainlink price update"
                            );

                            {
                                let mut c = cache.write().await;
                                c.insert(
                                    symbol.clone(),
                                    CachedPrice {
                                        price: price_msg.value,
                                        source: "chainlink".into(),
                                        timestamp,
                                    },
                                );
                            }

                            bus.publish(Event::MarketData(MarketDataEvent::ExternalPrice {
                                symbol,
                                price: price_msg.value,
                                source: "chainlink".into(),
                                timestamp,
                            }));
                        }
                        Err(e) => {
                            warn!(error = %e, "chainlink price stream error");
                        }
                    }
                }
                info!("chainlink price stream ended");
            }));
        }

        // Spawn Binance price stream
        {
            let client = Arc::clone(&rtds_client);
            let bus = event_bus;
            let cache = self.price_cache.clone();

            self.task_handles.push(tokio::spawn(async move {
                let stream = match client.subscribe_crypto_prices(None) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "failed to subscribe to binance crypto prices");
                        return;
                    }
                };

                let mut stream = std::pin::pin!(stream);
                while let Some(result) = stream.next().await {
                    match result {
                        Ok(price_msg) => {
                            let symbol = normalize_symbol(&price_msg.symbol);
                            let timestamp =
                                DateTime::<Utc>::from_timestamp_millis(price_msg.timestamp)
                                    .unwrap_or_else(Utc::now);

                            debug!(
                                symbol = %symbol,
                                price = %price_msg.value,
                                source = "binance",
                                "binance price update"
                            );

                            let should_publish = {
                                let mut c = cache.write().await;
                                let should_update = c.get(&symbol).is_none_or(|existing| {
                                    // Prefer Chainlink but allow Binance to overwrite
                                    // stale Chainlink data (>30s old)
                                    existing.source != "chainlink"
                                        || (timestamp - existing.timestamp)
                                            > chrono::Duration::seconds(30)
                                });
                                if should_update {
                                    c.insert(
                                        symbol.clone(),
                                        CachedPrice {
                                            price: price_msg.value,
                                            source: "binance".into(),
                                            timestamp,
                                        },
                                    );
                                }
                                should_update
                            };

                            if should_publish {
                                bus.publish(Event::MarketData(MarketDataEvent::ExternalPrice {
                                    symbol,
                                    price: price_msg.value,
                                    source: "binance".into(),
                                    timestamp,
                                }));
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "binance price stream error");
                        }
                    }
                }
                info!("binance price stream ended");
            }));
        }

        info!("RTDS price feeds started (chainlink + binance)");
        Ok(())
    }

    async fn subscribe_market(&mut self, _market: &MarketInfo) -> Result<()> {
        // Price feeds are symbol-based, not market-based.
        // All crypto prices are streamed globally; individual market subscriptions are a no-op.
        debug!("price feed subscribe_market is a no-op (prices are global)");
        Ok(())
    }

    async fn unsubscribe_market(&mut self, _market_id: &str) -> Result<()> {
        debug!("price feed unsubscribe_market is a no-op (prices are global)");
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        info!("stopping RTDS price feed");

        for handle in self.task_handles.drain(..) {
            handle.abort();
        }

        self.event_bus = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_symbol_chainlink() {
        assert_eq!(normalize_symbol("eth/usd"), "ETH");
        assert_eq!(normalize_symbol("btc/usd"), "BTC");
        assert_eq!(normalize_symbol("SOL/USD"), "SOL");
    }

    #[test]
    fn test_normalize_symbol_binance() {
        assert_eq!(normalize_symbol("btcusdt"), "BTC");
        assert_eq!(normalize_symbol("ethusdt"), "ETH");
        assert_eq!(normalize_symbol("solusdt"), "SOL");
        assert_eq!(normalize_symbol("XRPUSDT"), "XRP");
    }

    #[test]
    fn test_normalize_symbol_already_clean() {
        assert_eq!(normalize_symbol("BTC"), "BTC");
        assert_eq!(normalize_symbol("ETH"), "ETH");
    }

    #[test]
    fn test_normalize_symbol_usd_suffix() {
        assert_eq!(normalize_symbol("BTCUSD"), "BTC");
        assert_eq!(normalize_symbol("ETHBUSD"), "ETH");
    }
}
