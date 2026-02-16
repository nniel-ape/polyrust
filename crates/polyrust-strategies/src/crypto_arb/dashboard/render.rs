//! HTML section rendering for the crypto arbitrage dashboard.
//!
//! Contains render functions for positions, prices, performance, and skip stats.

use std::collections::HashSet;
use std::fmt::Write as FmtWrite;

use rust_decimal::Decimal;

use polyrust_core::prelude::*;

use crate::crypto_arb::domain::ReferenceQuality;
use crate::crypto_arb::runtime::CryptoArbRuntime;
use crate::crypto_arb::services::{escape_html, fmt_usd};

/// Render reference prices & predictions table (shared across dashboards).
pub(crate) async fn render_reference_prices(base: &CryptoArbRuntime, html: &mut String) {
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
                .and_then(|h| h.back().map(|(_, p, _, _)| *p));

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
pub(crate) async fn render_positions(base: &CryptoArbRuntime, html: &mut String) {
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
                    let pnl =
                        (cp - pos.entry_price) * pos.size - (pos.entry_fee_per_share * pos.size);
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
pub(crate) async fn render_performance(base: &CryptoArbRuntime, html: &mut String) {
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
// TailEnd skip-reason bar chart
// ---------------------------------------------------------------------------

/// Map internal skip-reason key → (human label, tooltip description).
fn skip_reason_info(key: &str) -> (&str, &str) {
    match key {
        "time_window" => (
            "Time window",
            "Market not in the final 0\u{2013}120s before expiry",
        ),
        "coin_not_near_expiry" => ("No expiry soon", "No market expiring soon for this coin"),
        "no_ask" => ("No liquidity", "No ask in orderbook for predicted side"),
        "stale_ob" => ("Stale orderbook", "Orderbook data too old to trust"),
        "spread" => ("Wide spread", "Bid-ask spread too wide (illiquid)"),
        "ref_quality" => ("Low ref quality", "Reference price quality below threshold"),
        "no_prediction" => (
            "No prediction",
            "Price equals reference \u{2014} no directional signal",
        ),
        "threshold" => (
            "Below threshold",
            "Ask price below dynamic confidence threshold",
        ),
        "sustained" => ("Not sustained", "Price direction not held long enough"),
        "volatility" => ("High volatility", "Recent price too choppy"),
        "strike_proximity" => ("Near strike", "Crypto price too close to strike price"),
        "stale_cooldown" => ("Stale cooldown", "Market on cooldown after stale data"),
        "rejection_cooldown" => (
            "Rejection cooldown",
            "Market on cooldown after order rejection",
        ),
        "recovery_cooldown" => (
            "Recovery cooldown",
            "Market on cooldown after recovery exit",
        ),
        "reservation" => (
            "Reserved/maxed",
            "Market reserved, has exposure, or max positions",
        ),
        "auto_disabled" => ("Auto-disabled", "Mode disabled by performance tracker"),
        "composite_stale" => ("Stale composite", "Composite reference price too old"),
        other => (other, ""),
    }
}

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
            "recovery_cooldown",
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
pub(crate) fn render_skip_stats(base: &CryptoArbRuntime, html: &mut String) {
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
            .filter_map(|r| snapshot.iter().find(|(k, _)| k == r).map(|(k, v)| (*k, *v)))
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
            let (label, tooltip) = skip_reason_info(reason);
            let _ = write!(
                html,
                r#"<div class="bp-skip-row"><span class="bp-skip-label" title="{tooltip}">{label}</span><div class="bp-skip-bar-track"><div class="bp-skip-bar-fill bp-skip-bar-fill--{suffix}" style="width:{width:.1}%"></div></div><span class="bp-skip-count">{count}</span></div>"#,
                tooltip = tooltip,
                label = label,
                suffix = group.css_suffix,
                width = width_pct,
                count = count,
            );
        }
    }

    html.push_str("</div></div>");
}
