//! Dashboard view for the crypto arbitrage strategy.
//!
//! Single dashboard at `/strategy/crypto-arb` combining overview, positions,
//! performance stats, config summary, and skip-reason diagnostics.

mod render;
mod updates;

pub use updates::try_emit_dashboard_updates;

use std::fmt::Write as FmtWrite;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rust_decimal::Decimal;

use polyrust_core::prelude::*;

use crate::crypto_arb::runtime::CryptoArbRuntime;
use crate::crypto_arb::services::{escape_html, fmt_market_price, net_profit_margin, taker_fee};

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

/// Dashboard view for the crypto arbitrage strategy at `/strategy/crypto-arb`.
pub struct CryptoArbDashboard {
    pub(super) base: Arc<CryptoArbRuntime>,
}

impl CryptoArbDashboard {
    pub fn new(base: Arc<CryptoArbRuntime>) -> Self {
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
            let _ = write!(
                html,
                r#"<div class="bp-config-label">Time window:</div><div class="bp-config-value">&lt; {}s</div>"#,
                te.time_threshold_secs
            );
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

            html.push_str("</div></details>");
        }

        // --- Reference Prices ---
        render::render_reference_prices(&self.base, &mut html).await;

        // --- Active Markets ---
        {
            let active_markets = self.base.active_markets.read().await;
            let cached_asks = self.base.cached_asks.read().await;
            let fee_rate = self.base.config.fee.taker_fee_rate;

            html.push_str(r#"<div class="bp-card mb-4">"#);
            let _ = write!(
                html,
                r#"<h2 class="bp-section-title">Active Markets ({})</h2>"#,
                active_markets.len()
            );

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

        // --- Open Positions ---
        render::render_positions(&self.base, &mut html).await;

        // --- Performance Stats ---
        render::render_performance(&self.base, &mut html).await;

        // --- Skip Reason Chart ---
        render::render_skip_stats(&self.base, &mut html);

        Ok(html)
    }
}
