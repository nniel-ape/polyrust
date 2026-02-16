//! Tests for the crypto arbitrage strategies.
//!
//! Tests are organized by domain area:
//! - test_domain: ReferenceQuality, MarketWithReference, ModeStats
//! - test_config: Config defaults, deserialization, validation
//! - test_pricing: Price history, composite, spike, reference, boundary
//! - test_markets: Market reservation, lifecycle, coin extraction
//! - test_orders: Order reconciliation, cooldowns, rejection classification
//! - test_lifecycle: Position lifecycle FSM, evaluate_triggers
//! - test_tailend: TailEnd integration (entry, exits, PnL, fast-path)

mod test_config;
mod test_domain;
mod test_lifecycle;
mod test_markets;
mod test_orders;
mod test_pricing;
mod test_tailend;

// ---------------------------------------------------------------------------
// Shared test helpers
// ---------------------------------------------------------------------------

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use polyrust_core::prelude::*;

use super::config::ArbitrageConfig;
use super::domain::{ArbitragePosition, MarketWithReference, OpenLimitOrder, ReferenceQuality};
use super::runtime::CryptoArbRuntime;

pub(super) fn make_market_info(id: &str, end_date: DateTime<Utc>) -> MarketInfo {
    MarketInfo {
        id: id.to_string(),
        slug: "btc-up-down".to_string(),
        question: "Will BTC go up?".to_string(),
        start_date: None,
        end_date,
        token_ids: TokenIds {
            outcome_a: "token_up".to_string(),
            outcome_b: "token_down".to_string(),
        },
        accepting_orders: true,
        neg_risk: false,
        min_order_size: dec!(5.0), // 5.0 shares default
        tick_size: dec!(0.01),     // 0.01 default
        fee_rate_bps: 0,
    }
}

pub(super) fn make_mwr(reference_price: Decimal, time_remaining_secs: i64) -> MarketWithReference {
    MarketWithReference {
        market: make_market_info(
            "market1",
            Utc::now() + Duration::seconds(time_remaining_secs),
        ),
        reference_price,
        reference_quality: ReferenceQuality::Exact,
        discovery_time: Utc::now(),
        coin: "BTC".to_string(),
        window_ts: 0,
    }
}

pub(super) fn make_base_no_chainlink() -> Arc<CryptoArbRuntime> {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    Arc::new(CryptoArbRuntime::new(config, vec![]))
}

/// Helper to create an ArbitragePosition with controlled parameters.
pub(super) fn make_position(
    market_id: &str,
    token_id: &str,
    side: OutcomeSide,
    entry_price: Decimal,
    size: Decimal,
    reference_price: Decimal,
    peak_price: Decimal,
) -> ArbitragePosition {
    ArbitragePosition {
        market_id: market_id.to_string(),
        token_id: token_id.to_string(),
        side,
        entry_price,
        size,
        reference_price,
        coin: "BTC".to_string(),
        order_id: None,
        entry_time: Utc::now(),
        kelly_fraction: None,
        peak_price,

        estimated_fee: Decimal::ZERO,
        entry_market_price: entry_price,
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
        entry_order_type: OrderType::Gtc,
        entry_fee_per_share: Decimal::ZERO,
        recovery_cost: Decimal::ZERO,
    }
}

/// Helper to set up a base with an active market having a known end_date.
pub(super) async fn make_base_with_market(
    market_id: &str,
    time_remaining_secs: i64,
) -> Arc<CryptoArbRuntime> {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.stop_loss.reversal_pct = dec!(0.005); // 0.5%
    config.stop_loss.min_drop = dec!(0.05); // 5¢
    config.stop_loss.trailing_enabled = true;
    config.stop_loss.trailing_distance = dec!(0.03);
    config.stop_loss.time_decay = true;
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    // Insert active market
    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            market_id.to_string(),
            MarketWithReference {
                market: make_market_info(
                    market_id,
                    Utc::now() + Duration::seconds(time_remaining_secs),
                ),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Exact,
                discovery_time: Utc::now(),
                coin: "BTC".to_string(),
                window_ts: 0,
            },
        );
    }

    base
}

pub(super) fn make_open_limit_order(
    order_id: &str,
    market_id: &str,
    token_id: &str,
) -> OpenLimitOrder {
    OpenLimitOrder {
        order_id: order_id.to_string(),
        market_id: market_id.to_string(),
        token_id: token_id.to_string(),
        side: OutcomeSide::Up,
        price: dec!(0.92),
        size: dec!(10),
        reference_price: dec!(50000),
        coin: "BTC".to_string(),
        placed_at: Utc::now(),

        kelly_fraction: None,
        estimated_fee: Decimal::ZERO,
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
        cancel_pending: false,
        reconcile_miss_count: 0,
    }
}
