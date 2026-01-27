use std::convert::Infallible;
use std::time::Duration;

use askama::Template;
use axum::extract::State;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use polyrust_core::prelude::*;
use rust_decimal::Decimal;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::server::AppState;

// ---------------------------------------------------------------------------
// Error wrapper
// ---------------------------------------------------------------------------

pub struct AppError(String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("<h1>Error</h1><pre>{}</pre>", self.0)),
        )
            .into_response()
    }
}

impl From<askama::Error> for AppError {
    fn from(err: askama::Error) -> Self {
        AppError(err.to_string())
    }
}

fn pnl_class(d: Decimal) -> &'static str {
    if d.is_sign_negative() {
        "pnl-negative"
    } else {
        "pnl-positive"
    }
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    strategy_count: usize,
    position_count: usize,
    order_count: usize,
    total_unrealized_pnl: String,
    pnl_class: String,
    available_usdc: String,
    locked_usdc: String,
}

#[derive(Template)]
#[template(path = "positions.html")]
struct PositionsTemplate {
    positions: Vec<PositionRow>,
}

struct PositionRow {
    id_short: String,
    market_id_short: String,
    side: String,
    entry_price: String,
    current_price: String,
    size: String,
    unrealized_pnl: String,
    pnl_class: String,
    strategy_name: String,
}

#[derive(Template)]
#[template(path = "trades.html")]
struct TradesTemplate {
    trades: Vec<TradeRow>,
}

struct TradeRow {
    id_short: String,
    market_id_short: String,
    side: String,
    price: String,
    size: String,
    realized_pnl: String,
    strategy_name: String,
    timestamp: String,
}

#[derive(Template)]
#[template(path = "health.html")]
struct HealthTemplate {
    subscriber_count: usize,
    position_count: usize,
    order_count: usize,
    available_usdc: String,
}

#[derive(Template)]
#[template(path = "partials/pnl_summary.html")]
struct PnlSummaryPartial {
    total_unrealized_pnl: String,
    pnl_class: String,
    available_usdc: String,
}

fn short_id(s: &str, len: usize) -> String {
    if s.len() > len {
        format!("{}...", &s[..len])
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET / — overview page
pub async fn index(
    State(state): State<AppState>,
) -> std::result::Result<Html<String>, AppError> {
    let pos_state = state.context.positions.read().await;
    let bal_state = state.context.balance.read().await;
    let pnl = pos_state.total_unrealized_pnl();

    let tmpl = IndexTemplate {
        strategy_count: 0,
        position_count: pos_state.position_count(),
        order_count: pos_state.open_orders.len(),
        total_unrealized_pnl: pnl.to_string(),
        pnl_class: pnl_class(pnl).to_string(),
        available_usdc: bal_state.available_usdc.to_string(),
        locked_usdc: bal_state.locked_usdc.to_string(),
    };
    Ok(Html(tmpl.render()?))
}

/// GET /positions — open positions table
pub async fn positions(
    State(state): State<AppState>,
) -> std::result::Result<Html<String>, AppError> {
    let pos_state = state.context.positions.read().await;

    let rows: Vec<PositionRow> = pos_state
        .open_positions
        .values()
        .map(|p| {
            let pnl = p.unrealized_pnl();
            PositionRow {
                id_short: short_id(&p.id.to_string(), 8),
                market_id_short: short_id(&p.market_id, 12),
                side: format!("{:?}", p.side),
                entry_price: p.entry_price.to_string(),
                current_price: p.current_price.to_string(),
                size: p.size.to_string(),
                unrealized_pnl: pnl.to_string(),
                pnl_class: pnl_class(pnl).to_string(),
                strategy_name: p.strategy_name.clone(),
            }
        })
        .collect();

    let tmpl = PositionsTemplate { positions: rows };
    Ok(Html(tmpl.render()?))
}

/// GET /trades — recent trade history
pub async fn trades(
    State(state): State<AppState>,
) -> std::result::Result<Html<String>, AppError> {
    let trade_list = state
        .store
        .list_trades(None, 50)
        .await
        .unwrap_or_default();

    let rows: Vec<TradeRow> = trade_list
        .iter()
        .map(|t| TradeRow {
            id_short: short_id(&t.id.to_string(), 8),
            market_id_short: short_id(&t.market_id, 12),
            side: format!("{:?}", t.side),
            price: t.price.to_string(),
            size: t.size.to_string(),
            realized_pnl: t
                .realized_pnl
                .map(|d| d.to_string())
                .unwrap_or_else(|| "—".into()),
            strategy_name: t.strategy_name.clone(),
            timestamp: t.timestamp.format("%Y-%m-%d %H:%M:%S").to_string(),
        })
        .collect();

    let tmpl = TradesTemplate { trades: rows };
    Ok(Html(tmpl.render()?))
}

/// GET /health — system health
pub async fn health(
    State(state): State<AppState>,
) -> std::result::Result<Html<String>, AppError> {
    let pos_state = state.context.positions.read().await;
    let bal_state = state.context.balance.read().await;

    let tmpl = HealthTemplate {
        subscriber_count: state.event_bus.subscriber_count(),
        position_count: pos_state.position_count(),
        order_count: pos_state.open_orders.len(),
        available_usdc: bal_state.available_usdc.to_string(),
    };
    Ok(Html(tmpl.render()?))
}

/// GET /events/stream — SSE endpoint for real-time HTMX updates
pub async fn sse_events(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = std::result::Result<SseEvent, Infallible>>> {
    let subscriber = state.event_bus.subscribe();
    let rx = subscriber.into_receiver();
    let ctx = state.context.clone();

    let stream = BroadcastStream::new(rx).filter_map(move |result| {
        let event = match result {
            Ok(e) => e,
            Err(_) => return None,
        };
        let topic = event.topic().to_string();

        // For position changes, render the PnL partial for HTMX swap
        if let Event::PositionChange(_) = &event
            && let Ok(pos_state) = ctx.positions.try_read()
        {
            let pnl = pos_state.total_unrealized_pnl();
            if let Ok(bal_state) = ctx.balance.try_read() {
                let partial = PnlSummaryPartial {
                    total_unrealized_pnl: pnl.to_string(),
                    pnl_class: pnl_class(pnl).to_string(),
                    available_usdc: bal_state.available_usdc.to_string(),
                };
                if let Ok(html) = partial.render() {
                    return Some(Ok(SseEvent::default().event("pnl-update").data(html)));
                }
            }
        }

        // Default: send JSON event data
        let data = serde_json::to_string(&event).unwrap_or_default();
        Some(Ok(SseEvent::default().event(topic).data(data)))
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}
