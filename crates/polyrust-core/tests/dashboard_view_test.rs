use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;
use polyrust_core::context::StrategyContext;
use polyrust_core::dashboard_view::DashboardViewProvider;
use polyrust_core::engine::Engine;
use polyrust_core::error::Result;
use polyrust_core::events::Event;
use polyrust_core::execution::ExecutionBackend;
use polyrust_core::strategy::Strategy;
use polyrust_core::types::*;
use rust_decimal::Decimal;

// --- Strategy without a dashboard view (default) ---

struct PlainStrategy;

#[async_trait]
impl Strategy for PlainStrategy {
    fn name(&self) -> &str {
        "plain"
    }

    fn description(&self) -> &str {
        "A strategy with no custom dashboard view"
    }

    async fn on_event(
        &mut self,
        _event: &Event,
        _ctx: &StrategyContext,
    ) -> Result<Vec<polyrust_core::actions::Action>> {
        Ok(vec![])
    }
}

// --- Strategy with a dashboard view ---

struct ViewStrategy;

impl DashboardViewProvider for ViewStrategy {
    fn view_name(&self) -> &str {
        "my-view"
    }

    fn render_view(&self) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async { Ok("<div>Hello from ViewStrategy</div>".to_string()) })
    }
}

#[async_trait]
impl Strategy for ViewStrategy {
    fn name(&self) -> &str {
        "view-strategy"
    }

    fn description(&self) -> &str {
        "A strategy with a custom dashboard view"
    }

    async fn on_event(
        &mut self,
        _event: &Event,
        _ctx: &StrategyContext,
    ) -> Result<Vec<polyrust_core::actions::Action>> {
        Ok(vec![])
    }

    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        Some(self)
    }
}

#[test]
fn strategy_without_view_returns_none() {
    let strategy = PlainStrategy;
    assert!(strategy.dashboard_view().is_none());
}

#[test]
fn strategy_with_view_returns_some() {
    let strategy = ViewStrategy;
    let view = strategy.dashboard_view();
    assert!(view.is_some());

    let provider = view.unwrap();
    assert_eq!(provider.view_name(), "my-view");
}

#[tokio::test]
async fn render_view_returns_html_fragment() {
    let strategy = ViewStrategy;
    let provider = strategy.dashboard_view().unwrap();
    let html = provider.render_view().await.unwrap();
    assert!(html.contains("<div>"));
    assert!(html.contains("Hello from ViewStrategy"));
}

// --- Mock execution backend for engine tests ---

struct MockBackend;

#[async_trait]
impl ExecutionBackend for MockBackend {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        Ok(OrderResult {
            success: true,
            order_id: Some("mock-1".to_string()),
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
        Ok(Decimal::ZERO)
    }
}

// --- Second view strategy for multi-strategy tests ---

struct AnotherViewStrategy;

impl DashboardViewProvider for AnotherViewStrategy {
    fn view_name(&self) -> &str {
        "another-view"
    }
    fn render_view(&self) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async { Ok("<div>Another view</div>".to_string()) })
    }
}

#[async_trait]
impl Strategy for AnotherViewStrategy {
    fn name(&self) -> &str {
        "another-strategy"
    }
    fn description(&self) -> &str {
        "Another strategy with a view"
    }
    async fn on_event(
        &mut self,
        _event: &Event,
        _ctx: &StrategyContext,
    ) -> Result<Vec<polyrust_core::actions::Action>> {
        Ok(vec![])
    }
    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        Some(self)
    }
}

// --- strategy_views tests ---

#[tokio::test]
async fn strategy_context_new_has_empty_views() {
    let ctx = StrategyContext::new();
    let views = ctx.strategy_views.read().await;
    assert!(views.is_empty());
}

#[tokio::test]
async fn strategy_names_returns_empty_for_no_views() {
    let ctx = StrategyContext::new();
    let names = ctx.strategy_names().await;
    assert!(names.is_empty());
}

#[tokio::test]
async fn engine_build_registers_strategy_views() {
    let engine = Engine::builder()
        .execution(MockBackend)
        .strategy(ViewStrategy)
        .strategy(PlainStrategy)
        .build()
        .await
        .unwrap();

    let ctx = engine.context();
    let views = ctx.strategy_views.read().await;
    assert_eq!(views.len(), 1);
    assert!(views.contains_key("my-view"));
}

#[tokio::test]
async fn engine_build_registers_multiple_strategy_views() {
    let engine = Engine::builder()
        .execution(MockBackend)
        .strategy(ViewStrategy)
        .strategy(AnotherViewStrategy)
        .strategy(PlainStrategy)
        .build()
        .await
        .unwrap();

    let ctx = engine.context();
    let names = ctx.strategy_names().await;
    assert_eq!(names, vec!["another-view", "my-view"]);
}

#[tokio::test]
async fn strategy_views_render_through_context() {
    let engine = Engine::builder()
        .execution(MockBackend)
        .strategy(ViewStrategy)
        .build()
        .await
        .unwrap();

    let ctx = engine.context();
    let views = ctx.strategy_views.read().await;
    let strategy_arc = views.get("my-view").unwrap();
    let strategy = strategy_arc.read().await;
    let provider = strategy.dashboard_view().unwrap();
    let html = provider.render_view().await.unwrap();
    assert!(html.contains("Hello from ViewStrategy"));
}

#[tokio::test]
async fn strategy_views_lookup_missing_returns_none() {
    let engine = Engine::builder()
        .execution(MockBackend)
        .strategy(PlainStrategy)
        .build()
        .await
        .unwrap();

    let ctx = engine.context();
    let views = ctx.strategy_views.read().await;
    assert!(views.get("nonexistent").is_none());
}
