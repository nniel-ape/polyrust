//! Dashboard view for the crypto arbitrage strategy.
//!
//! Single dashboard at `/strategy/crypto-arb` combining overview, positions,
//! performance stats, config summary, and skip-reason diagnostics.

use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rust_decimal::Decimal;

use polyrust_core::prelude::*;

use crate::crypto_arb::base::{
    CryptoArbBase, escape_html, fmt_market_price, fmt_usd, net_profit_margin, taker_fee,
};
use crate::crypto_arb::types::ReferenceQuality;

// ---------------------------------------------------------------------------
// Shared HTML rendering helpers
// ---------------------------------------------------------------------------

/// Render reference prices & predictions table (shared across dashboards).
async fn render_reference_prices(base: &CryptoArbBase, html: &mut String) {
    html.push_str(r#"<div class="bp-card mb-4">"#);
    html.push_str(r#"<h2 class="bp-section-title">Reference Prices &amp; Predictions</h2>"#);

    let active_markets = base.active_markets.read().await;
    let price_history = base.price_history.read().await;

    if active_markets.is_empty() {
        html.push_str(r#"<p class="bp-text-muted">No active markets</p>"#);
    } else {
        html.push_str(r#"<table class="bp-table"><thead><tr>"#);
        html.push_str("<th class=\"text-left\">Coin</th>");
        html.push_str("<th class=\"text-right\">Ref Price</th>");
        html.push_str("<th class=\"text-right\">Current</th>");
        html.push_str("<th class=\"text-right\">Change</th>");
        html.push_str("<th class=\"text-right\">Pred</th>");
        html.push_str("</tr></thead><tbody>");

        let mut seen_coins = HashSet::new();
        let mut markets_sorted: Vec<_> = active_markets.values().collect();
        markets_sorted.sort_by(|a, b| a.coin.cmp(&b.coin));

        for mwr in &markets_sorted {
            if !seen_coins.insert(&mwr.coin) {
                continue;
            }
            let current_price = price_history
                .get(&mwr.coin)
                .and_then(|h| h.back().map(|(_, p, _)| *p));

            let ref_label = match mwr.reference_quality {
                ReferenceQuality::OnChain(_) => "✓",
                ReferenceQuality::Exact => "=",
                ReferenceQuality::Historical(_) => "≈",
                ReferenceQuality::Current => "~",
            };

            let (change_str, change_class, prediction) = match current_price {
                Some(cp) => {
                    let change = if mwr.reference_price.is_zero() {
                        Decimal::ZERO
                    } else {
                        ((cp - mwr.reference_price) / mwr.reference_price) * Decimal::new(100, 0)
                    };
                    let cls = if change >= Decimal::ZERO {
                        "bp-profit"
                    } else {
                        "bp-loss"
                    };
                    let pred = match mwr.predict_winner(cp) {
                        Some(OutcomeSide::Up) | Some(OutcomeSide::Yes) => "UP",
                        Some(OutcomeSide::Down) | Some(OutcomeSide::No) => "DOWN",
                        None => "-",
                    };
                    (format!("{:+.2}%", change), cls, pred)
                }
                None => ("-".to_string(), "", "-"),
            };

            let _ = write!(
                html,
                r#"<tr><td class="py-1">{coin}</td><td class="text-right py-1">{ref_label}{ref_price}</td><td class="text-right py-1">{current}</td><td class="text-right py-1 {change_class}">{change}</td><td class="text-right py-1" style="font-weight:700">{prediction}</td></tr>"#,
                coin = escape_html(&mwr.coin),
                ref_label = ref_label,
                ref_price = fmt_usd(mwr.reference_price),
                current = current_price
                    .map(fmt_usd)
                    .unwrap_or_else(|| "-".to_string()),
                change_class = change_class,
                change = change_str,
                prediction = prediction,
            );
        }
        html.push_str("</tbody></table>");
    }
    html.push_str("</div>");
}

/// Render positions table.
async fn render_positions(base: &CryptoArbBase, html: &mut String) {
    let positions = base.positions.read().await;
    let cached_asks = base.cached_asks.read().await;

    let mode_positions: Vec<_> = positions.values().flat_map(|v| v.iter()).collect();

    html.push_str(r#"<div class="bp-card mb-4">"#);
    let _ = write!(
        html,
        r#"<h2 class="bp-section-title">Open Positions ({})</h2>"#,
        mode_positions.len()
    );

    if mode_positions.is_empty() {
        html.push_str(r#"<p class="bp-text-muted">No open positions</p>"#);
    } else {
        html.push_str(r#"<table class="bp-table"><thead><tr>"#);
        html.push_str("<th class=\"text-left\">Market</th>");
        html.push_str("<th class=\"text-left\">Side</th>");
        html.push_str("<th class=\"text-right\">Entry</th>");
        html.push_str("<th class=\"text-right\">Current</th>");
        html.push_str("<th class=\"text-right\">PnL</th>");
        html.push_str("<th class=\"text-right\">Size</th>");
        html.push_str("<th class=\"text-right\">Kelly</th>");
        html.push_str("</tr></thead><tbody>");

        for pos in &mode_positions {
            let current = cached_asks.get(&pos.token_id).copied();
            let (current_str, pnl_str, pnl_class) = match current {
                Some(cp) => {
                    let pnl = (cp - pos.entry_price) * pos.size - (pos.entry_fee_per_share * pos.size);
                    let cls = if pnl >= Decimal::ZERO {
                        "bp-profit"
                    } else {
                        "bp-loss"
                    };
                    (cp.to_string(), format!("${pnl:.2}"), cls)
                }
                None => ("-".to_string(), "-".to_string(), ""),
            };
            let kelly_str = match pos.kelly_fraction {
                Some(kf) => format!("{:.1}%", kf * Decimal::new(100, 0)),
                None => "fixed".to_string(),
            };

            let _ = write!(
                html,
                r#"<tr><td>{coin}</td><td>{side:?}</td><td class="text-right">{entry}</td><td class="text-right">{current}</td><td class="text-right"><span class="{pnl_class}">{pnl}</span></td><td class="text-right">{size}</td><td class="text-right">{kelly}</td></tr>"#,
                coin = escape_html(&pos.coin),
                side = pos.side,
                entry = pos.entry_price,
                current = current_str,
                pnl_class = pnl_class,
                pnl = pnl_str,
                size = pos.size,
                kelly = kelly_str,
            );
        }
        html.push_str("</tbody></table>");
    }
    html.push_str("</div>");
}

/// Render performance stats.
async fn render_performance(base: &CryptoArbBase, html: &mut String) {
    let s = base.stats.read().await;

    html.push_str(r#"<div class="bp-card mb-4">"#);
    html.push_str(r#"<h2 class="bp-section-title">Performance Stats</h2>"#);

    if s.total_trades() > 0 {
        {
            let s = &*s;
            html.push_str(r#"<table class="bp-table"><thead><tr>"#);
            html.push_str("<th class=\"text-right\">Trades</th>");
            html.push_str("<th class=\"text-right\">Won</th>");
            html.push_str("<th class=\"text-right\">Lost</th>");
            html.push_str("<th class=\"text-right\">Win Rate</th>");
            html.push_str("<th class=\"text-right\">Total P&amp;L</th>");
            html.push_str("<th class=\"text-right\">Avg P&amp;L</th>");
            html.push_str("</tr></thead><tbody>");

            let win_rate_pct = s.win_rate() * Decimal::new(100, 0);
            let pnl_class = if s.total_pnl >= Decimal::ZERO {
                "bp-profit"
            } else {
                "bp-loss"
            };

            let _ = write!(
                html,
                r#"<tr><td class="text-right">{trades}</td><td class="text-right">{won}</td><td class="text-right">{lost}</td><td class="text-right">{win_rate:.1}%</td><td class="text-right {pnl_class}">${total_pnl:.2}</td><td class="text-right">${avg_pnl:.4}</td></tr>"#,
                trades = s.total_trades(),
                won = s.won,
                lost = s.lost,
                win_rate = win_rate_pct,
                pnl_class = pnl_class,
                total_pnl = s.total_pnl,
                avg_pnl = s.avg_pnl(),
            );
            html.push_str("</tbody></table>");
        }
    } else {
        html.push_str(r#"<p class="bp-text-muted">No trades recorded yet</p>"#);
    }
    html.push_str("</div>");
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

/// Dashboard view for the crypto arbitrage strategy at `/strategy/crypto-arb`.
pub struct CryptoArbDashboard {
    base: Arc<CryptoArbBase>,
}

impl CryptoArbDashboard {
    pub fn new(base: Arc<CryptoArbBase>) -> Self {
        Self { base }
    }
}

impl DashboardViewProvider for CryptoArbDashboard {
    fn view_name(&self) -> &str {
        "crypto-arb"
    }

    fn render_view(
        &self,
    ) -> Pin<Box<dyn Future<Output = polyrust_core::error::Result<String>> + Send + '_>> {
        Box::pin(self.render_view_impl())
    }
}

impl CryptoArbDashboard {
    async fn render_view_impl(&self) -> polyrust_core::error::Result<String> {
        let mut html = String::with_capacity(8192);

        // --- Strategy Status ---
        {
            let status = if self.base.config.enabled {
                r#"<span style="color:var(--color-enabled)">Enabled</span>"#
            } else {
                r#"<span class="bp-text-muted">Disabled</span>"#
            };
            let _ = write!(
                html,
                r#"<div class="bp-card mb-4"><p class="bp-text-secondary" style="font-size:0.95rem">Status: {}</p></div>"#,
                status
            );
        }

        // --- TailEnd Config Summary ---
        {
            let te = &self.base.config.tailend;
            html.push_str(r#"<details class="bp-card mb-4">"#);
            html.push_str(
                r#"<summary class="bp-section-title" style="cursor:pointer">Configuration</summary>"#,
            );
            html.push_str(r#"<div class="grid grid-cols-2 gap-2" style="font-size:0.95rem">"#);

            html.push_str(r#"<div class="bp-text-muted" style="grid-column:1/-1;font-weight:600;margin-top:0.25rem">Entry Timing</div>"#);
            let _ = write!(html, r#"<div class="bp-config-label">Time window:</div><div class="bp-config-value">&lt; {}s</div>"#, te.time_threshold_secs);
            let thresh_str: Vec<String> = te.dynamic_thresholds.iter().map(|(s, p)| format!("{}s&rarr;{}", s, p)).collect();
            let _ = write!(html, r#"<div class="bp-config-label">Dynamic thresholds:</div><div class="bp-config-value">{}</div>"#, thresh_str.join(", "));
            let _ = write!(html, r#"<div class="bp-config-label">Min ref quality:</div><div class="bp-config-value">{:?}</div>"#, te.min_reference_quality);

            html.push_str(r#"<div class="bp-text-muted" style="grid-column:1/-1;font-weight:600;margin-top:0.5rem">Market Filters</div>"#);
            let _ = write!(html, r#"<div class="bp-config-label">Max spread:</div><div class="bp-config-value">{} bps</div>"#, te.max_spread_bps);
            let _ = write!(html, r#"<div class="bp-config-label">Stale OB:</div><div class="bp-config-value">{}s</div>"#, te.stale_ob_secs);
            let _ = write!(html, r#"<div class="bp-config-label">Sustained:</div><div class="bp-config-value">{}s / {} ticks</div>"#, te.min_sustained_secs, te.min_sustained_ticks);
            let _ = write!(html, r#"<div class="bp-config-label">Max volatility:</div><div class="bp-config-value">{}%</div>"#, te.max_recent_volatility * Decimal::from(100));
            let _ = write!(html, r#"<div class="bp-config-label">Strike distance:</div><div class="bp-config-value">{}%</div>"#, te.min_strike_distance_pct * Decimal::from(100));

            html.push_str("</div></details>");
        }

        // --- Reference Prices ---
        render_reference_prices(&self.base, &mut html).await;

        // --- Active Markets ---
        {
            let active_markets = self.base.active_markets.read().await;
            let cached_asks = self.base.cached_asks.read().await;
            let fee_rate = self.base.config.fee.taker_fee_rate;

            html.push_str(r#"<div class="bp-card mb-4">"#);
            let _ = write!(html, r#"<h2 class="bp-section-title">Active Markets ({})</h2>"#, active_markets.len());

            if active_markets.is_empty() {
                html.push_str(r#"<p class="bp-text-muted">No active markets</p>"#);
            } else {
                html.push_str(r#"<table class="bp-table"><thead><tr>"#);
                html.push_str("<th class=\"text-left\">Market</th>");
                html.push_str("<th class=\"text-right\">UP</th>");
                html.push_str("<th class=\"text-right\">DOWN</th>");
                html.push_str("<th class=\"text-right\">Fee</th>");
                html.push_str("<th class=\"text-right\">Net</th>");
                html.push_str("<th class=\"text-right\">Time Left</th>");
                html.push_str("</tr></thead><tbody>");

                let mut markets_by_time: Vec<_> = active_markets.values().collect();
                markets_by_time.sort_by_key(|m| m.market.end_date);

                for mwr in &markets_by_time {
                    let remaining = mwr.market.seconds_remaining().max(0);
                    let time_str = if remaining > 60 {
                        format!("{}m {}s", remaining / 60, remaining % 60)
                    } else {
                        format!("{}s", remaining)
                    };

                    let up_ask = cached_asks.get(&mwr.market.token_ids.outcome_a).copied();
                    let down_ask = cached_asks.get(&mwr.market.token_ids.outcome_b).copied();
                    let up_price = up_ask.map(fmt_market_price).unwrap_or_else(|| "-".to_string());
                    let down_price = down_ask.map(fmt_market_price).unwrap_or_else(|| "-".to_string());

                    let (fee_str, net_str) = match (up_ask, down_ask) {
                        (Some(ua), Some(da)) => {
                            let price = ua.min(da);
                            let fee = taker_fee(price, fee_rate);
                            let net = net_profit_margin(price, fee_rate, false);
                            (format!("{:.3}", fee.round_dp(3)), format!("{:.3}", net.round_dp(3)))
                        }
                        (Some(p), None) | (None, Some(p)) => {
                            let fee = taker_fee(p, fee_rate);
                            let net = net_profit_margin(p, fee_rate, false);
                            (format!("{:.3}", fee.round_dp(3)), format!("{:.3}", net.round_dp(3)))
                        }
                        _ => ("-".to_string(), "-".to_string()),
                    };

                    let _ = write!(
                        html,
                        r#"<tr><td>{coin} Up/Down</td><td class="text-right">{up}</td><td class="text-right">{down}</td><td class="text-right">{fee}</td><td class="text-right">{net}</td><td class="text-right">{time}</td></tr>"#,
                        coin = escape_html(&mwr.coin), up = up_price, down = down_price,
                        fee = fee_str, net = net_str, time = time_str,
                    );
                }
                html.push_str("</tbody></table>");
            }
            html.push_str("</div>");
        }

        // --- Open Positions ---
        render_positions(&self.base, &mut html).await;

        // --- Performance Stats ---
        render_performance(&self.base, &mut html).await;

        // --- Skip Reason Chart ---
        render_skip_stats(&self.base, &mut html);

        Ok(html)
    }
}

// ---------------------------------------------------------------------------
// TailEnd skip-reason bar chart
// ---------------------------------------------------------------------------

/// Group definition for skip reasons.
struct SkipGroup {
    label: &'static str,
    css_suffix: &'static str,
    reasons: &'static [&'static str],
}

const SKIP_GROUPS: &[SkipGroup] = &[
    SkipGroup {
        label: "Pre-filters",
        css_suffix: "prefilter",
        reasons: &["time_window", "coin_not_near_expiry"],
    },
    SkipGroup {
        label: "Market Quality",
        css_suffix: "market",
        reasons: &["no_ask", "stale_ob", "spread"],
    },
    SkipGroup {
        label: "Signal Quality",
        css_suffix: "signal",
        reasons: &[
            "ref_quality",
            "no_prediction",
            "threshold",
            "sustained",
            "volatility",
            "strike_proximity",
        ],
    },
    SkipGroup {
        label: "Rate Limiting",
        css_suffix: "ratelimit",
        reasons: &[
            "stale_cooldown",
            "rejection_cooldown",
            "reservation",
            "auto_disabled",
        ],
    },
    SkipGroup {
        label: "Composite",
        css_suffix: "composite",
        reasons: &["composite_stale"],
    },
];

/// Render the TailEnd skip-reason bar chart. Reads the current 60s period
/// stats from `tailend_skip_stats` without draining.
fn render_skip_stats(base: &CryptoArbBase, html: &mut String) {
    let snapshot: Vec<(&'static str, u64)> = {
        let stats = base.tailend_skip_stats.lock().unwrap();
        stats.iter().map(|(k, v)| (*k, *v)).collect()
    };

    let max_count = snapshot.iter().map(|(_, v)| *v).max().unwrap_or(0);

    html.push_str(r#"<div class="bp-card mb-4">"#);
    html.push_str(r#"<h2 class="bp-section-title">Skip Reasons (last 60s)</h2>"#);

    if snapshot.is_empty() || max_count == 0 {
        html.push_str(r#"<p class="bp-text-muted">No skips in current period</p></div>"#);
        return;
    }

    html.push_str(r#"<div class="bp-skip-chart">"#);

    for group in SKIP_GROUPS {
        let rows: Vec<_> = group
            .reasons
            .iter()
            .filter_map(|r| {
                snapshot
                    .iter()
                    .find(|(k, _)| k == r)
                    .map(|(k, v)| (*k, *v))
            })
            .filter(|(_, v)| *v > 0)
            .collect();

        if rows.is_empty() {
            continue;
        }

        let _ = write!(
            html,
            r#"<div class="bp-skip-group-header">{}</div>"#,
            group.label
        );

        for (reason, count) in &rows {
            let width_pct = (*count as f64 / max_count as f64) * 100.0;
            let _ = write!(
                html,
                r#"<div class="bp-skip-row"><span class="bp-skip-label">{reason}</span><div class="bp-skip-bar-track"><div class="bp-skip-bar-fill bp-skip-bar-fill--{suffix}" style="width:{width:.1}%"></div></div><span class="bp-skip-count">{count}</span></div>"#,
                reason = reason,
                suffix = group.css_suffix,
                width = width_pct,
                count = count,
            );
        }
    }

    html.push_str("</div></div>");
}

// ---------------------------------------------------------------------------
// SSE Dashboard Update Emission
// ---------------------------------------------------------------------------

/// Emit SSE dashboard-update signals if the shared throttle allows.
///
/// Each signal carries pre-rendered HTML so the SSE handler can broadcast it
/// without re-acquiring strategy locks. Called at the end of each strategy's
/// `on_event()` — the shared 5-second throttle ensures only one strategy per
/// window triggers the render.
pub async fn try_emit_dashboard_updates(base: &Arc<CryptoArbBase>) -> Vec<Action> {
    if !base.try_claim_dashboard_emit().await {
        return vec![];
    }

    let provider = CryptoArbDashboard::new(Arc::clone(base));
    match provider.render_view().await {
        Ok(html) => vec![Action::EmitSignal {
            signal_type: "dashboard-update".to_string(),
            payload: serde_json::json!({
                "view_name": provider.view_name(),
                "rendered_html": html,
            }),
        }],
        Err(_) => vec![],
    }
}
