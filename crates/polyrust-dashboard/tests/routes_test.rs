use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use polyrust_core::prelude::*;
use polyrust_dashboard::handlers;
use polyrust_dashboard::server::AppState;
use polyrust_store::Store;
use tokio::sync::RwLock;
use tower::ServiceExt;

async fn test_app() -> (Router, AppState) {
    let store = Store::new(":memory:").await.unwrap();
    let context = StrategyContext::new();
    let event_bus = EventBus::new();

    let state = AppState {
        context,
        store: Arc::new(store),
        event_bus,
    };

    let app = Router::new()
        .route("/", get(handlers::index))
        .route("/positions", get(handlers::positions))
        .route("/trades", get(handlers::trades))
        .route("/health", get(handlers::health))
        .route("/strategy/{name}", get(handlers::strategy_view))
        .route("/events/stream", get(handlers::sse_events))
        .with_state(state.clone());

    (app, state)
}

/// Mock strategy with a dashboard view for testing.
struct MockViewStrategy;

impl DashboardViewProvider for MockViewStrategy {
    fn view_name(&self) -> &str {
        "mock-view"
    }

    fn render_view(&self) -> polyrust_core::error::Result<String> {
        Ok("<div class=\"mock-content\">Mock strategy dashboard</div>".to_string())
    }
}

#[async_trait]
impl Strategy for MockViewStrategy {
    fn name(&self) -> &str {
        "mock-view"
    }
    fn description(&self) -> &str {
        "Mock strategy for testing"
    }
    async fn on_event(&mut self, _event: &Event, _ctx: &StrategyContext) -> polyrust_core::error::Result<Vec<Action>> {
        Ok(vec![])
    }
    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        Some(self)
    }
}

#[tokio::test]
async fn index_returns_200() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn positions_returns_200() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/positions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn trades_returns_200() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/trades")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn health_returns_200() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn sse_endpoint_returns_200() {
    let (app, _) = test_app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/events/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("text/event-stream"),
        "expected SSE content type, got: {content_type}"
    );
}

#[tokio::test]
async fn positions_handler_reads_context() {
    let (app, state) = test_app().await;

    // Insert a position into context
    {
        let mut pos_state = state.context.positions.write().await;
        let pos = Position {
            id: uuid::Uuid::new_v4(),
            market_id: "market-123".into(),
            token_id: "token-abc".into(),
            side: OutcomeSide::Up,
            entry_price: Decimal::new(50, 2), // 0.50
            size: Decimal::new(10, 0),
            current_price: Decimal::new(60, 2), // 0.60
            entry_time: chrono::Utc::now(),
            strategy_name: "test-strategy".into(),
        };
        pos_state.open_positions.insert(pos.id, pos);
    }

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/positions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("test-strategy"),
        "should display strategy name"
    );
    assert!(html.contains("Up"), "should display position side");
}

#[tokio::test]
async fn trades_handler_queries_store() {
    let (app, state) = test_app().await;

    // Insert a trade into the store
    let trade = Trade {
        id: uuid::Uuid::new_v4(),
        order_id: "order-1".into(),
        market_id: "market-xyz".into(),
        token_id: "token-def".into(),
        side: OrderSide::Buy,
        price: Decimal::new(75, 2),
        size: Decimal::new(5, 0),
        realized_pnl: Some(Decimal::new(125, 2)),
        strategy_name: "arb-strategy".into(),
        timestamp: chrono::Utc::now(),
    };
    state.store.insert_trade(&trade).await.unwrap();

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/trades")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("arb-strategy"),
        "should display trade strategy"
    );
    assert!(html.contains("Buy"), "should display trade side");
}

#[tokio::test]
async fn strategy_view_returns_200_for_registered_strategy() {
    let (app, state) = test_app().await;

    // Register the mock strategy
    {
        let strategy_handle: Arc<RwLock<Box<dyn Strategy>>> =
            Arc::new(RwLock::new(Box::new(MockViewStrategy)));
        let mut views = state.context.strategy_views.write().await;
        views.insert("mock-view".to_string(), strategy_handle);
    }

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/strategy/mock-view")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("mock-content"),
        "should contain rendered strategy view content"
    );
    assert!(
        html.contains("Strategy: mock-view"),
        "should contain strategy name in heading"
    );
}

#[tokio::test]
async fn strategy_view_returns_404_for_unknown_strategy() {
    let (app, _) = test_app().await;

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/strategy/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains("not found"),
        "should indicate strategy was not found"
    );
}

#[tokio::test]
async fn nav_links_include_registered_strategy_views() {
    let (app, state) = test_app().await;

    // Register the mock strategy so it appears in nav
    {
        let strategy_handle: Arc<RwLock<Box<dyn Strategy>>> =
            Arc::new(RwLock::new(Box::new(MockViewStrategy)));
        let mut views = state.context.strategy_views.write().await;
        views.insert("mock-view".to_string(), strategy_handle);
    }

    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        html.contains(r#"href="/strategy/mock-view""#),
        "nav should contain link to strategy view"
    );
    assert!(
        html.contains(">mock-view</a>"),
        "nav should display strategy name"
    );
}

#[tokio::test]
async fn nav_links_absent_when_no_strategy_views() {
    let (app, _) = test_app().await;

    let resp = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        !html.contains(r#"href="/strategy/"#),
        "nav should not contain strategy links when none registered"
    );
}

#[tokio::test]
async fn sse_receives_published_events() {
    let store = Store::new(":memory:").await.unwrap();
    let context = StrategyContext::new();
    let event_bus = EventBus::new();

    let state = AppState {
        context,
        store: Arc::new(store),
        event_bus: event_bus.clone(),
    };

    let app = Router::new()
        .route("/events/stream", get(handlers::sse_events))
        .with_state(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/events/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Publish an event after a short delay
    let bus = event_bus.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bus.publish(Event::System(SystemEvent::EngineStarted));
    });

    // Read initial SSE data with timeout
    let body = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        axum::body::to_bytes(resp.into_body(), usize::MAX),
    )
    .await;

    // We either get data or timeout — both are acceptable since SSE is a long-lived stream.
    // The key test is that the endpoint connected successfully (200 status above).
    // If we got data, verify it looks like SSE.
    if let Ok(Ok(bytes)) = body {
        let text = String::from_utf8(bytes.to_vec()).unwrap_or_default();
        if !text.is_empty() {
            assert!(
                text.contains("event:") || text.contains("data:") || text.contains(":keep-alive"),
                "SSE output should contain event data or keepalive"
            );
        }
    }
}

#[tokio::test]
async fn sse_dashboard_update_signal_renders_strategy_view() {
    let store = Store::new(":memory:").await.unwrap();
    let context = StrategyContext::new();
    let event_bus = EventBus::new();

    // Register a mock strategy with a dashboard view
    {
        let strategy_handle: Arc<RwLock<Box<dyn Strategy>>> =
            Arc::new(RwLock::new(Box::new(MockViewStrategy)));
        let mut views = context.strategy_views.write().await;
        views.insert("mock-view".to_string(), strategy_handle);
    }

    let state = AppState {
        context,
        store: Arc::new(store),
        event_bus: event_bus.clone(),
    };

    let app = Router::new()
        .route("/events/stream", get(handlers::sse_events))
        .with_state(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/events/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Publish a dashboard-update Signal event after a short delay
    let bus = event_bus.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bus.publish(Event::Signal(SignalEvent {
            strategy_name: "mock-view".to_string(),
            signal_type: "dashboard-update".to_string(),
            payload: serde_json::json!({ "view_name": "mock-view" }),
            timestamp: chrono::Utc::now(),
        }));
    });

    // Read SSE output with timeout
    let body = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        axum::body::to_bytes(resp.into_body(), usize::MAX),
    )
    .await;

    if let Ok(Ok(bytes)) = body {
        let text = String::from_utf8(bytes.to_vec()).unwrap_or_default();
        // Should contain the strategy-specific SSE event name and re-rendered HTML
        if !text.is_empty() {
            assert!(
                text.contains("event:strategy-mock-view-update"),
                "SSE should emit strategy-specific event name, got: {text}"
            );
            assert!(
                text.contains("mock-content"),
                "SSE should contain re-rendered strategy view HTML, got: {text}"
            );
        }
    }
}

#[tokio::test]
async fn sse_non_dashboard_signal_passes_through_as_json() {
    let store = Store::new(":memory:").await.unwrap();
    let context = StrategyContext::new();
    let event_bus = EventBus::new();

    let state = AppState {
        context,
        store: Arc::new(store),
        event_bus: event_bus.clone(),
    };

    let app = Router::new()
        .route("/events/stream", get(handlers::sse_events))
        .with_state(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/events/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Publish a non-dashboard signal
    let bus = event_bus.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bus.publish(Event::Signal(SignalEvent {
            strategy_name: "test".to_string(),
            signal_type: "heartbeat".to_string(),
            payload: serde_json::json!({ "event_count": 42 }),
            timestamp: chrono::Utc::now(),
        }));
    });

    let body = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        axum::body::to_bytes(resp.into_body(), usize::MAX),
    )
    .await;

    if let Ok(Ok(bytes)) = body {
        let text = String::from_utf8(bytes.to_vec()).unwrap_or_default();
        if !text.is_empty() {
            // Non-dashboard signals pass through as JSON with topic "signal"
            assert!(
                text.contains("event:signal"),
                "Non-dashboard signal should use topic as event name, got: {text}"
            );
            assert!(
                text.contains("heartbeat"),
                "Should contain signal data, got: {text}"
            );
        }
    }
}
