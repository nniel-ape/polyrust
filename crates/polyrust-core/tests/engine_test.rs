use async_trait::async_trait;
use polyrust_core::actions::Action;
use polyrust_core::config::Config;
use polyrust_core::context::StrategyContext;
use polyrust_core::engine::Engine;
use polyrust_core::error::{PolyError, Result};
use polyrust_core::events::Event;
use polyrust_core::execution::ExecutionBackend;
use polyrust_core::strategy::Strategy;
use polyrust_core::types::*;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::NamedTempFile;

// --- Mock execution backend ---

struct MockBackend {
    balance: Decimal,
}

impl MockBackend {
    fn new(balance: Decimal) -> Self {
        Self { balance }
    }
}

#[async_trait]
impl ExecutionBackend for MockBackend {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        Ok(OrderResult {
            success: true,
            order_id: Some("mock-order-1".to_string()),
            token_id: order.token_id.clone(),
            price: order.price,
            size: order.size,
            side: order.side,
            status: Some("placed".to_string()),
            message: "ok".to_string(),
        })
    }

    async fn cancel_order(&self, _order_id: &str) -> Result<()> {
        Ok(())
    }

    async fn cancel_all_orders(&self) -> Result<()> {
        Ok(())
    }

    async fn get_open_orders(&self) -> Result<Vec<Order>> {
        Ok(vec![])
    }

    async fn get_positions(&self) -> Result<Vec<Position>> {
        Ok(vec![])
    }

    async fn get_balance(&self) -> Result<Decimal> {
        Ok(self.balance)
    }
}

// --- Mock strategy ---

struct MockStrategy {
    started: bool,
}

impl MockStrategy {
    fn new() -> Self {
        Self { started: false }
    }
}

#[async_trait]
impl Strategy for MockStrategy {
    fn name(&self) -> &str {
        "mock-strategy"
    }

    fn description(&self) -> &str {
        "A mock strategy for testing"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        self.started = true;
        Ok(())
    }

    async fn on_event(&mut self, _event: &Event, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        Ok(vec![])
    }
}

// --- Tests ---

#[tokio::test]
async fn engine_builder_without_execution_returns_error() {
    let result = Engine::builder()
        .strategy(MockStrategy::new())
        .build()
        .await;

    let err = match result {
        Ok(_) => panic!("expected error, got Ok"),
        Err(e) => e,
    };
    assert!(matches!(err, PolyError::Config(_)));
    assert!(err.to_string().contains("Execution backend is required"));
}

#[tokio::test]
async fn engine_builder_with_execution_succeeds() {
    let result = Engine::builder()
        .execution(MockBackend::new(Decimal::new(5000, 0)))
        .strategy(MockStrategy::new())
        .build()
        .await;

    assert!(result.is_ok());
    let engine = result.unwrap();
    // Verify initial balance was set from backend
    let balance = engine.context().balance.read().await;
    assert_eq!(balance.available_usdc, Decimal::new(5000, 0));
}

#[tokio::test]
async fn engine_builder_with_config() {
    let mut config = Config::default();
    config.engine.event_bus_capacity = 1024;

    let engine = Engine::builder()
        .config(config)
        .execution(MockBackend::new(Decimal::ZERO))
        .build()
        .await
        .unwrap();

    assert_eq!(engine.config().engine.event_bus_capacity, 1024);
}

#[test]
fn config_from_valid_toml_file() {
    let toml_content = r#"
[engine]
event_bus_capacity = 2048
health_check_interval_secs = 60

[dashboard]
enabled = false
port = 8080
host = "0.0.0.0"

[store]
db_path = "test.db"

[paper]
enabled = true
initial_balance = "5000"
"#;

    let mut tmp = NamedTempFile::new().unwrap();
    tmp.write_all(toml_content.as_bytes()).unwrap();
    tmp.flush().unwrap();

    let config = Config::from_file(tmp.path()).unwrap();

    assert_eq!(config.engine.event_bus_capacity, 2048);
    assert_eq!(config.engine.health_check_interval_secs, 60);
    assert!(!config.dashboard.enabled);
    assert_eq!(config.dashboard.port, 8080);
    assert_eq!(config.dashboard.host, "0.0.0.0");
    assert_eq!(config.store.db_path, "test.db");
    assert!(config.paper.enabled);
    assert_eq!(config.paper.initial_balance, Decimal::new(5000, 0));
}

#[test]
fn config_from_file_missing_file_returns_error() {
    let result = Config::from_file("/nonexistent/path.toml");
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), PolyError::Config(_)));
}

#[test]
fn config_defaults_are_correct() {
    let config = Config::default();
    assert_eq!(config.engine.event_bus_capacity, 4096);
    assert_eq!(config.engine.health_check_interval_secs, 30);
    assert!(config.dashboard.enabled);
    assert_eq!(config.dashboard.port, 3000);
    assert_eq!(config.dashboard.host, "127.0.0.1");
    assert_eq!(config.store.db_path, "polyrust.db");
    assert!(config.paper.enabled);
    assert_eq!(config.paper.initial_balance, Decimal::new(10_000, 0));
    assert!(config.polymarket.private_key.is_none());
}

#[test]
fn config_env_overrides_apply() {
    // SAFETY: This test runs single-threaded and cleans up after itself.
    unsafe {
        std::env::set_var("POLY_PRIVATE_KEY", "0xdeadbeef");
        std::env::set_var("POLY_SAFE_ADDRESS", "0xsafe");
        std::env::set_var("POLY_BUILDER_API_KEY", "key123");
        std::env::set_var("POLY_BUILDER_API_SECRET", "secret123");
        std::env::set_var("POLY_BUILDER_API_PASSPHRASE", "pass123");
        std::env::set_var("POLY_DASHBOARD_PORT", "9090");
        std::env::set_var("POLY_DB_PATH", "override.db");
        std::env::set_var("POLY_PAPER_TRADING", "true");
    }

    let config = Config::default().with_env_overrides();

    assert_eq!(config.polymarket.private_key.as_deref(), Some("0xdeadbeef"));
    assert_eq!(config.polymarket.safe_address.as_deref(), Some("0xsafe"));
    assert_eq!(config.polymarket.builder_api_key.as_deref(), Some("key123"));
    assert_eq!(
        config.polymarket.builder_api_secret.as_deref(),
        Some("secret123")
    );
    assert_eq!(
        config.polymarket.builder_api_passphrase.as_deref(),
        Some("pass123")
    );
    assert_eq!(config.dashboard.port, 9090);
    assert_eq!(config.store.db_path, "override.db");
    assert!(config.paper.enabled);

    // Clean up env vars
    unsafe {
        std::env::remove_var("POLY_PRIVATE_KEY");
        std::env::remove_var("POLY_SAFE_ADDRESS");
        std::env::remove_var("POLY_BUILDER_API_KEY");
        std::env::remove_var("POLY_BUILDER_API_SECRET");
        std::env::remove_var("POLY_BUILDER_API_PASSPHRASE");
        std::env::remove_var("POLY_DASHBOARD_PORT");
        std::env::remove_var("POLY_DB_PATH");
        std::env::remove_var("POLY_PAPER_TRADING");
    }
}

#[test]
fn config_from_minimal_toml() {
    // Empty TOML should use all defaults
    let mut tmp = NamedTempFile::new().unwrap();
    tmp.write_all(b"").unwrap();
    tmp.flush().unwrap();

    let config = Config::from_file(tmp.path()).unwrap();
    assert_eq!(config.engine.event_bus_capacity, 4096);
    assert!(config.dashboard.enabled);
    assert!(config.paper.enabled);
}

// --- Batch order tests ---

struct BatchTrackingBackend {
    balance: Decimal,
    place_order_count: AtomicUsize,
}

impl BatchTrackingBackend {
    fn new(balance: Decimal) -> Self {
        Self {
            balance,
            place_order_count: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ExecutionBackend for BatchTrackingBackend {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        self.place_order_count.fetch_add(1, Ordering::Relaxed);
        Ok(OrderResult {
            success: true,
            order_id: Some(format!(
                "batch-order-{}",
                self.place_order_count.load(Ordering::Relaxed)
            )),
            token_id: order.token_id.clone(),
            price: order.price,
            size: order.size,
            side: order.side,
            status: Some("placed".to_string()),
            message: "ok".to_string(),
        })
    }

    async fn cancel_order(&self, _order_id: &str) -> Result<()> {
        Ok(())
    }
    async fn cancel_all_orders(&self) -> Result<()> {
        Ok(())
    }
    async fn get_open_orders(&self) -> Result<Vec<Order>> {
        Ok(vec![])
    }
    async fn get_positions(&self) -> Result<Vec<Position>> {
        Ok(vec![])
    }
    async fn get_balance(&self) -> Result<Decimal> {
        Ok(self.balance)
    }
}

#[tokio::test]
async fn default_place_batch_orders_calls_place_order_per_item() {
    let backend = BatchTrackingBackend::new(dec!(1000));

    let orders = vec![
        OrderRequest::new(
            "token1".to_string(),
            dec!(0.50),
            dec!(10),
            OrderSide::Buy,
            OrderType::Gtc,
            false,
        ),
        OrderRequest::new(
            "token2".to_string(),
            dec!(0.40),
            dec!(10),
            OrderSide::Buy,
            OrderType::Gtc,
            false,
        ),
    ];

    let results = backend.place_batch_orders(&orders).await.unwrap();

    assert_eq!(results.len(), 2);
    assert!(results[0].success);
    assert!(results[1].success);
    assert_eq!(results[0].token_id, "token1");
    assert_eq!(results[1].token_id, "token2");
    // Default impl calls place_order for each item
    assert_eq!(backend.place_order_count.load(Ordering::Relaxed), 2);
}

// --- Phase 6: Engine integration tests (execute_action event publishing) ---

use polyrust_core::engine::execute_action;
use polyrust_core::event_bus::EventBus;
use polyrust_core::events::OrderEvent;

/// Mock backend for engine integration tests that returns configurable results.
struct ConfigurableBackend {
    balance: Decimal,
    reject: bool,
}

impl ConfigurableBackend {
    fn accepting(balance: Decimal) -> Self {
        Self {
            balance,
            reject: false,
        }
    }
    fn rejecting(balance: Decimal) -> Self {
        Self {
            balance,
            reject: true,
        }
    }
}

#[async_trait]
impl ExecutionBackend for ConfigurableBackend {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        if self.reject {
            Ok(OrderResult {
                success: false,
                order_id: None,
                token_id: order.token_id.clone(),
                price: order.price,
                size: order.size,
                side: order.side,
                status: None,
                message: "Insufficient balance".to_string(),
            })
        } else {
            Ok(OrderResult {
                success: true,
                order_id: Some("order-123".to_string()),
                token_id: order.token_id.clone(),
                price: order.price,
                size: order.size,
                side: order.side,
                status: Some("Filled".to_string()),
                message: "ok".to_string(),
            })
        }
    }

    async fn cancel_order(&self, _order_id: &str) -> Result<()> {
        Ok(())
    }
    async fn cancel_all_orders(&self) -> Result<()> {
        Ok(())
    }
    async fn get_open_orders(&self) -> Result<Vec<Order>> {
        Ok(vec![])
    }
    async fn get_positions(&self) -> Result<Vec<Position>> {
        Ok(vec![])
    }
    async fn get_balance(&self) -> Result<Decimal> {
        Ok(self.balance)
    }
}

#[tokio::test]
async fn engine_place_order_publishes_placed_event() {
    let backend: Arc<dyn ExecutionBackend> = Arc::new(ConfigurableBackend::accepting(dec!(1000)));
    let event_bus = EventBus::default();
    let context = StrategyContext::new();
    let mut subscriber = event_bus.subscribe();

    let action = Action::PlaceOrder(OrderRequest::new(
        "token1".to_string(),
        dec!(0.50),
        dec!(10),
        OrderSide::Buy,
        OrderType::Gtc,
        false,
    ));

    execute_action(
        &action,
        &backend,
        &event_bus,
        &context,
        "test-strategy",
        None,
    )
    .await
    .unwrap();

    // Should receive Placed event
    let event = tokio::time::timeout(std::time::Duration::from_millis(100), subscriber.recv())
        .await
        .unwrap()
        .unwrap();

    match event {
        Event::OrderUpdate(OrderEvent::Placed(result)) => {
            assert!(result.success);
            assert_eq!(result.token_id, "token1");
            assert_eq!(result.order_id.as_deref(), Some("order-123"));
        }
        other => panic!("Expected Placed event, got {:?}", other),
    }

    // Balance should be synced to context
    let balance = context.balance.read().await;
    assert_eq!(balance.available_usdc, dec!(1000));
}

#[tokio::test]
async fn engine_order_rejection_publishes_rejected_event() {
    let backend: Arc<dyn ExecutionBackend> = Arc::new(ConfigurableBackend::rejecting(dec!(1000)));
    let event_bus = EventBus::default();
    let context = StrategyContext::new();
    let mut subscriber = event_bus.subscribe();

    let action = Action::PlaceOrder(OrderRequest::new(
        "token1".to_string(),
        dec!(0.50),
        dec!(10),
        OrderSide::Buy,
        OrderType::Gtc,
        false,
    ));

    execute_action(
        &action,
        &backend,
        &event_bus,
        &context,
        "test-strategy",
        None,
    )
    .await
    .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_millis(100), subscriber.recv())
        .await
        .unwrap()
        .unwrap();

    match event {
        Event::OrderUpdate(OrderEvent::Rejected {
            reason, token_id, ..
        }) => {
            assert!(reason.contains("Insufficient balance"));
            assert_eq!(token_id, Some("token1".to_string()));
        }
        other => panic!("Expected Rejected event, got {:?}", other),
    }
}

#[tokio::test]
async fn engine_cancel_order_publishes_cancelled_event() {
    let backend: Arc<dyn ExecutionBackend> = Arc::new(ConfigurableBackend::accepting(dec!(1000)));
    let event_bus = EventBus::default();
    let context = StrategyContext::new();
    let mut subscriber = event_bus.subscribe();

    let action = Action::CancelOrder("order-456".to_string());

    execute_action(
        &action,
        &backend,
        &event_bus,
        &context,
        "test-strategy",
        None,
    )
    .await
    .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_millis(100), subscriber.recv())
        .await
        .unwrap()
        .unwrap();

    match event {
        Event::OrderUpdate(OrderEvent::Cancelled(id)) => {
            assert_eq!(id, "order-456");
        }
        other => panic!("Expected Cancelled event, got {:?}", other),
    }
}

#[tokio::test]
async fn engine_batch_order_publishes_per_leg() {
    let backend: Arc<dyn ExecutionBackend> = Arc::new(ConfigurableBackend::accepting(dec!(1000)));
    let event_bus = EventBus::default();
    let context = StrategyContext::new();
    let mut subscriber = event_bus.subscribe();

    let action = Action::PlaceBatchOrder(vec![
        OrderRequest::new(
            "token_a".to_string(),
            dec!(0.45),
            dec!(10),
            OrderSide::Buy,
            OrderType::Gtc,
            false,
        ),
        OrderRequest::new(
            "token_b".to_string(),
            dec!(0.50),
            dec!(10),
            OrderSide::Buy,
            OrderType::Gtc,
            false,
        ),
    ]);

    execute_action(
        &action,
        &backend,
        &event_bus,
        &context,
        "test-strategy",
        None,
    )
    .await
    .unwrap();

    // Should receive 2 Placed events (one per leg)
    let event1 = tokio::time::timeout(std::time::Duration::from_millis(100), subscriber.recv())
        .await
        .unwrap()
        .unwrap();
    let event2 = tokio::time::timeout(std::time::Duration::from_millis(100), subscriber.recv())
        .await
        .unwrap()
        .unwrap();

    // Events might also include Filled events since status is "Filled",
    // but we verify at least the first 2 are Placed events
    match (&event1, &event2) {
        (
            Event::OrderUpdate(OrderEvent::Placed(r1)),
            Event::OrderUpdate(OrderEvent::Placed(r2)),
        ) => {
            assert!(r1.success);
            assert!(r2.success);
            // Both should have same token_ids from the batch (order may vary due to sequential processing)
            let tokens: Vec<_> = vec![r1.token_id.clone(), r2.token_id.clone()];
            assert!(tokens.contains(&"token_a".to_string()));
            assert!(tokens.contains(&"token_b".to_string()));
        }
        _ => panic!(
            "Expected two Placed events, got {:?} and {:?}",
            event1, event2
        ),
    }
}
