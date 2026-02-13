use std::convert::Infallible;
use std::time::Duration;

use askama::Template;
use askama::filters::{Html as HtmlEscaper, escape};
use axum::extract::{Path, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use chrono::Utc;
use polyrust_core::prelude::*;
use rust_decimal::Decimal;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::fmt;
use crate::server::AppState;

// ---------------------------------------------------------------------------
// Error wrapper
// ---------------------------------------------------------------------------

pub struct AppError(String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let escaped = match escape(&self.0, HtmlEscaper) {
            Ok(s) => s.to_string(),
            Err(_) => "Error message could not be displayed".to_string(),
        };
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("<h1>Error</h1><pre>{}</pre>", escaped)),
        )
            .into_response()
    }
}

impl From<askama::Error> for AppError {
    fn from(err: askama::Error) -> Self {
        AppError(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    // SSE-updated row
    total_unrealized_pnl: String,
    unrealized_pnl_class: String,
    available_usdc: String,
    // Static P&L row
    total_pnl: String,
    total_pnl_class: String,
    realized_pnl: String,
    realized_pnl_class: String,
    total_fees: String,
    // Status line
    trade_count: i64,
    position_count: usize,
    order_count: usize,
    locked_usdc: String,
    uptime: String,
    subscriber_count: usize,
    // Nav
    strategy_names: Vec<String>,
}

#[derive(Template)]
#[template(path = "positions.html")]
struct PositionsTemplate {
    positions: Vec<PositionRow>,
    total_unrealized_pnl: String,
    total_unrealized_pnl_class: String,
    position_count: usize,
    strategy_names: Vec<String>,
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
    entry_time: String,
    age: String,
}

#[derive(Template)]
#[template(path = "trades.html")]
struct TradesTemplate {
    trades: Vec<TradeRow>,
    trade_count: i64,
    total_realized_pnl: String,
    total_realized_pnl_class: String,
    total_fees: String,
    strategy_names: Vec<String>,
}

struct TradeRow {
    id_short: String,
    market_id_short: String,
    side: String,
    order_type: String,
    price: String,
    entry_price: String,
    size: String,
    fee: String,
    realized_pnl: String,
    pnl_class: String,
    close_reason: String,
    strategy_name: String,
    timestamp: String,
}

#[derive(Template)]
#[template(path = "partials/pnl_summary.html")]
struct PnlSummaryPartial {
    total_unrealized_pnl: String,
    pnl_class: String,
    available_usdc: String,
}

#[derive(Template)]
#[template(path = "strategy_view.html")]
struct StrategyViewTemplate {
    strategy_name: String,
    content_html: String,
    strategy_names: Vec<String>,
}

fn short_id(s: &str, len: usize) -> String {
    let char_count = s.chars().count();
    if char_count > len {
        let truncated: String = s.chars().take(len).collect();
        format!("{truncated}...")
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET / — overview page
pub async fn index(State(state): State<AppState>) -> std::result::Result<Html<String>, AppError> {
    let (position_count, order_count, unrealized_pnl) = {
        let pos_state = state.context.positions.read().await;
        (
            pos_state.position_count(),
            pos_state.open_orders.len(),
            pos_state.total_unrealized_pnl(),
        )
    };
    let (available_usdc, locked_usdc) = {
        let bal_state = state.context.balance.read().await;
        (bal_state.available_usdc, bal_state.locked_usdc)
    };

    let realized_pnl = state.store.sum_realized_pnl(None).await.unwrap_or(Decimal::ZERO);
    let total_fees = state.store.sum_fees(None).await.unwrap_or(Decimal::ZERO);
    let trade_count = state.store.count_trades(None).await.unwrap_or(0);

    let total_pnl = realized_pnl + unrealized_pnl;
    let uptime = fmt::fmt_duration(Utc::now() - state.engine_started_at);

    let (unrealized_str, unrealized_class) = fmt::fmt_pnl(unrealized_pnl);
    let (total_pnl_str, total_pnl_class) = fmt::fmt_pnl(total_pnl);
    let (realized_str, realized_class) = fmt::fmt_pnl(realized_pnl);

    let strategy_names = state.context.strategy_names().await;

    let tmpl = IndexTemplate {
        total_unrealized_pnl: unrealized_str,
        unrealized_pnl_class: unrealized_class.to_string(),
        available_usdc: fmt::fmt_usdc(available_usdc),
        total_pnl: total_pnl_str,
        total_pnl_class: total_pnl_class.to_string(),
        realized_pnl: realized_str,
        realized_pnl_class: realized_class.to_string(),
        total_fees: fmt::fmt_usdc(total_fees),
        trade_count,
        position_count,
        order_count,
        locked_usdc: fmt::fmt_usdc(locked_usdc),
        uptime,
        subscriber_count: state.event_bus.subscriber_count(),
        strategy_names,
    };
    Ok(Html(tmpl.render()?))
}

/// GET /positions — open positions table
pub async fn positions(
    State(state): State<AppState>,
) -> std::result::Result<Html<String>, AppError> {
    let (rows, total_unrealized_pnl) = {
        let pos_state = state.context.positions.read().await;
        let total_pnl = pos_state.total_unrealized_pnl();
        let now = Utc::now();
        let mut positions: Vec<PositionRow> = pos_state
            .open_positions
            .values()
            .map(|p| {
                let pnl = p.unrealized_pnl();
                let (pnl_str, pnl_cls) = fmt::fmt_pnl(pnl);
                PositionRow {
                    id_short: short_id(&p.id.to_string(), 8),
                    market_id_short: short_id(&p.market_id, 12),
                    side: format!("{:?}", p.side),
                    entry_price: fmt::fmt_price(p.entry_price),
                    current_price: fmt::fmt_price(p.current_price),
                    size: fmt::fmt_size(p.size),
                    unrealized_pnl: pnl_str,
                    pnl_class: pnl_cls.to_string(),
                    strategy_name: p.strategy_name.clone(),
                    entry_time: p.entry_time.format("%H:%M:%S").to_string(),
                    age: fmt::fmt_duration(now - p.entry_time),
                }
            })
            .collect();
        // Sort by entry_time descending (newest first) — use the raw entry_time string
        positions.sort_by(|a, b| b.entry_time.cmp(&a.entry_time));
        (positions, total_pnl)
    };

    let position_count = rows.len();
    let (total_pnl_str, total_pnl_class) = fmt::fmt_pnl(total_unrealized_pnl);
    let strategy_names = state.context.strategy_names().await;

    let tmpl = PositionsTemplate {
        positions: rows,
        total_unrealized_pnl: total_pnl_str,
        total_unrealized_pnl_class: total_pnl_class.to_string(),
        position_count,
        strategy_names,
    };
    Ok(Html(tmpl.render()?))
}

/// GET /trades — recent trade history
pub async fn trades(State(state): State<AppState>) -> std::result::Result<Html<String>, AppError> {
    let trade_list = state.store.list_trades(None, 50).await.unwrap_or_default();

    let trade_count = state.store.count_trades(None).await.unwrap_or(0);
    let realized_pnl = state.store.sum_realized_pnl(None).await.unwrap_or(Decimal::ZERO);
    let total_fees = state.store.sum_fees(None).await.unwrap_or(Decimal::ZERO);

    let (realized_str, realized_class) = fmt::fmt_pnl(realized_pnl);
    let strategy_names = state.context.strategy_names().await;

    let rows: Vec<TradeRow> = trade_list
        .iter()
        .map(|t| {
            let (pnl_str, pnl_cls) = t
                .realized_pnl
                .map(fmt::fmt_pnl)
                .unwrap_or_else(|| ("\u{2014}".into(), ""));
            TradeRow {
                id_short: short_id(&t.id.to_string(), 8),
                market_id_short: short_id(&t.market_id, 12),
                side: format!("{:?}", t.side),
                order_type: t.order_type.clone().unwrap_or_else(|| "\u{2014}".into()),
                price: fmt::fmt_price(t.price),
                entry_price: t
                    .entry_price
                    .map(fmt::fmt_price)
                    .unwrap_or_else(|| "\u{2014}".into()),
                size: fmt::fmt_size(t.size),
                fee: t
                    .fee
                    .map(fmt::fmt_usdc)
                    .unwrap_or_else(|| "\u{2014}".into()),
                realized_pnl: pnl_str,
                pnl_class: pnl_cls.to_string(),
                close_reason: t.close_reason.clone().unwrap_or_else(|| "\u{2014}".into()),
                strategy_name: t.strategy_name.clone(),
                timestamp: t.timestamp.format("%Y-%m-%d %H:%M:%S").to_string(),
            }
        })
        .collect();

    let tmpl = TradesTemplate {
        trades: rows,
        trade_count,
        total_realized_pnl: realized_str,
        total_realized_pnl_class: realized_class.to_string(),
        total_fees: fmt::fmt_usdc(total_fees),
        strategy_names,
    };
    Ok(Html(tmpl.render()?))
}

/// GET /strategy/:name — per-strategy dashboard view
pub async fn strategy_view(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> std::result::Result<Response, AppError> {
    let views = state.context.strategy_views.read().await;
    let Some(strategy_handle) = views.get(&name) else {
        let escaped_name = name
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        return Ok((
            axum::http::StatusCode::NOT_FOUND,
            Html(format!("<h1>Strategy '{}' not found</h1>", escaped_name)),
        )
            .into_response());
    };

    let strategy = strategy_handle.read().await;
    let provider = strategy
        .dashboard_view()
        .ok_or_else(|| AppError(format!("Strategy '{}' has no dashboard view", name)))?;

    let content_html = provider
        .render_view()
        .await
        .map_err(|e| AppError(e.to_string()))?;
    drop(strategy);
    drop(views);
    let strategy_names = state.context.strategy_names().await;
    let tmpl = StrategyViewTemplate {
        strategy_name: name,
        content_html,
        strategy_names,
    };
    Ok(Html(tmpl.render()?).into_response())
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
                let (pnl_str, pnl_cls) = fmt::fmt_pnl(pnl);
                let partial = PnlSummaryPartial {
                    total_unrealized_pnl: pnl_str,
                    pnl_class: pnl_cls.to_string(),
                    available_usdc: fmt::fmt_usdc(bal_state.available_usdc),
                };
                if let Ok(html) = partial.render() {
                    return Some(Ok(SseEvent::default().event("pnl-update").data(html)));
                }
            }
        }

        // For dashboard-update signals, use pre-rendered HTML from the payload.
        if let Event::Signal(signal) = &event
            && signal.signal_type == "dashboard-update"
        {
            let view_name = signal.payload.get("view_name").and_then(|v| v.as_str())?;
            let html = signal
                .payload
                .get("rendered_html")
                .and_then(|v| v.as_str())?;
            let event_name = format!("strategy-{view_name}-update");
            return Some(Ok(SseEvent::default().event(event_name).data(html)));
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
