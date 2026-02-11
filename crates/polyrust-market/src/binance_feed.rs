use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use polyrust_core::prelude::*;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};
use tokio_tungstenite::{connect_async, tungstenite};
use tracing::{debug, error, info, warn};

use crate::feed::MarketDataFeed;

/// Direct Binance WebSocket feed for spot + futures prices.
///
/// Runs alongside RTDS PriceFeed. Connects to:
/// - Spot miniTicker stream (`stream.binance.com`)
/// - Futures markPrice stream (`fstream.binance.com`)
///
/// Publishes `ExternalPrice` events with sources "binance-spot" and "binance-futures".
pub struct BinanceFeed {
    coins: Vec<String>,
    event_bus: Option<EventBus>,
    task_handles: Vec<JoinHandle<()>>,
}

// Binance combined stream envelope
#[derive(Deserialize)]
struct CombinedStreamMsg {
    stream: String,
    data: serde_json::Value,
}

// Spot miniTicker payload
#[derive(Deserialize)]
struct MiniTickerData {
    s: String,       // symbol e.g. "BTCUSDT"
    c: String,       // close price
    #[serde(rename = "E")]
    event_time: u64, // event time in ms
}

// Futures markPrice payload
#[derive(Deserialize)]
struct MarkPriceData {
    s: String,       // symbol e.g. "BTCUSDT"
    p: String,       // mark price
    #[serde(rename = "E")]
    event_time: u64, // event time in ms
}

/// Normalize "BTCUSDT" -> "BTC"
fn normalize_symbol(raw: &str) -> String {
    let upper = raw.to_uppercase();
    upper
        .strip_suffix("USDT")
        .or_else(|| upper.strip_suffix("BUSD"))
        .or_else(|| upper.strip_suffix("USD"))
        .unwrap_or(&upper)
        .to_string()
}

impl BinanceFeed {
    pub fn new(coins: Vec<String>) -> Self {
        Self {
            coins,
            event_bus: None,
            task_handles: Vec::new(),
        }
    }

    /// Build combined stream URL for spot miniTicker.
    fn spot_url(&self) -> String {
        let streams: Vec<String> = self
            .coins
            .iter()
            .map(|c| format!("{}usdt@miniTicker", c.to_lowercase()))
            .collect();
        format!(
            "wss://stream.binance.com:9443/stream?streams={}",
            streams.join("/")
        )
    }

    /// Build combined stream URL for futures markPrice (@1s).
    fn futures_url(&self) -> String {
        let streams: Vec<String> = self
            .coins
            .iter()
            .map(|c| format!("{}usdt@markPrice@1s", c.to_lowercase()))
            .collect();
        format!(
            "wss://fstream.binance.com/stream?streams={}",
            streams.join("/")
        )
    }
}

#[async_trait]
impl MarketDataFeed for BinanceFeed {
    async fn start(&mut self, event_bus: EventBus) -> Result<()> {
        if self.coins.is_empty() {
            info!("BinanceFeed: no coins configured, skipping");
            return Ok(());
        }

        info!(coins = ?self.coins, "Starting direct Binance WebSocket feed");
        self.event_bus = Some(event_bus.clone());

        // Spawn spot miniTicker task
        {
            let url = self.spot_url();
            let bus = event_bus.clone();
            self.task_handles.push(tokio::spawn(async move {
                run_ws_loop("binance-spot", &url, bus, parse_mini_ticker).await;
            }));
        }

        // Spawn futures markPrice task
        {
            let url = self.futures_url();
            let bus = event_bus;
            self.task_handles.push(tokio::spawn(async move {
                run_ws_loop("binance-futures", &url, bus, parse_mark_price).await;
            }));
        }

        info!("BinanceFeed started (spot + futures)");
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
        info!("Stopping BinanceFeed");
        for handle in self.task_handles.drain(..) {
            handle.abort();
        }
        self.event_bus = None;
        Ok(())
    }
}

/// Parse a spot miniTicker message into (symbol, price, timestamp).
fn parse_mini_ticker(data: &serde_json::Value) -> Option<(String, Decimal, DateTime<Utc>)> {
    let ticker: MiniTickerData = serde_json::from_value(data.clone()).ok()?;
    let price: Decimal = ticker.c.parse().ok()?;
    let ts = DateTime::from_timestamp_millis(ticker.event_time as i64)?;
    Some((normalize_symbol(&ticker.s), price, ts))
}

/// Parse a futures markPrice message into (symbol, price, timestamp).
fn parse_mark_price(data: &serde_json::Value) -> Option<(String, Decimal, DateTime<Utc>)> {
    let mark: MarkPriceData = serde_json::from_value(data.clone()).ok()?;
    let price: Decimal = mark.p.parse().ok()?;
    let ts = DateTime::from_timestamp_millis(mark.event_time as i64)?;
    Some((normalize_symbol(&mark.s), price, ts))
}

/// Parser function type for extracting (symbol, price, timestamp) from Binance stream data.
type StreamParser = fn(&serde_json::Value) -> Option<(String, Decimal, DateTime<Utc>)>;

/// WebSocket reconnection loop with exponential backoff.
///
/// `parser` extracts (symbol, price, timestamp) from each combined stream message's `data` field.
async fn run_ws_loop(source: &str, url: &str, bus: EventBus, parser: StreamParser) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        info!(source, url, "Connecting to Binance WebSocket");

        let ws_request = match url.parse::<tungstenite::http::Uri>() {
            Ok(uri) => uri,
            Err(e) => {
                error!(source, error = %e, "Invalid WebSocket URL");
                return;
            }
        };

        match connect_async(ws_request).await {
            Ok((ws_stream, _)) => {
                info!(source, "Binance WebSocket connected");
                backoff = Duration::from_secs(1); // Reset on successful connect

                let (_write, mut read) = ws_stream.split();

                while let Some(msg) = read.next().await {
                    match msg {
                        Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                            if let Ok(combined) =
                                serde_json::from_str::<CombinedStreamMsg>(&text)
                                && let Some((symbol, price, timestamp)) =
                                    parser(&combined.data)
                            {
                                debug!(
                                    source,
                                    symbol = %symbol,
                                    price = %price,
                                    stream = %combined.stream,
                                    "Binance price update"
                                );

                                bus.publish(Event::MarketData(
                                    MarketDataEvent::ExternalPrice {
                                        symbol,
                                        price,
                                        source: source.to_string(),
                                        timestamp,
                                    },
                                ));
                            }
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Ping(payload)) => {
                            debug!(source, "Received ping, pong sent automatically");
                            // tokio-tungstenite auto-responds to pings
                            let _ = payload;
                        }
                        Ok(tokio_tungstenite::tungstenite::Message::Close(frame)) => {
                            info!(source, ?frame, "Binance WebSocket closed by server");
                            break;
                        }
                        Err(e) => {
                            warn!(source, error = %e, "Binance WebSocket error");
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                warn!(source, error = %e, backoff_secs = backoff.as_secs(), "Binance WebSocket connection failed");
            }
        }

        // Reconnect with exponential backoff
        info!(source, backoff_secs = backoff.as_secs(), "Reconnecting...");
        sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}
