use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use polyrust_core::prelude::*;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};
use tokio_tungstenite::connect_async;
use tracing::{debug, info, warn};

use crate::feed::MarketDataFeed;

/// Direct Coinbase WebSocket feed for spot prices.
///
/// Connects to `wss://ws-feed.exchange.coinbase.com` and subscribes to
/// the `ticker` channel for real-time mid-price updates.
///
/// Publishes `ExternalPrice` events with source "coinbase".
pub struct CoinbaseFeed {
    coins: Vec<String>,
    event_bus: Option<EventBus>,
    task_handles: Vec<JoinHandle<()>>,
}

/// Coinbase ticker message (from the "ticker" channel).
#[derive(Deserialize)]
struct TickerMessage {
    #[serde(rename = "type")]
    msg_type: String,
    product_id: String,
    /// Current best bid
    best_bid: Option<String>,
    /// Current best ask
    best_ask: Option<String>,
    time: Option<String>,
}

/// Normalize "BTC-USD" -> "BTC"
fn normalize_symbol(product_id: &str) -> String {
    product_id
        .split('-')
        .next()
        .unwrap_or(product_id)
        .to_uppercase()
}

impl CoinbaseFeed {
    pub fn new(coins: Vec<String>) -> Self {
        Self {
            coins,
            event_bus: None,
            task_handles: Vec::new(),
        }
    }

    /// Build product IDs for subscription (e.g. ["BTC-USD", "ETH-USD"]).
    fn product_ids(&self) -> Vec<String> {
        self.coins
            .iter()
            .map(|c| format!("{}-USD", c.to_uppercase()))
            .collect()
    }
}

#[async_trait]
impl MarketDataFeed for CoinbaseFeed {
    async fn start(&mut self, event_bus: EventBus) -> Result<()> {
        if self.coins.is_empty() {
            info!("CoinbaseFeed: no coins configured, skipping");
            return Ok(());
        }

        info!(coins = ?self.coins, "Starting Coinbase WebSocket feed");
        self.event_bus = Some(event_bus.clone());

        let product_ids = self.product_ids();
        let bus = event_bus;
        self.task_handles.push(tokio::spawn(async move {
            run_coinbase_ws_loop(product_ids, bus).await;
        }));

        info!("CoinbaseFeed started");
        Ok(())
    }

    async fn subscribe_market(&mut self, _market: &MarketInfo) -> Result<()> {
        // Global price feed — individual market subscriptions are a no-op
        Ok(())
    }

    async fn unsubscribe_market(&mut self, _market_id: &str) -> Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        info!("Stopping CoinbaseFeed");
        for handle in self.task_handles.drain(..) {
            handle.abort();
        }
        self.event_bus = None;
        Ok(())
    }
}

/// WebSocket reconnection loop with exponential backoff for Coinbase.
async fn run_coinbase_ws_loop(product_ids: Vec<String>, bus: EventBus) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);
    let url = "wss://ws-feed.exchange.coinbase.com";
    let source = "coinbase";

    loop {
        info!(source, url, "Connecting to Coinbase WebSocket");

        match connect_async(url).await {
            Ok((ws_stream, _)) => {
                info!(source, "Coinbase WebSocket connected");
                backoff = Duration::from_secs(1);

                let (mut write, mut read) = ws_stream.split();

                // Send subscribe message
                let subscribe = serde_json::json!({
                    "type": "subscribe",
                    "product_ids": product_ids,
                    "channels": ["ticker"]
                });
                use futures::SinkExt;
                if let Err(e) = write
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        subscribe.to_string().into(),
                    ))
                    .await
                {
                    warn!(source, error = %e, "Failed to send subscribe message");
                    break;
                }

                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                            if let Ok(ticker) = serde_json::from_str::<TickerMessage>(&text) {
                                if ticker.msg_type != "ticker" {
                                    continue;
                                }
                                // Compute mid price from best_bid and best_ask
                                let mid = match (&ticker.best_bid, &ticker.best_ask) {
                                    (Some(bid_s), Some(ask_s)) => {
                                        match (bid_s.parse::<Decimal>(), ask_s.parse::<Decimal>()) {
                                            (Ok(bid), Ok(ask)) => {
                                                (bid + ask) / Decimal::new(2, 0)
                                            }
                                            _ => continue,
                                        }
                                    }
                                    _ => continue,
                                };

                                let timestamp = ticker
                                    .time
                                    .as_deref()
                                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                                    .map(|dt| dt.with_timezone(&Utc))
                                    .unwrap_or_else(Utc::now);

                                let symbol = normalize_symbol(&ticker.product_id);

                                debug!(
                                    source,
                                    symbol = %symbol,
                                    price = %mid,
                                    "Coinbase price update"
                                );

                                bus.publish(Event::MarketData(MarketDataEvent::ExternalPrice {
                                    symbol,
                                    price: mid,
                                    source: source.to_string(),
                                    timestamp,
                                }));
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(frame)) => {
                            info!(source, ?frame, "Coinbase WebSocket closed by server");
                            break;
                        }
                        Err(e) => {
                            warn!(source, error = %e, "Coinbase WebSocket error");
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                warn!(source, error = %e, backoff_secs = backoff.as_secs(), "Coinbase WebSocket connection failed");
            }
        }

        // Reconnect with exponential backoff
        info!(source, backoff_secs = backoff.as_secs(), "Reconnecting...");
        sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}
