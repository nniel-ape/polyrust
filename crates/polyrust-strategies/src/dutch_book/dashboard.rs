//! Dashboard view for the Dutch Book arbitrage strategy.
//!
//! Renders at `/strategy/dutch-book` with sections for summary stats,
//! active positions, recent opportunities, and execution status.

use std::fmt::Write as FmtWrite;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rust_decimal::Decimal;
use tokio::sync::RwLock;

use polyrust_core::prelude::*;

/// Safely truncate a string ID for display, avoiding panics on non-ASCII boundaries.
fn truncate_id(id: &str, max_len: usize) -> String {
    if id.len() > max_len {
        let truncated: String = id.chars().take(max_len).collect();
        format!("{truncated}...")
    } else {
        id.to_string()
    }
}

use crate::shared::escape_html;

use super::types::{DutchBookState, ExecutionState};

/// Dashboard view for the Dutch Book arbitrage strategy.
pub struct DutchBookDashboard {
    state: Arc<RwLock<DutchBookState>>,
}

impl DutchBookDashboard {
    pub fn new(state: Arc<RwLock<DutchBookState>>) -> Self {
        Self { state }
    }
}

impl DashboardViewProvider for DutchBookDashboard {
    fn view_name(&self) -> &str {
        "dutch-book"
    }

    fn render_view(
        &self,
    ) -> Pin<Box<dyn Future<Output = polyrust_core::error::Result<String>> + Send + '_>> {
        Box::pin(self.render_view_impl())
    }
}

impl DutchBookDashboard {
    async fn render_view_impl(&self) -> polyrust_core::error::Result<String> {
        let mut html = String::with_capacity(4096);
        let state = self.state.read().await;

        // --- Summary ---
        render_summary(&state, &mut html);

        // --- Active Positions ---
        render_positions(&state, &mut html);

        // --- Recent Opportunities ---
        render_opportunities(&state, &mut html);

        // --- Execution Status ---
        render_executions(&state, &mut html);

        Ok(html)
    }
}

/// Render summary card with key metrics.
fn render_summary(state: &DutchBookState, html: &mut String) {
    let total_expected: Decimal = state.positions.iter().map(|p| p.expected_profit).sum();
    let total_cost: Decimal = state.positions.iter().map(|p| p.combined_cost).sum();

    html.push_str(r#"<div class="bp-card mb-4">"#);
    html.push_str(r#"<h2 class="bp-section-title">Dutch Book Summary</h2>"#);
    html.push_str(r#"<div class="grid grid-cols-2 gap-2" style="font-size:0.95rem">"#);

    let _ = write!(
        html,
        r#"<div class="bp-config-label">Markets monitored:</div><div class="bp-config-value">{}</div>"#,
        state.tracked_markets
    );
    let _ = write!(
        html,
        r#"<div class="bp-config-label">Active positions:</div><div class="bp-config-value">{}</div>"#,
        state.positions.len()
    );
    let _ = write!(
        html,
        r#"<div class="bp-config-label">Active executions:</div><div class="bp-config-value">{}</div>"#,
        state.executions.len()
    );
    let _ = write!(
        html,
        r#"<div class="bp-config-label">Opportunities detected:</div><div class="bp-config-value">{}</div>"#,
        state.total_opportunities
    );

    let pnl_class = if state.total_realized_pnl >= Decimal::ZERO {
        "bp-profit"
    } else {
        "bp-loss"
    };
    let _ = write!(
        html,
        r#"<div class="bp-config-label" title="Includes pending redemptions">Realized P&amp;L:</div><div class="bp-config-value {pnl_class}">${:.4}</div>"#,
        state.total_realized_pnl,
    );
    let _ = write!(
        html,
        r#"<div class="bp-config-label">Expected profit (open):</div><div class="bp-config-value bp-profit">${:.4}</div>"#,
        total_expected,
    );
    let _ = write!(
        html,
        r#"<div class="bp-config-label">Capital deployed:</div><div class="bp-config-value">${:.2}</div>"#,
        total_cost,
    );
    if state.total_unwind_losses > Decimal::ZERO {
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Unwind losses:</div><div class="bp-config-value bp-loss">-${:.4}</div>"#,
            state.total_unwind_losses,
        );
    }

    html.push_str("</div></div>");
}

/// Render active positions table.
fn render_positions(state: &DutchBookState, html: &mut String) {
    html.push_str(r#"<div class="bp-card mb-4">"#);
    let _ = write!(
        html,
        r#"<h2 class="bp-section-title">Active Positions ({})</h2>"#,
        state.positions.len()
    );

    if state.positions.is_empty() {
        html.push_str(r#"<p class="bp-text-muted">No active positions</p>"#);
    } else {
        html.push_str(r#"<table class="bp-table"><thead><tr>"#);
        html.push_str(r#"<th class="text-left">Market</th>"#);
        html.push_str(r#"<th class="text-right">YES Price</th>"#);
        html.push_str(r#"<th class="text-right">NO Price</th>"#);
        html.push_str(r#"<th class="text-right">Cost</th>"#);
        html.push_str(r#"<th class="text-right">Profit</th>"#);
        html.push_str(r#"<th class="text-right">Size</th>"#);
        html.push_str(r#"<th class="text-right">Age</th>"#);
        html.push_str("</tr></thead><tbody>");

        for pos in &state.positions {
            let age = chrono::Utc::now() - pos.opened_at;
            let age_str = if age.num_hours() > 0 {
                format!("{}h {}m", age.num_hours(), age.num_minutes() % 60)
            } else {
                format!("{}m", age.num_minutes())
            };

            let market_short = truncate_id(&pos.market_id, 8);

            let _ = write!(
                html,
                r#"<tr><td title="{full_id}">{short}</td><td class="text-right">{yes}</td><td class="text-right">{no}</td><td class="text-right">${cost:.4}</td><td class="text-right bp-profit">${profit:.4}</td><td class="text-right">{size}</td><td class="text-right">{age}</td></tr>"#,
                full_id = escape_html(&pos.market_id),
                short = escape_html(&market_short),
                yes = pos.yes_entry_price,
                no = pos.no_entry_price,
                cost = pos.combined_cost,
                profit = pos.expected_profit,
                size = pos.size,
                age = age_str,
            );
        }
        html.push_str("</tbody></table>");
    }
    html.push_str("</div>");
}

/// Render recent opportunities table.
fn render_opportunities(state: &DutchBookState, html: &mut String) {
    html.push_str(r#"<div class="bp-card mb-4">"#);
    let _ = write!(
        html,
        r#"<h2 class="bp-section-title">Recent Opportunities ({})</h2>"#,
        state.recent_opportunities.len()
    );

    if state.recent_opportunities.is_empty() {
        html.push_str(r#"<p class="bp-text-muted">No opportunities detected yet</p>"#);
    } else {
        html.push_str(r#"<table class="bp-table"><thead><tr>"#);
        html.push_str(r#"<th class="text-left">Market</th>"#);
        html.push_str(r#"<th class="text-right">YES Ask</th>"#);
        html.push_str(r#"<th class="text-right">NO Ask</th>"#);
        html.push_str(r#"<th class="text-right">Combined</th>"#);
        html.push_str(r#"<th class="text-right">Profit %</th>"#);
        html.push_str(r#"<th class="text-right">Max Size</th>"#);
        html.push_str(r#"<th class="text-right">When</th>"#);
        html.push_str("</tr></thead><tbody>");

        // Show last 20
        for opp in state.recent_opportunities.iter().take(20) {
            let age = chrono::Utc::now() - opp.detected_at;
            let age_str = if age.num_hours() > 0 {
                format!("{}h ago", age.num_hours())
            } else if age.num_minutes() > 0 {
                format!("{}m ago", age.num_minutes())
            } else {
                format!("{}s ago", age.num_seconds())
            };

            let market_short = truncate_id(&opp.market_id, 8);

            let profit_pct_display = opp.profit_pct * Decimal::new(100, 0);

            let _ = write!(
                html,
                r#"<tr><td title="{full_id}">{short}</td><td class="text-right">{yes}</td><td class="text-right">{no}</td><td class="text-right">{combined}</td><td class="text-right bp-profit">{profit:.2}%</td><td class="text-right">{size}</td><td class="text-right">{when}</td></tr>"#,
                full_id = escape_html(&opp.market_id),
                short = escape_html(&market_short),
                yes = opp.yes_ask,
                no = opp.no_ask,
                combined = opp.combined_cost,
                profit = profit_pct_display,
                size = opp.max_size,
                when = age_str,
            );
        }
        html.push_str("</tbody></table>");
    }
    html.push_str("</div>");
}

/// Render active/unwinding execution status.
fn render_executions(state: &DutchBookState, html: &mut String) {
    if state.executions.is_empty() {
        return;
    }

    html.push_str(r#"<div class="bp-card mb-4">"#);
    let _ = write!(
        html,
        r#"<h2 class="bp-section-title">Execution Status ({})</h2>"#,
        state.executions.len()
    );

    html.push_str(r#"<table class="bp-table"><thead><tr>"#);
    html.push_str(r#"<th class="text-left">Market</th>"#);
    html.push_str(r#"<th class="text-left">State</th>"#);
    html.push_str(r#"<th class="text-right">Size</th>"#);
    html.push_str(r#"<th class="text-right">Age</th>"#);
    html.push_str("</tr></thead><tbody>");

    for exec in &state.executions {
        let age = chrono::Utc::now() - exec.submitted_at;
        let age_str = format!("{}s", age.num_seconds());

        let market_short = truncate_id(&exec.market_id, 8);

        let state_str = match &exec.state {
            ExecutionState::AwaitingFills {
                yes_filled,
                no_filled,
            } => {
                let y = if *yes_filled { "Y" } else { "-" };
                let n = if *no_filled { "N" } else { "-" };
                format!("Awaiting [{y}/{n}]")
            }
            ExecutionState::BothFilled => "Both Filled".to_string(),
            ExecutionState::PartialFill { filled_side, .. } => {
                format!("Partial ({filled_side:?})")
            }
            ExecutionState::Unwinding { .. } => {
                r#"<span class="bp-loss">Unwinding</span>"#.to_string()
            }
            ExecutionState::OneCancelled { cancelled_side } => {
                format!("1 Cancelled ({cancelled_side:?})")
            }
            ExecutionState::Complete => "Complete".to_string(),
        };

        let _ = write!(
            html,
            r#"<tr><td title="{full_id}">{short}</td><td>{state}</td><td class="text-right">{size}</td><td class="text-right">{age}</td></tr>"#,
            full_id = escape_html(&exec.market_id),
            short = escape_html(&market_short),
            state = state_str,
            size = exec.size,
            age = age_str,
        );
    }
    html.push_str("</tbody></table></div>");
}
