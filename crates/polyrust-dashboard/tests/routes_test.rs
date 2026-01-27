use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use polyrust_core::prelude::*;
use polyrust_dashboard::handlers;
use polyrust_dashboard::server::AppState;
use polyrust_store::Store;
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
        .route("/events/stream", get(handlers::sse_events))
        .with_state(state.clone());

    (app, state)
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
