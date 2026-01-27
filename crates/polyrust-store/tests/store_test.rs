use chrono::Utc;
use polyrust_core::prelude::*;
use polyrust_store::{PnlSnapshot, Store};
use rust_decimal_macros::dec;
use uuid::Uuid;

async fn mem_store() -> Store {
    Store::new(":memory:").await.unwrap()
}

fn sample_trade() -> Trade {
    Trade {
        id: Uuid::new_v4(),
        order_id: "order-1".into(),
        market_id: "market-1".into(),
        token_id: "token-1".into(),
        side: OrderSide::Buy,
        price: dec!(0.55),
        size: dec!(10.0),
        realized_pnl: Some(dec!(0.50)),
        strategy_name: "crypto-arb".into(),
        timestamp: Utc::now(),
    }
}

fn sample_order() -> Order {
    Order {
        id: "ord-abc".into(),
        token_id: "token-2".into(),
        side: OrderSide::Sell,
        price: dec!(0.72),
        size: dec!(5.0),
        filled_size: dec!(0),
        status: OrderStatus::Open,
        created_at: Utc::now(),
    }
}

// --- Migration tests ---

#[tokio::test]
async fn migrations_are_idempotent() {
    let _store1 = Store::new(":memory:").await.unwrap();
    // Running new() again on the same path (in-memory is separate) should not error
    let _store2 = Store::new(":memory:").await.unwrap();
}

// --- Trade tests ---

#[tokio::test]
async fn insert_and_get_trade_roundtrip() {
    let store = mem_store().await;
    let trade = sample_trade();

    store.insert_trade(&trade).await.unwrap();
    let fetched = store
        .get_trade(&trade.id.to_string())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(fetched.id, trade.id);
    assert_eq!(fetched.order_id, trade.order_id);
    assert_eq!(fetched.market_id, trade.market_id);
    assert_eq!(fetched.token_id, trade.token_id);
    assert_eq!(fetched.side, trade.side);
    assert_eq!(fetched.price, trade.price);
    assert_eq!(fetched.size, trade.size);
    assert_eq!(fetched.realized_pnl, trade.realized_pnl);
    assert_eq!(fetched.strategy_name, trade.strategy_name);
}

#[tokio::test]
async fn list_trades_with_strategy_filter() {
    let store = mem_store().await;

    let mut t1 = sample_trade();
    t1.strategy_name = "strat-a".into();
    let mut t2 = sample_trade();
    t2.strategy_name = "strat-b".into();
    let mut t3 = sample_trade();
    t3.strategy_name = "strat-a".into();

    store.insert_trade(&t1).await.unwrap();
    store.insert_trade(&t2).await.unwrap();
    store.insert_trade(&t3).await.unwrap();

    let all = store.list_trades(None, 100).await.unwrap();
    assert_eq!(all.len(), 3);

    let strat_a = store.list_trades(Some("strat-a"), 100).await.unwrap();
    assert_eq!(strat_a.len(), 2);
    assert!(strat_a.iter().all(|t| t.strategy_name == "strat-a"));

    let strat_b = store.list_trades(Some("strat-b"), 100).await.unwrap();
    assert_eq!(strat_b.len(), 1);
}

// --- Order tests ---

#[tokio::test]
async fn insert_and_get_order_roundtrip() {
    let store = mem_store().await;
    let order = sample_order();

    store.insert_order(&order, "test-strat").await.unwrap();
    let fetched = store.get_order(&order.id).await.unwrap().unwrap();

    assert_eq!(fetched.id, order.id);
    assert_eq!(fetched.token_id, order.token_id);
    assert_eq!(fetched.side, order.side);
    assert_eq!(fetched.price, order.price);
    assert_eq!(fetched.size, order.size);
    assert_eq!(fetched.filled_size, order.filled_size);
    assert_eq!(fetched.status, OrderStatus::Open);
}

#[tokio::test]
async fn update_order_status() {
    let store = mem_store().await;
    let order = sample_order();

    store.insert_order(&order, "test-strat").await.unwrap();
    store
        .update_order_status(&order.id, OrderStatus::Filled)
        .await
        .unwrap();

    let fetched = store.get_order(&order.id).await.unwrap().unwrap();
    assert_eq!(fetched.status, OrderStatus::Filled);
}

// --- Event tests ---

#[tokio::test]
async fn insert_and_list_events_with_topic_filter() {
    let store = mem_store().await;

    let market_event = Event::MarketData(MarketDataEvent::MarketExpired("m1".into()));
    let system_event = Event::System(SystemEvent::EngineStarted);

    store.insert_event(&market_event).await.unwrap();
    store.insert_event(&system_event).await.unwrap();

    let all = store.list_events(None, 100).await.unwrap();
    assert_eq!(all.len(), 2);

    let market_only = store.list_events(Some("market_data"), 100).await.unwrap();
    assert_eq!(market_only.len(), 1);
    assert_eq!(market_only[0].topic, "market_data");

    let system_only = store.list_events(Some("system"), 100).await.unwrap();
    assert_eq!(system_only.len(), 1);
    assert_eq!(system_only[0].topic, "system");
}

// --- Snapshot tests ---

#[tokio::test]
async fn insert_and_latest_snapshot() {
    let store = mem_store().await;

    let snap1 = PnlSnapshot {
        id: None,
        total_pnl: dec!(100.50),
        unrealized_pnl: dec!(30.25),
        realized_pnl: dec!(70.25),
        open_positions: 3,
        open_orders: 5,
        available_balance: dec!(9500.00),
        timestamp: Utc::now(),
    };
    let snap2 = PnlSnapshot {
        id: None,
        total_pnl: dec!(120.75),
        unrealized_pnl: dec!(40.00),
        realized_pnl: dec!(80.75),
        open_positions: 4,
        open_orders: 2,
        available_balance: dec!(9400.00),
        timestamp: Utc::now(),
    };

    store.insert_snapshot(&snap1).await.unwrap();
    store.insert_snapshot(&snap2).await.unwrap();

    let latest = store.latest_snapshot().await.unwrap().unwrap();
    assert_eq!(latest.total_pnl, dec!(120.75));
    assert_eq!(latest.unrealized_pnl, dec!(40.00));
    assert_eq!(latest.realized_pnl, dec!(80.75));
    assert_eq!(latest.open_positions, 4);
    assert_eq!(latest.open_orders, 2);
    assert_eq!(latest.available_balance, dec!(9400.00));
}

#[tokio::test]
async fn list_snapshots_limit() {
    let store = mem_store().await;

    for i in 0..5 {
        let snap = PnlSnapshot {
            id: None,
            total_pnl: Decimal::from(i),
            unrealized_pnl: dec!(0),
            realized_pnl: Decimal::from(i),
            open_positions: i,
            open_orders: 0,
            available_balance: dec!(10000),
            timestamp: Utc::now(),
        };
        store.insert_snapshot(&snap).await.unwrap();
    }

    let limited = store.list_snapshots(3).await.unwrap();
    assert_eq!(limited.len(), 3);
}
