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
    html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
    html.push_str(r#"<h2 class="text-lg font-bold mb-3">Reference Prices &amp; Predictions</h2>"#);

    let active_markets = base.active_markets.read().await;
    let price_history = base.price_history.read().await;

    if active_markets.is_empty() {
        html.push_str(r#"<p class="text-gray-500">No active markets</p>"#);
    } else {
        html.push_str(
            r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#,
        );
        html.push_str("<th class=\"text-left py-1\">Coin</th>");
        html.push_str("<th class=\"text-right py-1\">Ref Price</th>");
        html.push_str("<th class=\"text-right py-1\">Current</th>");
        html.push_str("<th class=\"text-right py-1\">Change</th>");
        html.push_str("<th class=\"text-right py-1\">Prediction</th>");
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
                        "pnl-positive"
                    } else {
                        "pnl-negative"
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
                r#"<tr class="border-b border-gray-800"><td class="py-1">{coin}</td><td class="text-right py-1">{ref_label}{ref_price}</td><td class="text-right py-1">{current}</td><td class="text-right py-1 {change_class}">{change}</td><td class="text-right py-1 font-bold">{prediction}</td></tr>"#,
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

    html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
    let _ = write!(
        html,
        r#"<h2 class="text-lg font-bold mb-3">Open Positions ({})</h2>"#,
        mode_positions.len()
    );

    if mode_positions.is_empty() {
        html.push_str(r#"<p class="text-gray-500">No open positions</p>"#);
    } else {
        html.push_str(
            r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#,
        );
        html.push_str("<th class=\"text-left py-1\">Market</th>");
        html.push_str("<th class=\"text-left py-1\">Side</th>");
        html.push_str("<th class=\"text-right py-1\">Entry</th>");
        html.push_str("<th class=\"text-right py-1\">Current</th>");
        html.push_str("<th class=\"text-right py-1\">PnL</th>");
        html.push_str("<th class=\"text-right py-1\">Size</th>");
        html.push_str("<th class=\"text-right py-1\">Kelly</th>");
        html.push_str("</tr></thead><tbody>");

        for pos in &mode_positions {
            let current = cached_asks.get(&pos.token_id).copied();
            let (current_str, pnl_str, pnl_class) = match current {
                Some(cp) => {
                    let pnl = (cp - pos.entry_price) * pos.size - (pos.estimated_fee * pos.size);
                    let cls = if pnl >= Decimal::ZERO {
                        "pnl-positive"
                    } else {
                        "pnl-negative"
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
                r#"<tr class="border-b border-gray-800"><td class="py-1">{coin}</td><td class="py-1">{side:?}</td><td class="text-right py-1">{entry}</td><td class="text-right py-1">{current}</td><td class="text-right py-1"><span class="{pnl_class}">{pnl}</span></td><td class="text-right py-1">{size}</td><td class="text-right py-1">{kelly}</td></tr>"#,
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

    html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
    html.push_str(r#"<h2 class="text-lg font-bold mb-3">Performance Stats</h2>"#);

    // Find stats for this mode
    let stats = mode_stats
        .iter()
        .find(|(m, _)| mode_matches(m, mode_filter))
        .map(|(_, s)| s);

    match stats {
        Some(s) => {
            html.push_str(
                r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#,
            );
            html.push_str("<th class=\"text-right py-1\">Trades</th>");
            html.push_str("<th class=\"text-right py-1\">Won</th>");
            html.push_str("<th class=\"text-right py-1\">Lost</th>");
            html.push_str("<th class=\"text-right py-1\">Win Rate</th>");
            html.push_str("<th class=\"text-right py-1\">Total P&amp;L</th>");
            html.push_str("<th class=\"text-right py-1\">Avg P&amp;L</th>");
            html.push_str("</tr></thead><tbody>");

            let win_rate_pct = s.win_rate() * Decimal::new(100, 0);
            let pnl_class = if s.total_pnl >= Decimal::ZERO {
                "pnl-positive"
            } else {
                "pnl-negative"
            };

            let _ = write!(
                html,
                r#"<tr class="border-b border-gray-800"><td class="text-right py-1">{trades}</td><td class="text-right py-1">{won}</td><td class="text-right py-1">{lost}</td><td class="text-right py-1">{win_rate:.1}%</td><td class="text-right py-1 {pnl_class}">${total_pnl:.2}</td><td class="text-right py-1">${avg_pnl:.4}</td></tr>"#,
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
            html.push_str(r#"<p class="text-gray-500">No trades recorded yet</p>"#);
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
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        html.push_str(r#"<h2 class="text-lg font-bold mb-3">Trading Modes</h2>"#);
        html.push_str(r#"<div class="grid grid-cols-2 gap-4">"#);

        let modes = [
            ("TailEnd", "tailend", self.base.config.tailend.enabled),
            ("TwoSided", "twosided", self.base.config.twosided.enabled),
        ];

        for (name, slug, enabled) in &modes {
            let status_class = if *enabled {
                "text-green-400"
            } else {
                "text-gray-500"
            };
            let status_text = if *enabled { "Enabled" } else { "Disabled" };

            let _ = write!(
                html,
                r#"<a href="/strategy/crypto-arb-{slug}" class="block p-3 bg-gray-800 rounded hover:bg-gray-700 transition-colors"><div class="font-bold">{name}</div><div class="{status_class} text-sm">{status_text}</div></a>"#,
                slug = slug,
                name = name,
                status_class = status_class,
                status_text = status_text,
            );
        }
        html.push_str("</div></div>");

        // --- Reference Prices ---
        render_reference_prices(&self.base, &mut html).await;

        // --- Active Markets ---
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        let active_markets = self.base.active_markets.read().await;
        let _ = write!(
            html,
            r#"<h2 class="text-lg font-bold mb-3">Active Markets ({})</h2>"#,
            active_markets.len()
        );

        let cached_asks = self.base.cached_asks.read().await;

        if active_markets.is_empty() {
            html.push_str(r#"<p class="text-gray-500">No active markets</p>"#);
        } else {
            html.push_str(
                r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#,
            );
            html.push_str("<th class=\"text-left py-1\">Market</th>");
            html.push_str("<th class=\"text-right py-1\">UP</th>");
            html.push_str("<th class=\"text-right py-1\">DOWN</th>");
            html.push_str("<th class=\"text-right py-1\">Fee</th>");
            html.push_str("<th class=\"text-right py-1\">Net</th>");
            html.push_str("<th class=\"text-right py-1\">Time Left</th>");
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
                    r#"<tr class="border-b border-gray-800"><td class="py-1">{coin} Up/Down</td><td class="text-right py-1">{up}</td><td class="text-right py-1">{down}</td><td class="text-right py-1">{fee}</td><td class="text-right py-1">{net}</td><td class="text-right py-1">{time}</td></tr>"#,
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
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        let total_positions: usize = positions.values().map(|v| v.len()).sum();
        let _ = write!(
            html,
            r#"<h2 class="text-lg font-bold mb-3">Open Positions ({})</h2>"#,
            total_positions
        );

        if positions.is_empty() {
            html.push_str(r#"<p class="text-gray-500">No open positions</p>"#);
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
                    r#"<span class="px-2 py-1 bg-gray-800 rounded text-sm">{mode}: {count}</span>"#,
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
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        html.push_str(r#"<h2 class="text-lg font-bold mb-3">Performance Stats</h2>"#);

        if mode_stats.is_empty() {
            html.push_str(r#"<p class="text-gray-500">No trades recorded yet</p>"#);
        } else {
            html.push_str(
                r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#,
            );
            html.push_str("<th class=\"text-left py-1\">Mode</th>");
            html.push_str("<th class=\"text-right py-1\">Trades</th>");
            html.push_str("<th class=\"text-right py-1\">Won</th>");
            html.push_str("<th class=\"text-right py-1\">Lost</th>");
            html.push_str("<th class=\"text-right py-1\">Win Rate</th>");
            html.push_str("<th class=\"text-right py-1\">Total P&amp;L</th>");
            html.push_str("<th class=\"text-right py-1\">Avg P&amp;L</th>");
            html.push_str("<th class=\"text-left py-1\">Status</th>");
            html.push_str("</tr></thead><tbody>");

            let mut modes: Vec<_> = mode_stats.iter().collect();
            modes.sort_by(|a, b| a.0.to_string().cmp(&b.0.to_string()));

            for (mode, stats) in &modes {
                let win_rate_pct = stats.win_rate() * Decimal::new(100, 0);
                let pnl_class = if stats.total_pnl >= Decimal::ZERO {
                    "pnl-positive"
                } else {
                    "pnl-negative"
                };
                let status = if self.base.is_mode_disabled(mode).await {
                    r#"<span class="text-red-400">Disabled</span>"#
                } else {
                    r#"<span class="text-green-400">Active</span>"#
                };
                let _ = write!(
                    html,
                    r#"<tr class="border-b border-gray-800"><td class="py-1">{mode}</td><td class="text-right py-1">{trades}</td><td class="text-right py-1">{won}</td><td class="text-right py-1">{lost}</td><td class="text-right py-1">{win_rate:.1}%</td><td class="text-right py-1 {pnl_class}">${total_pnl:.2}</td><td class="text-right py-1">${avg_pnl:.4}</td><td class="py-1">{status}</td></tr>"#,
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
        html.push_str(r#"<a href="/strategy/crypto-arb" class="text-blue-400 hover:underline">&larr; Back to Overview</a>"#);
        html.push_str(r#"<h1 class="text-xl font-bold mt-2">TailEnd Mode</h1>"#);

        // Status
        let status = if self.base.config.tailend.enabled {
            r#"<span class="text-green-400">Enabled</span>"#
        } else {
            r#"<span class="text-gray-500">Disabled</span>"#
        };
        let _ = write!(
            html,
            r#"<p class="text-sm text-gray-400">Status: {}</p>"#,
            status
        );
        html.push_str("</div>");

        // Config summary
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        html.push_str(r#"<h2 class="text-lg font-bold mb-3">Configuration</h2>"#);
        html.push_str(r#"<div class="grid grid-cols-2 gap-2 text-sm">"#);
        let _ = write!(
            html,
            r#"<div class="text-gray-400">Time threshold:</div><div>&lt; {}s remaining</div>"#,
            self.base.config.tailend.time_threshold_secs
        );
        let _ = write!(
            html,
            r#"<div class="text-gray-400">Ask threshold:</div><div>&ge; {}</div>"#,
            self.base.config.tailend.ask_threshold
        );
        html.push_str("</div></div>");

        // Active markets
        {
            let active_markets = self.base.active_markets.read().await;
            let cached_asks = self.base.cached_asks.read().await;
            let fee_rate = self.base.config.fee.taker_fee_rate;

            let mut markets_by_time: Vec<_> = active_markets.values().collect();
            markets_by_time.sort_by_key(|m| m.market.end_date);

            html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
            let _ = write!(
                html,
                r#"<h2 class="text-lg font-bold mb-3">Active Markets ({})</h2>"#,
                markets_by_time.len()
            );

            if markets_by_time.is_empty() {
                html.push_str(r#"<p class="text-gray-500">No active markets</p>"#);
            } else {
                html.push_str(
                    r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#,
                );
                html.push_str("<th class=\"text-left py-1\">Market</th>");
                html.push_str("<th class=\"text-right py-1\">UP</th>");
                html.push_str("<th class=\"text-right py-1\">DOWN</th>");
                html.push_str("<th class=\"text-right py-1\">Fee</th>");
                html.push_str("<th class=\"text-right py-1\">Net</th>");
                html.push_str("<th class=\"text-right py-1\">Time Left</th>");
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
                        r#"<tr class="border-b border-gray-800"><td class="py-1">{coin} Up/Down</td><td class="text-right py-1">{up}</td><td class="text-right py-1">{down}</td><td class="text-right py-1">{fee}</td><td class="text-right py-1">{net}</td><td class="text-right py-1">{time}</td></tr>"#,
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

            html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
            let _ = write!(
                html,
                r#"<h2 class="text-lg font-bold mb-3">Market Confidence ({})</h2>"#,
                eligible.len()
            );

            if eligible.is_empty() {
                html.push_str(r#"<p class="text-gray-500">No active markets</p>"#);
            } else {
                html.push_str(
                    r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#,
                );
                html.push_str("<th class=\"text-left py-1\">Coin</th>");
                html.push_str("<th class=\"text-right py-1\">Time Left</th>");
                html.push_str("<th class=\"text-right py-1\">Ask</th>");
                html.push_str("<th class=\"text-right py-1\">Confidence</th>");
                html.push_str("<th class=\"text-right py-1\">Quality</th>");
                html.push_str("</tr></thead><tbody>");

                for mwr in &eligible {
                    let time_remaining = mwr.market.seconds_remaining_at(now);
                    let current_price = price_history
                        .get(&mwr.coin)
                        .and_then(|h| h.back().map(|(_, p, _)| *p));

                    let (ask_str, conf_str, conf_class) = match current_price {
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
                                    let cls = if confidence >= Decimal::new(90, 2) {
                                        "text-green-400"
                                    } else if confidence >= Decimal::new(70, 2) {
                                        "text-yellow-400"
                                    } else {
                                        "text-gray-400"
                                    };
                                    (ask_display, format!("{:.1}%", pct), cls)
                                }
                                None => (ask_display, "-".to_string(), "text-gray-400"),
                            }
                        }
                        None => ("-".to_string(), "-".to_string(), "text-gray-400"),
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
                        r#"<tr class="border-b border-gray-800"><td class="py-1">{coin}</td><td class="text-right py-1">{time}s</td><td class="text-right py-1">{ask}</td><td class="text-right py-1 {conf_class}">{conf}</td><td class="text-right py-1">{qlabel} ({qfactor:.0}%)</td></tr>"#,
                        coin = escape_html(&mwr.coin),
                        time = time_remaining,
                        ask = ask_str,
                        conf_class = conf_class,
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
        html.push_str(r#"<a href="/strategy/crypto-arb" class="text-blue-400 hover:underline">&larr; Back to Overview</a>"#);
        html.push_str(r#"<h1 class="text-xl font-bold mt-2">TwoSided Mode</h1>"#);

        let status = if self.base.config.twosided.enabled {
            r#"<span class="text-green-400">Enabled</span>"#
        } else {
            r#"<span class="text-gray-500">Disabled</span>"#
        };
        let _ = write!(
            html,
            r#"<p class="text-sm text-gray-400">Status: {}</p>"#,
            status
        );
        html.push_str("</div>");

        // Config summary
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        html.push_str(r#"<h2 class="text-lg font-bold mb-3">Configuration</h2>"#);
        html.push_str(r#"<div class="grid grid-cols-2 gap-2 text-sm">"#);
        let _ = write!(
            html,
            r#"<div class="text-gray-400">Combined threshold:</div><div>&lt; {} (both outcomes)</div>"#,
            self.base.config.twosided.combined_threshold
        );
        html.push_str("</div></div>");

        // Reference prices
        render_reference_prices(&self.base, &mut html).await;

        // Two-sided opportunities (markets where both asks sum < threshold)
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        html.push_str(r#"<h2 class="text-lg font-bold mb-3">Current Opportunities</h2>"#);

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
            html.push_str(r#"<p class="text-gray-500">No two-sided opportunities found</p>"#);
        } else {
            html.push_str(
                r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#,
            );
            html.push_str("<th class=\"text-left py-1\">Market</th>");
            html.push_str("<th class=\"text-right py-1\">UP Ask</th>");
            html.push_str("<th class=\"text-right py-1\">DOWN Ask</th>");
            html.push_str("<th class=\"text-right py-1\">Combined</th>");
            html.push_str("<th class=\"text-right py-1\">Edge</th>");
            html.push_str("</tr></thead><tbody>");

            for (mwr, up_ask, down_ask, combined) in &opportunities {
                let edge = Decimal::ONE - *combined;
                let _ = write!(
                    html,
                    r#"<tr class="border-b border-gray-800"><td class="py-1">{coin}</td><td class="text-right py-1">{up:.2}</td><td class="text-right py-1">{down:.2}</td><td class="text-right py-1">{combined:.2}</td><td class="text-right py-1 pnl-positive">{edge:.2}</td></tr>"#,
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
