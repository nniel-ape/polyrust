//! TailEnd strategy: High-confidence trades near market expiration.
//!
//! Entry conditions:
//! - Time remaining < 120 seconds
//! - Predicted winner's ask >= 0.90
//! - Confidence: 1.0 (fixed, highest priority)
//!
//! Uses GTC orders with aggressive pricing (above ask) for immediate fills.
//! Taker fee at TailEnd prices (0.90-0.99) is negligible (0.06-0.57%).

mod entry;
mod exit;
mod order_events;

use std::sync::Arc;

use async_trait::async_trait;
use tracing::info;

use polyrust_core::prelude::*;

use crate::crypto_arb::dashboard::try_emit_dashboard_updates;
use crate::crypto_arb::runtime::CryptoArbRuntime;

/// TailEnd strategy: trades near expiration with high market prices.
pub struct TailEndStrategy {
    pub(crate) base: Arc<CryptoArbRuntime>,
}

impl TailEndStrategy {
    pub fn new(base: Arc<CryptoArbRuntime>) -> Self {
        // Validate config at construction time as defense-in-depth.
        // main.rs should call ArbitrageConfig::validate() first for graceful errors.
        if let Err(e) = base.config.validate() {
            panic!("Invalid arbitrage config: {e}");
        }
        Self { base }
    }

    /// Get the dynamic ask threshold based on time remaining.
    /// Uses the tightest (highest) threshold where time_remaining <= bucket threshold.
    /// Falls back to legacy ask_threshold if no dynamic thresholds match.
    #[cfg(test)]
    pub(crate) fn get_ask_threshold(&self, time_remaining_secs: i64) -> rust_decimal::Decimal {
        self.get_ask_threshold_impl(time_remaining_secs)
    }
}

#[async_trait]
impl Strategy for TailEndStrategy {
    fn name(&self) -> &str {
        "crypto-arb-tailend"
    }

    fn description(&self) -> &str {
        "Tail-end arbitrage: trades near expiration with high market prices"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        info!(
            coins = ?self.base.config.coins,
            "TailEnd strategy started"
        );
        Ok(())
    }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        self.base.update_event_time(ctx).await;

        let mut actions = match event {
            Event::MarketData(MarketDataEvent::MarketDiscovered(market)) => {
                self.base.on_market_discovered(market, ctx).await
            }

            Event::MarketData(MarketDataEvent::MarketExpired(id)) => {
                self.base.on_market_expired(id).await
            }

            Event::MarketData(MarketDataEvent::ExternalPrice {
                symbol,
                price,
                source,
                timestamp,
            }) => {
                self.handle_external_price(symbol, *price, source, *timestamp, ctx)
                    .await
            }

            Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot)) => {
                self.handle_orderbook_update(snapshot).await
            }

            Event::OrderUpdate(OrderEvent::Placed(result)) => self.on_order_placed(result).await,

            Event::OrderUpdate(OrderEvent::Filled {
                order_id,
                token_id,
                price,
                size,
                ..
            }) => {
                self.on_order_filled(order_id, token_id, *price, *size)
                    .await
            }

            Event::OrderUpdate(OrderEvent::PartiallyFilled {
                order_id,
                filled_size,
                remaining_size,
            }) => {
                self.handle_partially_filled(order_id, *filled_size, *remaining_size)
                    .await
            }

            Event::OrderUpdate(OrderEvent::Rejected {
                token_id, reason, ..
            }) => self.handle_rejected(token_id.as_deref(), reason).await?,

            Event::OrderUpdate(OrderEvent::Cancelled(order_id)) => {
                self.handle_cancelled(order_id).await?
            }

            Event::OrderUpdate(OrderEvent::CancelFailed { order_id, reason }) => {
                self.handle_cancel_failed(order_id, reason).await
            }

            Event::System(SystemEvent::OpenOrderSnapshot(ids)) => {
                self.handle_open_order_snapshot(ids).await
            }

            _ => vec![],
        };

        // Check stale limit orders (TailEnd uses GTC for entries)
        actions.extend(self.base.check_stale_limit_orders().await);

        // Emit SSE dashboard updates (throttled to ~5s across all strategies)
        actions.extend(try_emit_dashboard_updates(&self.base).await);

        // Periodic pipeline status summary (every 60s)
        self.base.maybe_log_status_summary().await;

        Ok(actions)
    }

    async fn on_stop(&mut self, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        info!("TailEnd strategy stopping");
        Ok(vec![])
    }

    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        None // Uses shared dashboard
    }
}
