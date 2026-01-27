use std::collections::HashSet;
use std::sync::Arc;

use alloy_primitives::U256;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use polymarket_client_sdk::clob::ws;
use polymarket_client_sdk::clob::ws::types::response::OrderBookLevel as SdkOrderBookLevel;
use polymarket_client_sdk::clob::ws::BookUpdate;
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
}

impl ClobFeed {
    pub fn new() -> Self {
        Self {
            event_bus: None,
            ws_client: None,
            subscribed_assets: Arc::new(RwLock::new(HashSet::new())),
            task_handles: Vec::new(),
        }
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

    OrderbookSnapshot {
        token_id: update.asset_id.to_string(),
        bids: update.bids.iter().map(convert_level).collect(),
        asks: update.asks.iter().map(convert_level).collect(),
        timestamp,
    }
}

#[async_trait]
impl MarketDataFeed for ClobFeed {
    async fn start(&mut self, event_bus: EventBus) -> Result<()> {
        info!("starting CLOB orderbook feed");

        let client = ws::Client::default();
        self.ws_client = Some(Arc::new(client));
        self.event_bus = Some(event_bus);

        Ok(())
    }

    async fn subscribe_market(&mut self, market: &MarketInfo) -> Result<()> {
        let client = self.ws_client.as_ref().ok_or_else(|| {
            PolyError::MarketData("CLOB feed not started, call start() first".into())
        })?;
        let event_bus = self.event_bus.as_ref().ok_or_else(|| {
            PolyError::MarketData("CLOB feed not started, call start() first".into())
        })?;

        let token_a = &market.token_ids.outcome_a;
        let token_b = &market.token_ids.outcome_b;

        // Parse token IDs to U256 for the SDK
        let asset_a: U256 = token_a
            .parse()
            .map_err(|e| PolyError::MarketData(format!("invalid token_id {token_a}: {e}")))?;
        let asset_b: U256 = token_b
            .parse()
            .map_err(|e| PolyError::MarketData(format!("invalid token_id {token_b}: {e}")))?;

        {
            let mut assets = self.subscribed_assets.write().await;
            assets.insert(token_a.clone());
            assets.insert(token_b.clone());
        }

        // Clone the Arc<Client> so the spawned task owns a reference
        let client = Arc::clone(client);
        let bus = event_bus.clone();
        let subscribed = self.subscribed_assets.clone();
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

        self.task_handles.push(handle);

        info!(
            market_id = %market.id,
            token_a = %token_a,
            token_b = %token_b,
            "subscribed to CLOB orderbook"
        );

        Ok(())
    }

    async fn unsubscribe_market(&mut self, market_id: &str) -> Result<()> {
        warn!(
            market_id = %market_id,
            "unsubscribe_market is best-effort; assets remain tracked until feed stops"
        );
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
