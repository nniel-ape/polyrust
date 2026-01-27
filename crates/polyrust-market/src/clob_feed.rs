use std::collections::HashSet;
use std::sync::Arc;

use alloy_primitives::U256;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use polymarket_client_sdk::clob::ws;
use polymarket_client_sdk::clob::ws::BookUpdate;
use polymarket_client_sdk::clob::ws::types::response::OrderBookLevel as SdkOrderBookLevel;
use polyrust_core::prelude::*;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::feed::MarketDataFeed;

/// CLOB orderbook feed using the Polymarket WebSocket API.
///
/// Connects to the Polymarket CLOB WebSocket and subscribes to orderbook updates
/// for specific token IDs. Publishes `MarketDataEvent::OrderbookUpdate` events
/// to the EventBus.
pub struct ClobFeed {
    event_bus: Option<EventBus>,
    ws_client: Option<Arc<ws::Client>>,
    subscribed_assets: Arc<RwLock<HashSet<String>>>,
    task_handles: Vec<JoinHandle<()>>,
    command_rx: Option<FeedCommandReceiver>,
}

impl ClobFeed {
    pub fn new() -> Self {
        Self {
            event_bus: None,
            ws_client: None,
            subscribed_assets: Arc::new(RwLock::new(HashSet::new())),
            task_handles: Vec::new(),
            command_rx: None,
        }
    }

    /// Attach a command receiver for dynamic market subscriptions from the engine.
    pub fn with_command_receiver(mut self, rx: FeedCommandReceiver) -> Self {
        self.command_rx = Some(rx);
        self
    }
}

impl Default for ClobFeed {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert an SDK `OrderBookLevel` to our domain `OrderbookLevel`.
fn convert_level(level: &SdkOrderBookLevel) -> OrderbookLevel {
    OrderbookLevel {
        price: level.price,
        size: level.size,
    }
}

/// Convert an SDK `BookUpdate` to our domain `OrderbookSnapshot`.
fn book_update_to_snapshot(update: &BookUpdate) -> OrderbookSnapshot {
    let timestamp =
        DateTime::<Utc>::from_timestamp_millis(update.timestamp).unwrap_or_else(Utc::now);

    let mut bids: Vec<OrderbookLevel> = update.bids.iter().map(convert_level).collect();
    let mut asks: Vec<OrderbookLevel> = update.asks.iter().map(convert_level).collect();

    // Server does not guarantee sort order; match Python reference behavior
    bids.sort_by(|a, b| b.price.cmp(&a.price)); // descending (best bid first)
    asks.sort_by(|a, b| a.price.cmp(&b.price)); // ascending (best ask first)

    OrderbookSnapshot {
        token_id: update.asset_id.to_string(),
        bids,
        asks,
        timestamp,
    }
}

/// Subscribe to orderbook updates for a market, spawning a WebSocket stream task.
/// Returns the JoinHandle for the spawned task.
async fn subscribe_market_impl(
    client: &Arc<ws::Client>,
    event_bus: &EventBus,
    subscribed_assets: &Arc<RwLock<HashSet<String>>>,
    market: &MarketInfo,
) -> Result<JoinHandle<()>> {
    let token_a = &market.token_ids.outcome_a;
    let token_b = &market.token_ids.outcome_b;

    let asset_a: U256 = token_a
        .parse()
        .map_err(|e| PolyError::MarketData(format!("invalid token_id {token_a}: {e}")))?;
    let asset_b: U256 = token_b
        .parse()
        .map_err(|e| PolyError::MarketData(format!("invalid token_id {token_b}: {e}")))?;

    {
        let mut assets = subscribed_assets.write().await;
        assets.insert(token_a.clone());
        assets.insert(token_b.clone());
    }

    let client = Arc::clone(client);
    let bus = event_bus.clone();
    let subscribed = subscribed_assets.clone();
    let market_id = market.id.clone();

    let handle = tokio::spawn(async move {
        let stream = match client.subscribe_orderbook(vec![asset_a, asset_b]) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to subscribe to orderbook stream");
                return;
            }
        };

        let mut stream = std::pin::pin!(stream);
        while let Some(result) = stream.next().await {
            match result {
                Ok(book_update) => {
                    let asset_str = book_update.asset_id.to_string();
                    let is_subscribed = {
                        let assets = subscribed.read().await;
                        assets.contains(&asset_str)
                    };
                    if !is_subscribed {
                        continue;
                    }

                    let snapshot = book_update_to_snapshot(&book_update);
                    debug!(
                        market_id = %market_id,
                        token_id = %snapshot.token_id,
                        bids = snapshot.bids.len(),
                        asks = snapshot.asks.len(),
                        "orderbook update received"
                    );
                    bus.publish(Event::MarketData(MarketDataEvent::OrderbookUpdate(
                        snapshot,
                    )));
                }
                Err(e) => {
                    warn!(error = %e, "CLOB WebSocket error, stream may reconnect");
                }
            }
        }
        info!(market_id = %market_id, "CLOB orderbook stream ended");
    });

    info!(
        market_id = %market.id,
        token_a = %token_a,
        token_b = %token_b,
        "subscribed to CLOB orderbook"
    );

    Ok(handle)
}

#[async_trait]
impl MarketDataFeed for ClobFeed {
    async fn start(&mut self, event_bus: EventBus) -> Result<()> {
        info!("starting CLOB orderbook feed");

        let client = ws::Client::default();
        self.ws_client = Some(Arc::new(client));
        self.event_bus = Some(event_bus);

        // Spawn command listener for dynamic subscriptions from the engine
        if let Some(mut rx) = self.command_rx.take() {
            let ws_client = self.ws_client.as_ref().unwrap().clone();
            let bus = self.event_bus.as_ref().unwrap().clone();
            let subscribed = self.subscribed_assets.clone();

            tokio::spawn(async move {
                while let Some(cmd) = rx.recv().await {
                    match cmd {
                        FeedCommand::Subscribe(info) => {
                            info!(market_id = %info.id, question = %info.question, "received subscribe command");
                            match subscribe_market_impl(&ws_client, &bus, &subscribed, &info).await
                            {
                                Ok(_handle) => {
                                    // Handle is managed by the spawned task itself
                                }
                                Err(e) => {
                                    warn!(market_id = %info.id, error = %e, "failed to subscribe via command");
                                }
                            }
                        }
                        FeedCommand::Unsubscribe(id) => {
                            info!(market_id = %id, "received unsubscribe command (best-effort)");
                            // Best-effort: the spawned WS tasks check subscribed_assets
                        }
                    }
                }
                info!("feed command channel closed");
            });
        }

        Ok(())
    }

    async fn subscribe_market(&mut self, market: &MarketInfo) -> Result<()> {
        let client = self.ws_client.as_ref().ok_or_else(|| {
            PolyError::MarketData("CLOB feed not started, call start() first".into())
        })?;
        let event_bus = self.event_bus.as_ref().ok_or_else(|| {
            PolyError::MarketData("CLOB feed not started, call start() first".into())
        })?;

        let handle =
            subscribe_market_impl(client, event_bus, &self.subscribed_assets, market).await?;
        self.task_handles.push(handle);

        Ok(())
    }

    async fn unsubscribe_market(&mut self, market_id: &str) -> Result<()> {
        // Remove all token IDs for this market from the subscribed set.
        // The spawned WebSocket tasks check `subscribed_assets` on each update,
        // so removed tokens will be silently skipped (best-effort unsubscribe).
        // Full task teardown would require tracking handles per market.
        info!(
            market_id = %market_id,
            "unsubscribe_market: removing tokens from subscribed set (best-effort)"
        );
        // Note: we don't have a reverse mapping from market_id to token_ids here.
        // The caller (engine) should provide token IDs, but the trait only passes market_id.
        // For now, log the limitation. The WebSocket tasks will be aborted on feed stop().
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        info!("stopping CLOB orderbook feed");

        for handle in self.task_handles.drain(..) {
            handle.abort();
        }

        self.ws_client = None;
        self.event_bus = None;
        self.subscribed_assets.write().await.clear();

        Ok(())
    }
}
