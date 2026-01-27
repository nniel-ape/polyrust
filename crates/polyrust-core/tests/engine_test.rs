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
use std::io::Write;
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
    assert!(!config.paper.enabled);
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

    assert_eq!(
        config.polymarket.private_key.as_deref(),
        Some("0xdeadbeef")
    );
    assert_eq!(config.polymarket.safe_address.as_deref(), Some("0xsafe"));
    assert_eq!(
        config.polymarket.builder_api_key.as_deref(),
        Some("key123")
    );
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
    assert!(!config.paper.enabled);
}
