use async_trait::async_trait;
use polyrust_core::prelude::*;

/// Trait for market data feed producers.
///
/// Implementations connect to external data sources (WebSocket, REST, etc.)
/// and publish events to the EventBus for consumption by strategies and the engine.
#[async_trait]
pub trait MarketDataFeed: Send + Sync {
    /// Start the feed, connecting to the data source and publishing events.
    async fn start(&mut self, event_bus: EventBus) -> Result<()>;

    /// Subscribe to market data for a specific market.
    async fn subscribe_market(&mut self, market: &MarketInfo) -> Result<()>;

    /// Unsubscribe from market data for a specific market.
    async fn unsubscribe_market(&mut self, market_id: &str) -> Result<()>;

    /// Stop the feed, disconnecting from the data source.
    async fn stop(&mut self) -> Result<()>;
}
