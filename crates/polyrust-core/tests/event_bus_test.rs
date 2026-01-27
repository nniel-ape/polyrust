use polyrust_core::event_bus::EventBus;
use polyrust_core::events::{Event, MarketDataEvent, SystemEvent};
use polyrust_core::types::OrderbookSnapshot;

use chrono::Utc;
use rust_decimal_macros::dec;
use std::time::Duration;
use tokio::time::timeout;

fn make_market_data_event() -> Event {
    Event::MarketData(MarketDataEvent::OrderbookUpdate(OrderbookSnapshot {
        token_id: "token-1".to_string(),
        bids: vec![],
        asks: vec![],
        timestamp: Utc::now(),
    }))
}

fn make_system_event() -> Event {
    Event::System(SystemEvent::EngineStarted)
}

fn make_external_price_event() -> Event {
    Event::MarketData(MarketDataEvent::ExternalPrice {
        symbol: "BTC".to_string(),
        price: dec!(50000),
        source: "binance".to_string(),
        timestamp: Utc::now(),
    })
}

#[tokio::test]
async fn event_bus_publish_to_multiple_subscribers() {
    let bus = EventBus::new();
    let mut sub1 = bus.subscribe();
    let mut sub2 = bus.subscribe();

    assert_eq!(bus.subscriber_count(), 2);

    bus.publish(make_market_data_event());

    let ev1 = timeout(Duration::from_secs(1), sub1.recv())
        .await
        .expect("sub1 timed out")
        .expect("sub1 got None");
    assert_eq!(ev1.topic(), "market_data");

    let ev2 = timeout(Duration::from_secs(1), sub2.recv())
        .await
        .expect("sub2 timed out")
        .expect("sub2 got None");
    assert_eq!(ev2.topic(), "market_data");
}

#[tokio::test]
async fn event_bus_topic_filter_excludes_unmatched() {
    let bus = EventBus::new();
    let mut market_sub = bus.subscribe_topics(&["market_data"]);

    // Publish a system event — should NOT reach the market_data subscriber
    bus.publish(make_system_event());

    // Publish a market data event — should reach it
    bus.publish(make_market_data_event());

    let ev = timeout(Duration::from_secs(1), market_sub.recv())
        .await
        .expect("timed out")
        .expect("got None");
    assert_eq!(ev.topic(), "market_data");
}

#[tokio::test]
async fn event_bus_publish_no_subscribers_does_not_panic() {
    let bus = EventBus::new();
    assert_eq!(bus.subscriber_count(), 0);
    // Should not panic
    bus.publish(make_system_event());
    bus.publish(make_market_data_event());
    bus.publish(make_external_price_event());
}

#[tokio::test]
async fn event_bus_subscriber_handles_lag_gracefully() {
    // Create a tiny-capacity bus to force lag
    let bus = EventBus::with_capacity(4);
    let mut sub = bus.subscribe();

    // Publish more events than the buffer can hold
    for _ in 0..10 {
        bus.publish(make_market_data_event());
    }

    // Subscriber should still recover and receive an event (after lag warning)
    let ev = timeout(Duration::from_secs(1), sub.recv())
        .await
        .expect("timed out")
        .expect("got None after lag");
    assert_eq!(ev.topic(), "market_data");
}
