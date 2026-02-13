use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use chrono::{DateTime, Utc};
use polyrust_core::prelude::*;
use polyrust_store::Store;
use tracing::{info, warn};

use crate::handlers;

/// Shared application state for Axum handlers.
#[derive(Clone)]
pub struct AppState {
    pub context: StrategyContext,
    pub store: Arc<Store>,
    pub event_bus: EventBus,
    pub engine_started_at: DateTime<Utc>,
}

/// Axum + HTMX monitoring dashboard.
pub struct Dashboard {
    context: StrategyContext,
    store: Arc<Store>,
    event_bus: EventBus,
    engine_started_at: DateTime<Utc>,
}

impl Dashboard {
    pub fn new(
        context: StrategyContext,
        store: Arc<Store>,
        event_bus: EventBus,
        engine_started_at: DateTime<Utc>,
    ) -> Self {
        Self {
            context,
            store,
            event_bus,
            engine_started_at,
        }
    }

    /// Start serving the dashboard on the given host and port.
    pub async fn serve(self, host: &str, port: u16) -> Result<()> {
        let state = AppState {
            context: self.context,
            store: self.store,
            event_bus: self.event_bus,
            engine_started_at: self.engine_started_at,
        };

        let app = Router::new()
            .route("/", get(handlers::index))
            .route("/positions", get(handlers::positions))
            .route("/trades", get(handlers::trades))
            .route("/strategy/{name}", get(handlers::strategy_view))
            .route("/events/stream", get(handlers::sse_events))
            .with_state(state);

        let addr = format!("{host}:{port}");
        if host != "127.0.0.1" && host != "localhost" && host != "::1" {
            warn!(
                host = %host,
                "dashboard binding to non-localhost address without authentication; \
                 trading data will be accessible to anyone on the network"
            );
        }
        info!("dashboard listening on http://{addr}");
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .map_err(|e| PolyError::Other(anyhow::anyhow!(e)))?;
        axum::serve(listener, app)
            .await
            .map_err(|e| PolyError::Other(anyhow::anyhow!(e)))?;
        Ok(())
    }
}
