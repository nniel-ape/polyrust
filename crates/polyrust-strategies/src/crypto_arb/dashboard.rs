//! Dashboard views for crypto arbitrage strategies.
//!
//! Provides:
//! - **Overview dashboard** at `/strategy/crypto-arb` — summary of all modes, links to each
//! - **Per-mode dashboards** — detailed view for each trading mode:
//!   - `/strategy/crypto-arb-tailend`
//!   - `/strategy/crypto-arb-twosided`

use std::collections::HashSet;
use std::fmt::Write as FmtWrite;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::Utc;
use rust_decimal::Decimal;

use polyrust_core::prelude::*;

use crate::crypto_arb::base::{
    CryptoArbBase, escape_html, fmt_market_price, fmt_usd, net_profit_margin, taker_fee,
};
use crate::crypto_arb::types::{ArbitrageMode, ReferenceQuality};

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

/// Render positions table filtered by mode.
async fn render_positions_for_mode(base: &CryptoArbBase, html: &mut String, mode_filter: &str) {
    let positions = base.positions.read().await;
    let cached_asks = base.cached_asks.read().await;

    // Filter positions by mode
    let mode_positions: Vec<_> = positions
        .values()
        .flat_map(|v| v.iter())
        .filter(|p| mode_matches(&p.mode, mode_filter))
        .collect();

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
                    let pnl = (cp - pos.entry_price) * pos.size - (pos.estimated_fee * pos.size);
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

/// Render performance stats for a specific mode.
async fn render_performance_for_mode(base: &CryptoArbBase, html: &mut String, mode_filter: &str) {
    let mode_stats = base.mode_stats.read().await;

    html.push_str(r#"<div class="bp-card mb-4">"#);
    html.push_str(r#"<h2 class="bp-section-title">Performance Stats</h2>"#);

    // Find stats for this mode
    let stats = mode_stats
        .iter()
        .find(|(m, _)| mode_matches(m, mode_filter))
        .map(|(_, s)| s);

    match stats {
        Some(s) => {
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
        None => {
            html.push_str(r#"<p class="bp-text-muted">No trades recorded yet</p>"#);
        }
    }
    html.push_str("</div>");
}

/// Check if mode matches filter string.
fn mode_matches(mode: &ArbitrageMode, filter: &str) -> bool {
    matches!(
        (mode, filter),
        (ArbitrageMode::TailEnd, "tailend") | (ArbitrageMode::TwoSided, "twosided")
    )
}

// ---------------------------------------------------------------------------
// Overview Dashboard (shows all modes, links to per-mode dashboards)
// ---------------------------------------------------------------------------

/// Overview dashboard view for crypto arbitrage strategies.
///
/// Shows summary of all modes at `/strategy/crypto-arb` with links to per-mode dashboards.
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

        // --- Mode Status & Navigation ---
        html.push_str(r#"<div class="bp-card mb-4">"#);
        html.push_str(r#"<h2 class="bp-section-title">Trading Modes</h2>"#);
        html.push_str(r#"<div class="grid grid-cols-2 gap-4">"#);

        let modes = [
            ("TailEnd", "tailend", self.base.config.tailend.enabled),
            ("TwoSided", "twosided", self.base.config.twosided.enabled),
        ];

        for (name, slug, enabled) in &modes {
            let status_style = if *enabled {
                "color: var(--color-enabled);"
            } else {
                "color: var(--text-muted);"
            };
            let status_text = if *enabled { "Enabled" } else { "Disabled" };

            let _ = write!(
                html,
                r#"<a href="/strategy/crypto-arb-{slug}" class="bp-mode-card"><div class="bp-mode-card-title">{name}</div><div style="font-size:0.95rem;{status_style}">{status_text}</div></a>"#,
                slug = slug,
                name = name,
                status_style = status_style,
                status_text = status_text,
            );
        }
        html.push_str("</div></div>");

        // --- Reference Prices ---
        render_reference_prices(&self.base, &mut html).await;

        // --- Active Markets ---
        html.push_str(r#"<div class="bp-card mb-4">"#);
        let active_markets = self.base.active_markets.read().await;
        let _ = write!(
            html,
            r#"<h2 class="bp-section-title">Active Markets ({})</h2>"#,
            active_markets.len()
        );

        let cached_asks = self.base.cached_asks.read().await;

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

            let fee_rate = self.base.config.fee.taker_fee_rate;

            for mwr in &markets_by_time {
                let remaining = mwr.market.seconds_remaining().max(0);
                let time_str = if remaining > 60 {
                    format!("{}m {}s", remaining / 60, remaining % 60)
                } else {
                    format!("{}s", remaining)
                };

                let up_ask = cached_asks.get(&mwr.market.token_ids.outcome_a).copied();
                let down_ask = cached_asks.get(&mwr.market.token_ids.outcome_b).copied();

                let up_price = up_ask
                    .map(fmt_market_price)
                    .unwrap_or_else(|| "-".to_string());
                let down_price = down_ask
                    .map(fmt_market_price)
                    .unwrap_or_else(|| "-".to_string());

                let (fee_str, net_str) = match (up_ask, down_ask) {
                    (Some(ua), Some(da)) => {
                        let price = ua.min(da);
                        let fee = taker_fee(price, fee_rate);
                        let net = net_profit_margin(price, fee_rate, false);
                        (
                            format!("{:.3}", fee.round_dp(3)),
                            format!("{:.3}", net.round_dp(3)),
                        )
                    }
                    (Some(p), None) | (None, Some(p)) => {
                        let fee = taker_fee(p, fee_rate);
                        let net = net_profit_margin(p, fee_rate, false);
                        (
                            format!("{:.3}", fee.round_dp(3)),
                            format!("{:.3}", net.round_dp(3)),
                        )
                    }
                    _ => ("-".to_string(), "-".to_string()),
                };

                let _ = write!(
                    html,
                    r#"<tr><td>{coin} Up/Down</td><td class="text-right">{up}</td><td class="text-right">{down}</td><td class="text-right">{fee}</td><td class="text-right">{net}</td><td class="text-right">{time}</td></tr>"#,
                    coin = escape_html(&mwr.coin),
                    up = up_price,
                    down = down_price,
                    fee = fee_str,
                    net = net_str,
                    time = time_str,
                );
            }
            html.push_str("</tbody></table>");
        }
        html.push_str("</div>");

        drop(active_markets);
        drop(cached_asks);

        // --- All Open Positions (summary) ---
        let positions = self.base.positions.read().await;
        html.push_str(r#"<div class="bp-card mb-4">"#);
        let total_positions: usize = positions.values().map(|v| v.len()).sum();
        let _ = write!(
            html,
            r#"<h2 class="bp-section-title">Open Positions ({})</h2>"#,
            total_positions
        );

        if positions.is_empty() {
            html.push_str(r#"<p class="bp-text-muted">No open positions</p>"#);
        } else {
            // Count by mode
            let mut mode_counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for pos_list in positions.values() {
                for pos in pos_list {
                    let mode_name = match &pos.mode {
                        ArbitrageMode::TailEnd => "TailEnd",
                        ArbitrageMode::TwoSided => "TwoSided",
                    };
                    *mode_counts.entry(mode_name.to_string()).or_default() += 1;
                }
            }

            html.push_str(r#"<div class="flex flex-wrap gap-2">"#);
            for (mode, count) in &mode_counts {
                let _ = write!(
                    html,
                    r#"<span class="bp-badge">{mode}: {count}</span>"#,
                    mode = mode,
                    count = count,
                );
            }
            html.push_str("</div>");
        }
        html.push_str("</div>");

        drop(positions);

        // --- Performance Stats (all modes) ---
        let mode_stats = self.base.mode_stats.read().await;
        html.push_str(r#"<div class="bp-card mb-4">"#);
        html.push_str(r#"<h2 class="bp-section-title">Performance Stats</h2>"#);

        if mode_stats.is_empty() {
            html.push_str(r#"<p class="bp-text-muted">No trades recorded yet</p>"#);
        } else {
            html.push_str(r#"<table class="bp-table"><thead><tr>"#);
            html.push_str("<th class=\"text-left\">Mode</th>");
            html.push_str("<th class=\"text-right\">Trades</th>");
            html.push_str("<th class=\"text-right\">Won</th>");
            html.push_str("<th class=\"text-right\">Lost</th>");
            html.push_str("<th class=\"text-right\">Win Rate</th>");
            html.push_str("<th class=\"text-right\">Total P&amp;L</th>");
            html.push_str("<th class=\"text-right\">Avg P&amp;L</th>");
            html.push_str("<th class=\"text-left\">Status</th>");
            html.push_str("</tr></thead><tbody>");

            let mut modes: Vec<_> = mode_stats.iter().collect();
            modes.sort_by(|a, b| a.0.to_string().cmp(&b.0.to_string()));

            for (mode, stats) in &modes {
                let win_rate_pct = stats.win_rate() * Decimal::new(100, 0);
                let pnl_class = if stats.total_pnl >= Decimal::ZERO {
                    "bp-profit"
                } else {
                    "bp-loss"
                };
                let status = if self.base.is_mode_disabled(mode).await {
                    r#"<span style="color:var(--color-loss)">Disabled</span>"#
                } else {
                    r#"<span style="color:var(--color-enabled)">Active</span>"#
                };
                let _ = write!(
                    html,
                    r#"<tr><td>{mode}</td><td class="text-right">{trades}</td><td class="text-right">{won}</td><td class="text-right">{lost}</td><td class="text-right">{win_rate:.1}%</td><td class="text-right {pnl_class}">${total_pnl:.2}</td><td class="text-right">${avg_pnl:.4}</td><td>{status}</td></tr>"#,
                    mode = mode,
                    trades = stats.total_trades(),
                    won = stats.won,
                    lost = stats.lost,
                    win_rate = win_rate_pct,
                    pnl_class = pnl_class,
                    total_pnl = stats.total_pnl,
                    avg_pnl = stats.avg_pnl(),
                    status = status,
                );
            }
            html.push_str("</tbody></table>");
        }
        html.push_str("</div>");

        Ok(html)
    }
}

// ---------------------------------------------------------------------------
// TailEnd Dashboard
// ---------------------------------------------------------------------------

/// TailEnd mode dashboard at `/strategy/crypto-arb-tailend`.
pub struct TailEndDashboard {
    base: Arc<CryptoArbBase>,
}

impl TailEndDashboard {
    pub fn new(base: Arc<CryptoArbBase>) -> Self {
        Self { base }
    }
}

impl DashboardViewProvider for TailEndDashboard {
    fn view_name(&self) -> &str {
        "crypto-arb-tailend"
    }

    fn render_view(
        &self,
    ) -> Pin<Box<dyn Future<Output = polyrust_core::error::Result<String>> + Send + '_>> {
        Box::pin(self.render_view_impl())
    }
}

impl TailEndDashboard {
    async fn render_view_impl(&self) -> polyrust_core::error::Result<String> {
        let mut html = String::with_capacity(4096);

        // Header with back link
        html.push_str(r#"<div class="mb-4">"#);
        html.push_str(
            r#"<a href="/strategy/crypto-arb" class="bp-back-link">&larr; Back to Overview</a>"#,
        );
        html.push_str(r#"<h1 class="bp-page-title mt-2">TailEnd Mode</h1>"#);

        // Status
        let status = if self.base.config.tailend.enabled {
            r#"<span style="color:var(--color-enabled)">Enabled</span>"#
        } else {
            r#"<span class="bp-text-muted">Disabled</span>"#
        };
        let _ = write!(
            html,
            r#"<p class="bp-text-secondary" style="font-size:0.95rem">Status: {}</p>"#,
            status
        );
        html.push_str("</div>");

        // Config summary
        let te = &self.base.config.tailend;
        html.push_str(r#"<details class="bp-card mb-4">"#);
        html.push_str(
            r#"<summary class="bp-section-title" style="cursor:pointer">Configuration</summary>"#,
        );
        html.push_str(r#"<div class="grid grid-cols-2 gap-2" style="font-size:0.95rem">"#);

        // — Entry Timing —
        html.push_str(r#"<div class="bp-text-muted" style="grid-column:1/-1;font-weight:600;margin-top:0.25rem">Entry Timing</div>"#);
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Time window:</div><div class="bp-config-value">&lt; {}s</div>"#,
            te.time_threshold_secs
        );
        // Dynamic thresholds as compact list
        let thresh_str: Vec<String> = te
            .dynamic_thresholds
            .iter()
            .map(|(s, p)| format!("{}s&rarr;{}", s, p))
            .collect();
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Dynamic thresholds:</div><div class="bp-config-value">{}</div>"#,
            thresh_str.join(", ")
        );
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Min ref quality:</div><div class="bp-config-value">{:?}</div>"#,
            te.min_reference_quality
        );

        // — Market Filters —
        html.push_str(r#"<div class="bp-text-muted" style="grid-column:1/-1;font-weight:600;margin-top:0.5rem">Market Filters</div>"#);
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Max spread:</div><div class="bp-config-value">{} bps</div>"#,
            te.max_spread_bps
        );
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Stale OB:</div><div class="bp-config-value">{}s</div>"#,
            te.stale_ob_secs
        );
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Sustained:</div><div class="bp-config-value">{}s / {} ticks</div>"#,
            te.min_sustained_secs, te.min_sustained_ticks
        );
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Max volatility:</div><div class="bp-config-value">{}%</div>"#,
            te.max_recent_volatility * Decimal::from(100)
        );
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Strike distance:</div><div class="bp-config-value">{}%</div>"#,
            te.min_strike_distance_pct * Decimal::from(100)
        );
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Rejection cooldown:</div><div class="bp-config-value">{}s</div>"#,
            te.rejection_cooldown_secs
        );

        // — Post-Entry / Sell —
        html.push_str(r#"<div class="bp-text-muted" style="grid-column:1/-1;font-weight:600;margin-top:0.5rem">Post-Entry / Sell</div>"#);
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Post-entry exit:</div><div class="bp-config-value">${} drop in {}s</div>"#,
            te.post_entry_exit_drop, te.post_entry_window_secs
        );
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Min sell delay:</div><div class="bp-config-value">{}s</div>"#,
            te.min_sell_delay_secs
        );
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Post-only:</div><div class="bp-config-value">{}</div>"#,
            if te.post_only { "Yes" } else { "No" }
        );

        // — Composite Price —
        html.push_str(r#"<div class="bp-text-muted" style="grid-column:1/-1;font-weight:600;margin-top:0.5rem">Composite Price</div>"#);
        if te.use_composite_price {
            let _ = write!(
                html,
                r#"<div class="bp-config-label">Min sources:</div><div class="bp-config-value">{}</div>"#,
                te.min_sources
            );
            let _ = write!(
                html,
                r#"<div class="bp-config-label">Max source stale:</div><div class="bp-config-value">{}s</div>"#,
                te.max_source_stale_secs
            );
            let _ = write!(
                html,
                r#"<div class="bp-config-label">Max dispersion:</div><div class="bp-config-value">{} bps</div>"#,
                te.max_dispersion_bps
            );
            let _ = write!(
                html,
                r#"<div class="bp-config-label">Feed stale:</div><div class="bp-config-value">{}s</div>"#,
                te.feed_stale_secs
            );
        } else {
            html.push_str(r#"<div class="bp-config-label" style="grid-column:1/-1"><span class="bp-text-muted">Disabled</span></div>"#);
        }

        html.push_str("</div></details>");

        // Active markets
        {
            let active_markets = self.base.active_markets.read().await;
            let cached_asks = self.base.cached_asks.read().await;
            let fee_rate = self.base.config.fee.taker_fee_rate;

            let mut markets_by_time: Vec<_> = active_markets.values().collect();
            markets_by_time.sort_by_key(|m| m.market.end_date);

            html.push_str(r#"<div class="bp-card mb-4">"#);
            let _ = write!(
                html,
                r#"<h2 class="bp-section-title">Active Markets ({})</h2>"#,
                markets_by_time.len()
            );

            if markets_by_time.is_empty() {
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

                for mwr in &markets_by_time {
                    let remaining = mwr.market.seconds_remaining().max(0);
                    let time_str = if remaining > 60 {
                        format!("{}m {}s", remaining / 60, remaining % 60)
                    } else {
                        format!("{}s", remaining)
                    };

                    let up_ask = cached_asks.get(&mwr.market.token_ids.outcome_a).copied();
                    let down_ask = cached_asks.get(&mwr.market.token_ids.outcome_b).copied();

                    let up_price = up_ask
                        .map(fmt_market_price)
                        .unwrap_or_else(|| "-".to_string());
                    let down_price = down_ask
                        .map(fmt_market_price)
                        .unwrap_or_else(|| "-".to_string());

                    let (fee_str, net_str) = match (up_ask, down_ask) {
                        (Some(ua), Some(da)) => {
                            let price = ua.min(da);
                            let fee = taker_fee(price, fee_rate);
                            let net = net_profit_margin(price, fee_rate, false);
                            (
                                format!("{:.3}", fee.round_dp(3)),
                                format!("{:.3}", net.round_dp(3)),
                            )
                        }
                        (Some(p), None) | (None, Some(p)) => {
                            let fee = taker_fee(p, fee_rate);
                            let net = net_profit_margin(p, fee_rate, false);
                            (
                                format!("{:.3}", fee.round_dp(3)),
                                format!("{:.3}", net.round_dp(3)),
                            )
                        }
                        _ => ("-".to_string(), "-".to_string()),
                    };

                    let _ = write!(
                        html,
                        r#"<tr><td>{coin} Up/Down</td><td class="text-right">{up}</td><td class="text-right">{down}</td><td class="text-right">{fee}</td><td class="text-right">{net}</td><td class="text-right">{time}</td></tr>"#,
                        coin = escape_html(&mwr.coin),
                        up = up_price,
                        down = down_price,
                        fee = fee_str,
                        net = net_str,
                        time = time_str,
                    );
                }
                html.push_str("</tbody></table>");
            }
            html.push_str("</div>");
        }

        // Market confidence scores
        {
            let active_markets = self.base.active_markets.read().await;
            let price_history = self.base.price_history.read().await;
            let cached_asks = self.base.cached_asks.read().await;
            let now = Utc::now();

            let mut eligible: Vec<_> = active_markets.values().collect();
            eligible.sort_by(|a, b| a.coin.cmp(&b.coin));

            html.push_str(r#"<div class="bp-card mb-4">"#);
            let _ = write!(
                html,
                r#"<h2 class="bp-section-title">Market Confidence ({})</h2>"#,
                eligible.len()
            );

            if eligible.is_empty() {
                html.push_str(r#"<p class="bp-text-muted">No active markets</p>"#);
            } else {
                html.push_str(r#"<table class="bp-table"><thead><tr>"#);
                html.push_str("<th class=\"text-left\">Coin</th>");
                html.push_str("<th class=\"text-right\">Time Left</th>");
                html.push_str("<th class=\"text-right\">Ask</th>");
                html.push_str("<th class=\"text-right\">Conf</th>");
                html.push_str("<th class=\"text-right\">Quality</th>");
                html.push_str("</tr></thead><tbody>");

                for mwr in &eligible {
                    let time_remaining = mwr.market.seconds_remaining_at(now);
                    let current_price = price_history
                        .get(&mwr.coin)
                        .and_then(|h| h.back().map(|(_, p, _)| *p));

                    let (ask_str, conf_str, conf_style) = match current_price {
                        Some(cp) => {
                            let prediction = mwr.predict_winner(cp);
                            let token_id = match prediction {
                                Some(OutcomeSide::Up) | Some(OutcomeSide::Yes) => {
                                    &mwr.market.token_ids.outcome_a
                                }
                                _ => &mwr.market.token_ids.outcome_b,
                            };
                            let ask = cached_asks.get(token_id).copied();
                            let ask_display =
                                ask.map(fmt_market_price).unwrap_or_else(|| "-".to_string());

                            match ask {
                                Some(market_price) => {
                                    let confidence =
                                        mwr.get_confidence(cp, market_price, time_remaining);
                                    let pct = confidence * Decimal::new(100, 0);
                                    let style = if confidence >= Decimal::new(90, 2) {
                                        "color:var(--color-enabled)"
                                    } else if confidence >= Decimal::new(70, 2) {
                                        "color:var(--color-warning)"
                                    } else {
                                        "color:var(--text-secondary)"
                                    };
                                    (ask_display, format!("{:.1}%", pct), style)
                                }
                                None => {
                                    (ask_display, "-".to_string(), "color:var(--text-secondary)")
                                }
                            }
                        }
                        None => (
                            "-".to_string(),
                            "-".to_string(),
                            "color:var(--text-secondary)",
                        ),
                    };

                    let quality_factor =
                        mwr.reference_quality.quality_factor() * Decimal::new(100, 0);
                    let quality_label = match mwr.reference_quality {
                        ReferenceQuality::Exact => "Exact",
                        ReferenceQuality::OnChain(_) => "OnChain",
                        ReferenceQuality::Historical(_) => "Historical",
                        ReferenceQuality::Current => "Current",
                    };

                    let _ = write!(
                        html,
                        r#"<tr><td>{coin}</td><td class="text-right">{time}s</td><td class="text-right">{ask}</td><td class="text-right" style="{conf_style}">{conf}</td><td class="text-right">{qlabel} ({qfactor:.0}%)</td></tr>"#,
                        coin = escape_html(&mwr.coin),
                        time = time_remaining,
                        ask = ask_str,
                        conf_style = conf_style,
                        conf = conf_str,
                        qlabel = quality_label,
                        qfactor = quality_factor,
                    );
                }
                html.push_str("</tbody></table>");
            }
            html.push_str("</div>");
        }

        // Reference prices (shared context)
        render_reference_prices(&self.base, &mut html).await;

        // Positions for this mode
        render_positions_for_mode(&self.base, &mut html, "tailend").await;

        // Performance stats
        render_performance_for_mode(&self.base, &mut html, "tailend").await;

        Ok(html)
    }
}

// ---------------------------------------------------------------------------
// TwoSided Dashboard
// ---------------------------------------------------------------------------

/// TwoSided mode dashboard at `/strategy/crypto-arb-twosided`.
pub struct TwoSidedDashboard {
    base: Arc<CryptoArbBase>,
}

impl TwoSidedDashboard {
    pub fn new(base: Arc<CryptoArbBase>) -> Self {
        Self { base }
    }
}

impl DashboardViewProvider for TwoSidedDashboard {
    fn view_name(&self) -> &str {
        "crypto-arb-twosided"
    }

    fn render_view(
        &self,
    ) -> Pin<Box<dyn Future<Output = polyrust_core::error::Result<String>> + Send + '_>> {
        Box::pin(self.render_view_impl())
    }
}

impl TwoSidedDashboard {
    async fn render_view_impl(&self) -> polyrust_core::error::Result<String> {
        let mut html = String::with_capacity(4096);

        // Header with back link
        html.push_str(r#"<div class="mb-4">"#);
        html.push_str(
            r#"<a href="/strategy/crypto-arb" class="bp-back-link">&larr; Back to Overview</a>"#,
        );
        html.push_str(r#"<h1 class="bp-page-title mt-2">TwoSided Mode</h1>"#);

        let status = if self.base.config.twosided.enabled {
            r#"<span style="color:var(--color-enabled)">Enabled</span>"#
        } else {
            r#"<span class="bp-text-muted">Disabled</span>"#
        };
        let _ = write!(
            html,
            r#"<p class="bp-text-secondary" style="font-size:0.95rem">Status: {}</p>"#,
            status
        );
        html.push_str("</div>");

        // Config summary
        html.push_str(r#"<div class="bp-card mb-4">"#);
        html.push_str(r#"<h2 class="bp-section-title">Configuration</h2>"#);
        html.push_str(r#"<div class="grid grid-cols-2 gap-2" style="font-size:0.95rem">"#);
        let _ = write!(
            html,
            r#"<div class="bp-config-label">Combined threshold:</div><div class="bp-config-value">&lt; {} (both outcomes)</div>"#,
            self.base.config.twosided.combined_threshold
        );
        html.push_str("</div></div>");

        // Reference prices
        render_reference_prices(&self.base, &mut html).await;

        // Two-sided opportunities (markets where both asks sum < threshold)
        html.push_str(r#"<div class="bp-card mb-4">"#);
        html.push_str(r#"<h2 class="bp-section-title">Current Opportunities</h2>"#);

        let active_markets = self.base.active_markets.read().await;
        let cached_asks = self.base.cached_asks.read().await;
        let threshold = self.base.config.twosided.combined_threshold;

        let opportunities: Vec<_> = active_markets
            .values()
            .filter_map(|mwr| {
                let up_ask = cached_asks.get(&mwr.market.token_ids.outcome_a)?;
                let down_ask = cached_asks.get(&mwr.market.token_ids.outcome_b)?;
                let combined = *up_ask + *down_ask;
                if combined < threshold {
                    Some((mwr, *up_ask, *down_ask, combined))
                } else {
                    None
                }
            })
            .collect();

        if opportunities.is_empty() {
            html.push_str(r#"<p class="bp-text-muted">No two-sided opportunities found</p>"#);
        } else {
            html.push_str(r#"<table class="bp-table"><thead><tr>"#);
            html.push_str("<th class=\"text-left\">Market</th>");
            html.push_str("<th class=\"text-right\">UP Ask</th>");
            html.push_str("<th class=\"text-right\">DOWN Ask</th>");
            html.push_str("<th class=\"text-right\">Combined</th>");
            html.push_str("<th class=\"text-right\">Edge</th>");
            html.push_str("</tr></thead><tbody>");

            for (mwr, up_ask, down_ask, combined) in &opportunities {
                let edge = Decimal::ONE - *combined;
                let _ = write!(
                    html,
                    r#"<tr><td>{coin}</td><td class="text-right">{up:.2}</td><td class="text-right">{down:.2}</td><td class="text-right">{combined:.2}</td><td class="text-right bp-profit">{edge:.2}</td></tr>"#,
                    coin = escape_html(&mwr.coin),
                    up = up_ask,
                    down = down_ask,
                    combined = combined,
                    edge = edge,
                );
            }
            html.push_str("</tbody></table>");
        }
        html.push_str("</div>");

        drop(active_markets);
        drop(cached_asks);

        // Positions
        render_positions_for_mode(&self.base, &mut html, "twosided").await;

        // Performance
        render_performance_for_mode(&self.base, &mut html, "twosided").await;

        Ok(html)
    }
}

// ---------------------------------------------------------------------------
// SSE Dashboard Update Emission
// ---------------------------------------------------------------------------

/// Emit SSE dashboard-update signals for all views if the shared throttle allows.
///
/// Each signal carries pre-rendered HTML so the SSE handler can broadcast it
/// without re-acquiring strategy locks. Called at the end of each strategy's
/// `on_event()` — the shared 5-second throttle ensures only one strategy per
/// window triggers the render.
pub async fn try_emit_dashboard_updates(base: &Arc<CryptoArbBase>) -> Vec<Action> {
    if !base.try_claim_dashboard_emit().await {
        return vec![];
    }

    let providers: Vec<Box<dyn DashboardViewProvider>> = vec![
        Box::new(CryptoArbDashboard::new(Arc::clone(base))),
        Box::new(TailEndDashboard::new(Arc::clone(base))),
        Box::new(TwoSidedDashboard::new(Arc::clone(base))),
    ];

    let mut actions = Vec::with_capacity(providers.len());
    for provider in &providers {
        if let Ok(html) = provider.render_view().await {
            actions.push(Action::EmitSignal {
                signal_type: "dashboard-update".to_string(),
                payload: serde_json::json!({
                    "view_name": provider.view_name(),
                    "rendered_html": html,
                }),
            });
        }
    }
    actions
}
