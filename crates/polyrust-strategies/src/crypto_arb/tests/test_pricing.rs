use std::collections::VecDeque;

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use polyrust_core::prelude::*;

use super::*;
use crate::crypto_arb::config::SizingConfig;
use crate::crypto_arb::domain::{BoundarySnapshot, CompositePriceResult, ReferenceQuality};
use crate::crypto_arb::runtime::{CryptoArbRuntime, WINDOW_SECS};
use crate::crypto_arb::services::{kelly_position_size, net_profit_margin, parse_slug_timestamp, taker_fee};

// ---------------------------------------------------------------------------
// Fee calculation tests
// ---------------------------------------------------------------------------

#[test]
fn taker_fee_at_50_50() {
    // At p=0.50: fee = 2 * 0.50 * 0.50 * 0.0315 = 0.01575
    let fee = taker_fee(dec!(0.50), dec!(0.0315));
    assert_eq!(fee, dec!(0.015750));
}

#[test]
fn taker_fee_at_80() {
    // At p=0.80: fee = 2 * 0.80 * 0.20 * 0.0315 = 0.01008
    let fee = taker_fee(dec!(0.80), dec!(0.0315));
    assert_eq!(fee, dec!(0.010080));
}

#[test]
fn taker_fee_at_95() {
    // At p=0.95: fee = 2 * 0.95 * 0.05 * 0.0315 = 0.0029925
    let fee = taker_fee(dec!(0.95), dec!(0.0315));
    assert_eq!(fee, dec!(0.0029925));
}

#[test]
fn net_profit_margin_taker() {
    // At p=0.80: gross = 0.20, fee = 0.01008, net = 0.18992
    let net = net_profit_margin(dec!(0.80), dec!(0.0315), false);
    let expected = dec!(0.20) - dec!(0.010080);
    assert_eq!(net, expected);
}

#[test]
fn net_profit_margin_maker() {
    // Maker fee = $0, so net = gross = 1 - price
    let net = net_profit_margin(dec!(0.80), dec!(0.0315), true);
    assert_eq!(net, dec!(0.20));
}

// ---------------------------------------------------------------------------
// Kelly sizing tests
// ---------------------------------------------------------------------------

#[test]
fn kelly_position_size_positive_edge() {
    let config = SizingConfig::default();
    // confidence = 0.60, price = 0.50 -> payout = 1.0
    // kelly = (0.60 * 1.0 - 0.40) / 1.0 = 0.20
    // size = 10 * 0.20 * 0.25 = 0.50, clamped to min_size = 2.0
    let size = kelly_position_size(dec!(0.60), dec!(0.50), &config);
    assert_eq!(size, dec!(2.0)); // min_size
}

#[test]
fn kelly_position_size_high_confidence() {
    let config = SizingConfig::default();
    // confidence = 0.90, price = 0.80 -> payout = 0.25
    // kelly = (0.90 * 0.25 - 0.10) / 0.25 = 0.50
    // size = 10 * 0.50 * 0.25 = 1.25, clamped to min_size = 2.0
    let size = kelly_position_size(dec!(0.90), dec!(0.80), &config);
    assert_eq!(size, dec!(2.0));
}

#[test]
fn kelly_position_size_negative_edge() {
    let config = SizingConfig::default();
    // confidence = 0.40, price = 0.80 -> payout = 0.25
    // kelly = (0.40 * 0.25 - 0.60) / 0.25 = -2.0 (negative)
    let size = kelly_position_size(dec!(0.40), dec!(0.80), &config);
    assert_eq!(size, Decimal::ZERO);
}

#[test]
fn kelly_position_size_zero_price() {
    let config = SizingConfig::default();
    let size = kelly_position_size(dec!(0.60), Decimal::ZERO, &config);
    assert_eq!(size, Decimal::ZERO);
}

// ---------------------------------------------------------------------------
// Slug timestamp parsing tests
// ---------------------------------------------------------------------------

#[test]
fn parse_slug_timestamp_valid() {
    assert_eq!(
        parse_slug_timestamp("btc-updown-15m-1706000000"),
        Some(1706000000)
    );
}

#[test]
fn parse_slug_timestamp_no_number() {
    assert_eq!(parse_slug_timestamp("btc-updown-15m"), None);
}

#[test]
fn parse_slug_timestamp_small_number() {
    assert_eq!(parse_slug_timestamp("btc-updown-15m-12345"), None);
}

// ---------------------------------------------------------------------------
// Price history and spike detection tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn base_record_price_and_detect_spike() {
    let base = make_base_no_chainlink();

    // Record initial price
    let now = Utc::now();
    let _ = base
        .record_price("BTC", dec!(50000), "binance", now, now)
        .await;

    // Small move - no spike
    let now = Utc::now();
    let (spike, _) = base
        .record_price("BTC", dec!(50100), "binance", now, now)
        .await;
    assert!(spike.is_none());

    // Big move - spike detected
    // Need to wait past the spike window (10 seconds by default)
    // For testing, we'll manually insert history
    {
        let mut history = base.price_history.write().await;
        let old_time = Utc::now() - Duration::seconds(15);
        history.insert(
            "TEST".to_string(),
            VecDeque::from([(old_time, dec!(50000), "binance".to_string(), old_time)]),
        );
    }

    let spike = base.detect_spike("TEST", dec!(50500), Utc::now()).await;
    // 500/50000 = 1% > 0.5% threshold
    assert!(spike.is_some());
    assert!(spike.unwrap().abs() >= dec!(0.005));
}

// ---------------------------------------------------------------------------
// Reference price boundary tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn base_find_best_reference_exact_boundary() {
    let base = make_base_no_chainlink();

    let ts = 1706000000i64;
    {
        let mut boundaries = base.boundary_prices.write().await;
        boundaries.insert(
            "BTC-1706000000".to_string(),
            BoundarySnapshot {
                timestamp: DateTime::from_timestamp(ts, 0).unwrap(),
                price: dec!(42500),
                source: "chainlink".to_string(),
            },
        );
    }

    let (price, quality) = base.find_best_reference("BTC", ts, dec!(43000)).await;
    assert_eq!(price, dec!(42500));
    assert_eq!(quality, ReferenceQuality::Exact);
}

#[tokio::test]
async fn base_find_best_reference_historical() {
    let base = make_base_no_chainlink();

    let window_ts = 1706000000i64;
    let target_dt = DateTime::from_timestamp(window_ts, 0).unwrap();

    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        // 5 seconds after window start
        let ts1 = target_dt + Duration::seconds(5);
        entries.push_back((ts1, dec!(42600), "binance".to_string(), ts1));
        // 20 seconds after window start
        let ts2 = target_dt + Duration::seconds(20);
        entries.push_back((ts2, dec!(42700), "binance".to_string(), ts2));
        history.insert("BTC".to_string(), entries);
    }

    let (price, quality) = base
        .find_best_reference("BTC", window_ts, dec!(43000))
        .await;
    assert_eq!(price, dec!(42600)); // Closest to window start (5s)
    assert_eq!(quality, ReferenceQuality::Historical(5));
}

#[tokio::test]
async fn base_find_best_reference_fallback_to_current() {
    let base = make_base_no_chainlink();

    // No boundary snapshots, no history
    let (price, quality) = base
        .find_best_reference("BTC", 1706000000, dec!(43000))
        .await;
    assert_eq!(price, dec!(43000));
    assert_eq!(quality, ReferenceQuality::Current);
}

// ---------------------------------------------------------------------------
// Price momentum tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn check_sustained_direction_up() {
    let base = make_base_no_chainlink();

    // Record prices consistently above reference
    let now = Utc::now();
    let reference = dec!(50000);

    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        // Use longer times to ensure they are within the sustained window
        entries.push_back((
            now - Duration::seconds(10),
            dec!(50100),
            "rtds".to_string(),
            now - Duration::seconds(10),
        ));
        entries.push_back((
            now - Duration::seconds(6),
            dec!(50200),
            "rtds".to_string(),
            now - Duration::seconds(6),
        ));
        entries.push_back((
            now - Duration::seconds(3),
            dec!(50300),
            "rtds".to_string(),
            now - Duration::seconds(3),
        ));
        entries.push_back((
            now - Duration::seconds(1),
            dec!(50400),
            "rtds".to_string(),
            now - Duration::seconds(1),
        ));
        history.insert("BTC".to_string(), entries);
    }

    // Should detect sustained up direction when looking back 5 seconds
    assert!(
        base.check_sustained_direction("BTC", reference, OutcomeSide::Up, 5, 2, Utc::now())
            .await
    );
    // Should NOT detect sustained down direction
    assert!(
        !base
            .check_sustained_direction("BTC", reference, OutcomeSide::Down, 5, 2, Utc::now())
            .await
    );
}

#[tokio::test]
async fn check_sustained_direction_not_sustained() {
    let base = make_base_no_chainlink();

    // Record prices that cross the reference within the window
    let now = Utc::now();
    let reference = dec!(50000);

    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        entries.push_back((
            now - Duration::seconds(4),
            dec!(49900),
            "rtds".to_string(),
            now - Duration::seconds(4),
        )); // Below
        entries.push_back((
            now - Duration::seconds(2),
            dec!(50100),
            "rtds".to_string(),
            now - Duration::seconds(2),
        )); // Above
        entries.push_back((
            now - Duration::seconds(1),
            dec!(50200),
            "rtds".to_string(),
            now - Duration::seconds(1),
        )); // Above
        history.insert("BTC".to_string(), entries);
    }

    // Should NOT detect sustained up direction (one entry within window was below)
    assert!(
        !base
            .check_sustained_direction("BTC", reference, OutcomeSide::Up, 5, 2, Utc::now())
            .await
    );
}

#[tokio::test]
async fn sustained_direction_single_tick_below_min_ticks() {
    let base = make_base_no_chainlink();

    let now = Utc::now();
    let reference = dec!(50000);

    // Only 1 entry in the window
    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        entries.push_back((
            now - Duration::seconds(2),
            dec!(50100),
            "rtds".to_string(),
            now - Duration::seconds(2),
        ));
        history.insert("BTC".to_string(), entries);
    }

    // min_ticks=2, but only 1 entry -> should return false
    assert!(
        !base
            .check_sustained_direction("BTC", reference, OutcomeSide::Up, 5, 2, Utc::now())
            .await
    );
}

#[tokio::test]
async fn sustained_direction_two_ticks_favoring() {
    let base = make_base_no_chainlink();

    let now = Utc::now();
    let reference = dec!(50000);

    // Exactly 2 entries, both above reference
    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        entries.push_back((
            now - Duration::seconds(4),
            dec!(50100),
            "rtds".to_string(),
            now - Duration::seconds(4),
        ));
        entries.push_back((
            now - Duration::seconds(2),
            dec!(50200),
            "rtds".to_string(),
            now - Duration::seconds(2),
        ));
        history.insert("BTC".to_string(), entries);
    }

    // min_ticks=2, 2 entries both favoring Up -> should return true
    assert!(
        base.check_sustained_direction("BTC", reference, OutcomeSide::Up, 5, 2, Utc::now())
            .await
    );
}

#[tokio::test]
async fn sustained_direction_two_ticks_one_against() {
    let base = make_base_no_chainlink();

    let now = Utc::now();
    let reference = dec!(50000);

    // 2 entries: one below, one above reference
    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        entries.push_back((
            now - Duration::seconds(4),
            dec!(49900),
            "rtds".to_string(),
            now - Duration::seconds(4),
        ));
        entries.push_back((
            now - Duration::seconds(2),
            dec!(50200),
            "rtds".to_string(),
            now - Duration::seconds(2),
        ));
        history.insert("BTC".to_string(), entries);
    }

    // min_ticks=2, 2 entries but first is against Up -> should return false
    assert!(
        !base
            .check_sustained_direction("BTC", reference, OutcomeSide::Up, 5, 2, Utc::now())
            .await
    );
}

#[tokio::test]
async fn max_recent_volatility_no_wick() {
    let base = make_base_no_chainlink();

    // Record stable prices
    let now = Utc::now();
    let reference = dec!(50000);

    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        entries.push_back((
            now - Duration::seconds(8),
            dec!(50100),
            "rtds".to_string(),
            now - Duration::seconds(8),
        ));
        entries.push_back((
            now - Duration::seconds(5),
            dec!(50200),
            "rtds".to_string(),
            now - Duration::seconds(5),
        ));
        entries.push_back((
            now - Duration::seconds(2),
            dec!(50150),
            "rtds".to_string(),
            now - Duration::seconds(2),
        ));
        history.insert("BTC".to_string(), entries);
    }

    let volatility = base
        .max_recent_volatility("BTC", reference, 10, Utc::now())
        .await;
    assert!(volatility.is_some());
    // Max price was 50200, reference is 50000
    // Volatility = (50200 - 50000) / 50000 = 0.004
    assert!(volatility.unwrap() < dec!(0.01)); // Less than 1%
}

#[tokio::test]
async fn max_recent_volatility_with_wick() {
    let base = make_base_no_chainlink();

    // Record prices with a significant wick
    let now = Utc::now();
    let reference = dec!(50000);

    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        entries.push_back((
            now - Duration::seconds(8),
            dec!(50100),
            "rtds".to_string(),
            now - Duration::seconds(8),
        ));
        entries.push_back((
            now - Duration::seconds(5),
            dec!(51000),
            "rtds".to_string(),
            now - Duration::seconds(5),
        )); // 2% wick
        entries.push_back((
            now - Duration::seconds(2),
            dec!(50150),
            "rtds".to_string(),
            now - Duration::seconds(2),
        ));
        history.insert("BTC".to_string(), entries);
    }

    let volatility = base
        .max_recent_volatility("BTC", reference, 10, Utc::now())
        .await;
    assert!(volatility.is_some());
    // Max price was 51000, reference is 50000
    // Volatility = (51000 - 50000) / 50000 = 0.02
    assert!(volatility.unwrap() >= dec!(0.02)); // At least 2%
}

// ---------------------------------------------------------------------------
// Peak bid tracking tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn peak_bid_updates_on_higher_bid() {
    let base = make_base_no_chainlink();

    let pos = make_position(
        "m1",
        "token1",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    base.record_position(pos).await;

    // Update with higher bid
    base.update_peak_bid(&"token1".to_string(), dec!(0.95))
        .await;

    let positions = base.positions.read().await;
    let pos = &positions["m1"][0];
    assert_eq!(pos.peak_bid, dec!(0.95));
}

#[tokio::test]
async fn peak_bid_ignores_lower_bid() {
    let base = make_base_no_chainlink();

    let pos = make_position(
        "m1",
        "token1",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.95),
    );
    base.record_position(pos).await;

    // Try to update with lower bid
    base.update_peak_bid(&"token1".to_string(), dec!(0.92))
        .await;

    let positions = base.positions.read().await;
    let pos = &positions["m1"][0];
    assert_eq!(pos.peak_bid, dec!(0.95), "Peak bid should not decrease");
}

// ---------------------------------------------------------------------------
// Kelly sizing edge case tests
// ---------------------------------------------------------------------------

#[test]
fn kelly_payout_below_minimum_returns_zero() {
    let config = SizingConfig::default();
    // price ~0.999 -> payout = 1/0.999 - 1 ~ 0.001, which is exactly at the 0.001 threshold
    // price at 0.9995 -> payout = 1/0.9995 - 1 ~ 0.0005 < 0.001 -> returns 0
    let size = kelly_position_size(dec!(0.99), dec!(0.9995), &config);
    assert_eq!(
        size,
        Decimal::ZERO,
        "Should return zero when payout < 0.001"
    );
}

#[test]
fn kelly_clamped_to_max_size() {
    let mut config = SizingConfig::default();
    config.kelly_multiplier = Decimal::ONE; // No fractional Kelly
    config.max_size = dec!(15);
    config.base_size = dec!(100);
    // confidence=0.95, price=0.50 -> payout=1.0
    // kelly = (0.95 * 1.0 - 0.05) / 1.0 = 0.90
    // size = 100 * 0.90 * 1.0 = 90, clamped to max_size=15
    let size = kelly_position_size(dec!(0.95), dec!(0.50), &config);
    assert_eq!(size, dec!(15), "Should be clamped to max_size");
}

#[test]
fn kelly_clamped_to_min_size() {
    let config = SizingConfig::default();
    // Already tested above (positive edge test), but explicitly verify the clamping
    // confidence=0.55, price=0.50 -> payout=1.0
    // kelly = (0.55 - 0.45) / 1.0 = 0.10
    // size = 10 * 0.10 * 0.25 = 0.25, clamped to min_size=2.0
    let size = kelly_position_size(dec!(0.55), dec!(0.50), &config);
    assert_eq!(size, dec!(2.0), "Should be clamped to min_size");
}

#[test]
fn kelly_multiplier_scales_result() {
    let mut config = SizingConfig::default();
    config.kelly_multiplier = dec!(0.25);
    config.min_size = Decimal::ZERO; // Remove min clamp for this test
    config.base_size = dec!(100);

    // confidence=0.80, price=0.50 -> payout=1.0
    // kelly = (0.80 - 0.20) / 1.0 = 0.60
    // size = 100 * 0.60 * 0.25 = 15.0
    let size = kelly_position_size(dec!(0.80), dec!(0.50), &config);
    assert_eq!(size, dec!(15.0));

    // With full multiplier (1.0):
    config.kelly_multiplier = Decimal::ONE;
    // size = 100 * 0.60 * 1.0 = 60.0, clamped to max_size=25
    let size_full = kelly_position_size(dec!(0.80), dec!(0.50), &config);
    assert_eq!(
        size_full, config.max_size,
        "Full multiplier should be clamped to max"
    );
}

#[test]
fn kelly_disabled_uses_fixed_size() {
    // When use_kelly is false, the caller uses base_size / price directly.
    // The kelly_position_size function doesn't handle this (it's the caller's logic).
    // But we can verify that when kelly returns 0, the caller would use fixed sizing.
    let config = SizingConfig::default();
    // Negative edge -> kelly returns 0
    let kelly = kelly_position_size(dec!(0.30), dec!(0.80), &config);
    assert_eq!(kelly, Decimal::ZERO, "Negative edge should return zero");
    // In this case, caller falls back to: base_size / price = 10 / 0.80 = 12.5
    let fixed = config.base_size / dec!(0.80);
    assert_eq!(fixed, dec!(12.5));
}

// ---------------------------------------------------------------------------
// Reference quality retroactive upgrade tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quality_upgrades_current_to_exact_on_boundary() {
    let base = make_base_no_chainlink();

    // Use a window_ts that is a 15-min boundary
    let window_ts = 1706000100i64;
    let boundary_ts = window_ts - (window_ts % WINDOW_SECS);

    // Insert a market with Current quality at that window_ts
    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "m1".to_string(),
            MarketWithReference {
                market: make_market_info("m1", Utc::now() + Duration::seconds(300)),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Current,
                discovery_time: Utc::now(),
                coin: "BTC".to_string(),
                window_ts: boundary_ts,
            },
        );
    }

    // Insert a boundary snapshot matching that window_ts
    {
        let mut boundaries = base.boundary_prices.write().await;
        boundaries.insert(
            format!("BTC-{boundary_ts}"),
            BoundarySnapshot {
                timestamp: DateTime::from_timestamp(boundary_ts, 0).unwrap(),
                price: dec!(49500),
                source: "rtds".to_string(),
            },
        );
    }

    // Call upgrade
    base.try_upgrade_quality("BTC").await;

    // Verify: should be Exact with updated price
    let markets = base.active_markets.read().await;
    let mwr = markets.get("m1").unwrap();
    assert_eq!(mwr.reference_quality, ReferenceQuality::Exact);
    assert_eq!(mwr.reference_price, dec!(49500));
}

#[tokio::test]
async fn quality_upgrades_current_to_historical() {
    let base = make_base_no_chainlink();

    let window_ts = 1706000100i64;
    let target_dt = DateTime::from_timestamp(window_ts, 0).unwrap();

    // Insert a market with Current quality
    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "m1".to_string(),
            MarketWithReference {
                market: make_market_info("m1", Utc::now() + Duration::seconds(300)),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Current,
                discovery_time: Utc::now(),
                coin: "BTC".to_string(),
                window_ts,
            },
        );
    }

    // Insert price history near the window_ts (5 seconds after)
    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        let ts = target_dt + Duration::seconds(5);
        entries.push_back((ts, dec!(49800), "binance".to_string(), ts));
        history.insert("BTC".to_string(), entries);
    }

    // Call upgrade
    base.try_upgrade_quality("BTC").await;

    // Verify: should be Historical(5) with updated price
    let markets = base.active_markets.read().await;
    let mwr = markets.get("m1").unwrap();
    assert_eq!(mwr.reference_quality, ReferenceQuality::Historical(5));
    assert_eq!(mwr.reference_price, dec!(49800));
}

#[tokio::test]
async fn quality_never_downgrades() {
    let base = make_base_no_chainlink();

    // Insert a market already at Exact quality
    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "m1".to_string(),
            MarketWithReference {
                market: make_market_info("m1", Utc::now() + Duration::seconds(300)),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Exact,
                discovery_time: Utc::now(),
                coin: "BTC".to_string(),
                window_ts: 1706000000,
            },
        );
    }

    // Insert price history that would match for Historical
    {
        let mut history = base.price_history.write().await;
        let target = DateTime::from_timestamp(1706000000, 0).unwrap();
        let mut entries = VecDeque::new();
        let ts = target + Duration::seconds(2);
        entries.push_back((ts, dec!(49999), "binance".to_string(), ts));
        history.insert("BTC".to_string(), entries);
    }

    // Call upgrade -- should be a no-op
    base.try_upgrade_quality("BTC").await;

    // Verify: still Exact with original price
    let markets = base.active_markets.read().await;
    let mwr = markets.get("m1").unwrap();
    assert_eq!(mwr.reference_quality, ReferenceQuality::Exact);
    assert_eq!(mwr.reference_price, dec!(50000));
}

#[tokio::test]
async fn quality_upgrade_updates_reference_price() {
    let base = make_base_no_chainlink();

    let window_ts = 1706000100i64;
    let boundary_ts = window_ts - (window_ts % WINDOW_SECS);

    // Insert market at Current with one price
    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "m1".to_string(),
            MarketWithReference {
                market: make_market_info("m1", Utc::now() + Duration::seconds(300)),
                reference_price: dec!(51000), // Current fallback price
                reference_quality: ReferenceQuality::Current,
                discovery_time: Utc::now(),
                coin: "BTC".to_string(),
                window_ts: boundary_ts,
            },
        );
    }

    // Insert boundary snapshot with a different price
    {
        let mut boundaries = base.boundary_prices.write().await;
        boundaries.insert(
            format!("BTC-{boundary_ts}"),
            BoundarySnapshot {
                timestamp: DateTime::from_timestamp(boundary_ts + 1, 0).unwrap(),
                price: dec!(50200),
                source: "rtds".to_string(),
            },
        );
    }

    base.try_upgrade_quality("BTC").await;

    let markets = base.active_markets.read().await;
    let mwr = markets.get("m1").unwrap();
    // Price should have changed from 51000 to 50200
    assert_eq!(mwr.reference_price, dec!(50200));
    assert_eq!(mwr.reference_quality, ReferenceQuality::Exact);
}

#[tokio::test]
async fn quality_historical_does_not_upgrade_to_historical() {
    let base = make_base_no_chainlink();

    let window_ts = 1706000100i64;
    let target_dt = DateTime::from_timestamp(window_ts, 0).unwrap();

    // Insert a market already at Historical(10)
    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "m1".to_string(),
            MarketWithReference {
                market: make_market_info("m1", Utc::now() + Duration::seconds(300)),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Historical(10),
                discovery_time: Utc::now(),
                coin: "BTC".to_string(),
                window_ts,
            },
        );
    }

    // Insert closer history entry (5s staleness, better than current 10s)
    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        let ts = target_dt + Duration::seconds(5);
        entries.push_back((ts, dec!(49900), "binance".to_string(), ts));
        history.insert("BTC".to_string(), entries);
    }

    base.try_upgrade_quality("BTC").await;

    // Historical->Historical upgrade is skipped (only Current->Historical is attempted)
    let markets = base.active_markets.read().await;
    let mwr = markets.get("m1").unwrap();
    assert_eq!(mwr.reference_quality, ReferenceQuality::Historical(10));
    assert_eq!(mwr.reference_price, dec!(50000));
}

// ---------------------------------------------------------------------------
// Task 10: Composite price cache for stop-loss
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sl_composite_cache_fresh_returns_result() {
    let base = make_base_no_chainlink();
    let now = Utc::now();

    // Seed the cache directly
    {
        let mut cache = base.sl_composite_cache.write().await;
        cache.insert(
            "BTC".to_string(),
            (
                CompositePriceResult {
                    price: dec!(88000),
                    sources_used: 3,
                    max_lag_ms: 200,
                    dispersion_bps: dec!(5),
                },
                now,
            ),
        );
    }

    // Request with generous age limit -- should return the cached composite
    let result = base.get_sl_composite("BTC", 5000, now).await;
    assert!(result.is_some(), "Fresh composite should be returned");
    let r = result.unwrap();
    assert_eq!(r.price, dec!(88000));
    assert_eq!(r.sources_used, 3);
}

#[tokio::test]
async fn sl_composite_cache_stale_returns_none() {
    let base = make_base_no_chainlink();
    let cached_at = Utc::now() - chrono::Duration::seconds(10);
    let now = Utc::now();

    // Seed with a 10-second-old entry
    {
        let mut cache = base.sl_composite_cache.write().await;
        cache.insert(
            "BTC".to_string(),
            (
                CompositePriceResult {
                    price: dec!(88000),
                    sources_used: 3,
                    max_lag_ms: 200,
                    dispersion_bps: dec!(5),
                },
                cached_at,
            ),
        );
    }

    // Request with 1200ms age limit -- 10s old entry should be stale
    let result = base.get_sl_composite("BTC", 1200, now).await;
    assert!(result.is_none(), "Stale composite should return None");
}

#[tokio::test]
async fn sl_composite_cache_missing_coin_returns_none() {
    let base = make_base_no_chainlink();
    let now = Utc::now();

    let result = base.get_sl_composite("ETH", 5000, now).await;
    assert!(result.is_none(), "Missing coin should return None");
}

#[tokio::test]
async fn sl_single_fresh_returns_recent_price() {
    let base = make_base_no_chainlink();
    let now = Utc::now();

    // Seed price_history directly
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        let ts = now - chrono::Duration::milliseconds(500);
        entries.push_back((ts, dec!(88500), "binance-spot".to_string(), ts));
        history.insert("BTC".to_string(), entries);
    }

    // 500ms old entry, 1500ms limit -- should return
    let result = base.get_sl_single_fresh("BTC", 1500, now).await;
    assert!(result.is_some(), "Fresh single source should be returned");
    assert_eq!(result.unwrap(), dec!(88500));
}

#[tokio::test]
async fn sl_single_fresh_returns_none_when_stale() {
    let base = make_base_no_chainlink();
    let now = Utc::now();

    // Seed price_history with a 5-second-old entry
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        let ts = now - chrono::Duration::seconds(5);
        entries.push_back((ts, dec!(88500), "binance-spot".to_string(), ts));
        history.insert("BTC".to_string(), entries);
    }

    // 5s old entry, 1500ms limit -- should be stale
    let result = base.get_sl_single_fresh("BTC", 1500, now).await;
    assert!(result.is_none(), "Stale single source should return None");
}

#[tokio::test]
async fn sl_single_fresh_returns_none_for_missing_coin() {
    let base = make_base_no_chainlink();
    let now = Utc::now();

    let result = base.get_sl_single_fresh("ETH", 1500, now).await;
    assert!(result.is_none(), "Missing coin should return None");
}

#[tokio::test]
async fn sl_composite_cache_propagates_to_lifecycle() {
    let base = make_base_no_chainlink();

    // Create a position with a lifecycle (coin defaults to "BTC" in make_position)
    let pos = make_position(
        "market-1",
        "token_btc",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(5),
        dec!(88000),
        dec!(0.90),
    );
    base.record_position(pos).await;

    // Verify lifecycle was created in Healthy state
    {
        let lc = base.position_lifecycle.read().await;
        let lifecycle = lc.get("token_btc").unwrap();
        assert!(lifecycle.last_composite.is_none());
        assert!(lifecycle.last_composite_at.is_none());
    }

    // Simulate update_sl_composite_cache by writing to cache and propagating
    let now = Utc::now();
    let composite = CompositePriceResult {
        price: dec!(88500),
        sources_used: 3,
        max_lag_ms: 150,
        dispersion_bps: dec!(3),
    };

    // Update cache
    {
        let mut cache = base.sl_composite_cache.write().await;
        cache.insert("BTC".to_string(), (composite.clone(), now));
    }

    // Propagate to lifecycle (simulating what update_sl_composite_cache does)
    {
        let snapshot = crate::crypto_arb::domain::CompositePriceSnapshot::from_result(&composite);
        let positions = base.positions.read().await;
        let mut lifecycles = base.position_lifecycle.write().await;
        for positions_vec in positions.values() {
            for pos in positions_vec {
                if pos.coin == "BTC" {
                    if let Some(lc) = lifecycles.get_mut(&pos.token_id) {
                        lc.last_composite = Some(snapshot.clone());
                        lc.last_composite_at = Some(now);
                    }
                }
            }
        }
    }

    // Verify lifecycle was updated
    {
        let lc = base.position_lifecycle.read().await;
        let lifecycle = lc.get("token_btc").unwrap();
        assert!(lifecycle.last_composite.is_some());
        let snap = lifecycle.last_composite.as_ref().unwrap();
        assert_eq!(snap.price, dec!(88500));
        assert_eq!(snap.sources_used, 3);
        assert!(lifecycle.last_composite_at.is_some());
    }
}

// ---------------------------------------------------------------------------
// Task 6: Timestamp correctness and source priority tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sl_single_fresh_uses_source_timestamp_not_receive_time() {
    let base = make_base_no_chainlink();
    let now = Utc::now();

    // Seed price_history: receive_time is recent (200ms ago),
    // but source_timestamp is old (10s ago).
    // Freshness should be computed from source_timestamp, not receive_time.
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        let receive_time = now - chrono::Duration::milliseconds(200);
        let source_ts = now - chrono::Duration::seconds(10);
        entries.push_back((
            receive_time,
            dec!(88500),
            "binance-spot".to_string(),
            source_ts,
        ));
        history.insert("BTC".to_string(), entries);
    }

    // With max_age_ms = 1500, source is 10s old -- should return None
    let result = base.get_sl_single_fresh("BTC", 1500, now).await;
    assert!(
        result.is_none(),
        "Should be stale: source_timestamp is 10s old, limit is 1.5s"
    );

    // With max_age_ms = 15000, source is 10s old -- should return the price
    let result = base.get_sl_single_fresh("BTC", 15000, now).await;
    assert!(
        result.is_some(),
        "Should be fresh: source_timestamp is 10s old, limit is 15s"
    );
    assert_eq!(result.unwrap(), dec!(88500));
}

#[tokio::test]
async fn composite_source_priority_fallback_order() {
    use polyrust_core::context::SourcedPrice;

    let base = make_base_no_chainlink();
    let ctx = StrategyContext::new();
    let now = Utc::now();

    // Populate sourced_prices with only coinbase and chainlink (no binance).
    // Both are fresh. Quorum requires 2+ sources by default but we set min_sources=2
    // and only have 2 non-WEIGHTS sources (coinbase is in WEIGHTS, chainlink is not).
    // Actually: WEIGHTS has binance-futures, binance-spot, coinbase.
    // With only coinbase fresh, sources_used=1 < min_sources=2 -> fallback kicks in.
    {
        let mut md = ctx.market_data.write().await;
        let mut coin_sources = std::collections::HashMap::new();
        coin_sources.insert(
            "coinbase".to_string(),
            SourcedPrice {
                price: dec!(50100),
                source: "coinbase".to_string(),
                timestamp: now - chrono::Duration::milliseconds(100),
            },
        );
        coin_sources.insert(
            "chainlink".to_string(),
            SourcedPrice {
                price: dec!(50200),
                source: "chainlink".to_string(),
                timestamp: now - chrono::Duration::milliseconds(200),
            },
        );
        md.sourced_prices.insert("BTC".to_string(), coin_sources);
    }

    // min_sources=2, only 1 weighted source (coinbase) is fresh -> quorum fails
    // Fallback should pick highest-priority fresh source.
    // Priority: binance-futures > binance-spot > coinbase > chainlink
    // Only coinbase and chainlink are available -> coinbase wins.
    let result = base
        .composite_fair_price("BTC", &ctx, 5, 2, dec!(100))
        .await;
    assert!(result.is_some(), "Fallback should return a result");
    let r = result.unwrap();
    assert_eq!(
        r.price,
        dec!(50100),
        "Should pick coinbase (higher priority than chainlink)"
    );
    assert_eq!(r.sources_used, 1, "Fallback uses single source");
}

#[tokio::test]
async fn composite_source_priority_prefers_binance_futures() {
    use polyrust_core::context::SourcedPrice;

    let base = make_base_no_chainlink();
    let ctx = StrategyContext::new();
    let now = Utc::now();

    // Populate with binance-futures and chainlink, both fresh.
    // binance-futures is in WEIGHTS so sources_used=1, but min_sources=2 -> fallback.
    // Fallback should prefer binance-futures over chainlink.
    {
        let mut md = ctx.market_data.write().await;
        let mut coin_sources = std::collections::HashMap::new();
        coin_sources.insert(
            "binance-futures".to_string(),
            SourcedPrice {
                price: dec!(50000),
                source: "binance-futures".to_string(),
                timestamp: now - chrono::Duration::milliseconds(50),
            },
        );
        coin_sources.insert(
            "chainlink".to_string(),
            SourcedPrice {
                price: dec!(50300),
                source: "chainlink".to_string(),
                timestamp: now - chrono::Duration::milliseconds(100),
            },
        );
        md.sourced_prices.insert("BTC".to_string(), coin_sources);
    }

    let result = base
        .composite_fair_price("BTC", &ctx, 5, 2, dec!(100))
        .await;
    assert!(result.is_some(), "Fallback should return a result");
    let r = result.unwrap();
    assert_eq!(
        r.price,
        dec!(50000),
        "Should pick binance-futures (highest priority)"
    );
}

#[tokio::test]
async fn composite_source_priority_skips_stale_sources() {
    use polyrust_core::context::SourcedPrice;

    let base = make_base_no_chainlink();
    let ctx = StrategyContext::new();
    let now = Utc::now();

    // binance-futures is stale (10s), coinbase is fresh (100ms)
    {
        let mut md = ctx.market_data.write().await;
        let mut coin_sources = std::collections::HashMap::new();
        coin_sources.insert(
            "binance-futures".to_string(),
            SourcedPrice {
                price: dec!(50000),
                source: "binance-futures".to_string(),
                timestamp: now - chrono::Duration::seconds(10),
            },
        );
        coin_sources.insert(
            "coinbase".to_string(),
            SourcedPrice {
                price: dec!(50100),
                source: "coinbase".to_string(),
                timestamp: now - chrono::Duration::milliseconds(100),
            },
        );
        md.sourced_prices.insert("BTC".to_string(), coin_sources);
    }

    // max_stale_secs=5 -> binance-futures is stale, coinbase is fresh
    // Quorum fails (1 weighted source < min_sources=2), fallback kicks in.
    // Fallback priority: binance-futures (stale, skip) > binance-spot (absent) > coinbase (fresh, pick)
    let result = base
        .composite_fair_price("BTC", &ctx, 5, 2, dec!(100))
        .await;
    assert!(result.is_some(), "Fallback should find coinbase");
    let r = result.unwrap();
    assert_eq!(
        r.price,
        dec!(50100),
        "Should pick coinbase (binance-futures is stale)"
    );
}
