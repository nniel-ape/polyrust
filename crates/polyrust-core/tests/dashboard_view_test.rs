use async_trait::async_trait;
use polyrust_core::context::StrategyContext;
use polyrust_core::dashboard_view::DashboardViewProvider;
use polyrust_core::error::Result;
use polyrust_core::events::Event;
use polyrust_core::strategy::Strategy;

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

    fn render_view(&self) -> Result<String> {
        Ok("<div>Hello from ViewStrategy</div>".to_string())
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

#[test]
fn render_view_returns_html_fragment() {
    let strategy = ViewStrategy;
    let provider = strategy.dashboard_view().unwrap();
    let html = provider.render_view().unwrap();
    assert!(html.contains("<div>"));
    assert!(html.contains("Hello from ViewStrategy"));
}
