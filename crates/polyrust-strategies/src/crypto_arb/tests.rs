//! Tests for the crypto arbitrage strategies.
//!
//! Tests are organized by module:
//! - types: ReferenceQuality, MarketWithReference, ModeStats
//! - config: Default values, deserialization
//! - base: Fee calculations, Kelly sizing, spike detection, reference lookup

use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use polyrust_core::prelude::*;

use super::base::{
    CryptoArbBase, kelly_position_size, net_profit_margin, parse_slug_timestamp, taker_fee,
};
use super::config::{ArbitrageConfig, SizingConfig};
use super::types::{
    compute_exit_clip, ArbitragePosition, BoundarySnapshot, CompositePriceSnapshot,
    ExitOrderMeta, MarketWithReference, ModeStats, OpenLimitOrder, PendingStopLoss,
    PositionLifecycle, PositionLifecycleState, ReferenceQuality, StopLossTriggerKind,
    TriggerEvalContext,
};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn make_market_info(id: &str, end_date: DateTime<Utc>) -> MarketInfo {
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

fn make_mwr(reference_price: Decimal, time_remaining_secs: i64) -> MarketWithReference {
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

fn make_base_no_chainlink() -> Arc<CryptoArbBase> {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    Arc::new(CryptoArbBase::new(config, vec![]))
}

// ---------------------------------------------------------------------------
// ReferenceQuality tests
// ---------------------------------------------------------------------------

#[test]
fn quality_factor_values() {
    assert_eq!(ReferenceQuality::Exact.quality_factor(), Decimal::ONE);
    assert_eq!(ReferenceQuality::OnChain(3).quality_factor(), Decimal::ONE);
    assert_eq!(ReferenceQuality::OnChain(12).quality_factor(), dec!(0.98));
    assert_eq!(ReferenceQuality::OnChain(20).quality_factor(), dec!(0.95));
    assert_eq!(ReferenceQuality::Historical(3).quality_factor(), dec!(0.95));
    assert_eq!(
        ReferenceQuality::Historical(10).quality_factor(),
        dec!(0.85)
    );
    assert_eq!(ReferenceQuality::Current.quality_factor(), dec!(0.70));
}

// ---------------------------------------------------------------------------
// MarketWithReference tests
// ---------------------------------------------------------------------------

#[test]
fn predict_winner_btc_up() {
    let mwr = make_mwr(dec!(50000), 600);
    assert_eq!(mwr.predict_winner(dec!(50100)), Some(OutcomeSide::Up));
}

#[test]
fn predict_winner_btc_down() {
    let mwr = make_mwr(dec!(50000), 600);
    assert_eq!(mwr.predict_winner(dec!(49900)), Some(OutcomeSide::Down));
}

#[test]
fn predict_winner_at_reference_returns_none() {
    let mwr = make_mwr(dec!(50000), 600);
    assert_eq!(mwr.predict_winner(dec!(50000)), None);
}

#[test]
fn confidence_tail_end() {
    // < 120s remaining, market >= 0.90 -> confidence 1.0
    let mwr = make_mwr(dec!(50000), 60);
    let confidence = mwr.get_confidence(dec!(51000), dec!(0.95), 60);
    assert_eq!(confidence, dec!(1.0));
}

#[test]
fn confidence_tail_end_low_market_price() {
    // < 120s but market < 0.90 -> NOT tail-end, falls to late window
    let mwr = make_mwr(dec!(50000), 60);
    let confidence = mwr.get_confidence(dec!(50050), dec!(0.55), 60);
    assert!(confidence < dec!(1.0));
    assert!(confidence > Decimal::ZERO);
}

#[test]
fn confidence_late_window() {
    // 120-300s remaining
    let mwr = make_mwr(dec!(50000), 200);
    let confidence = mwr.get_confidence(dec!(51000), dec!(0.70), 200);
    assert!(confidence > Decimal::ZERO);
    assert!(confidence <= dec!(1.0));
}

#[test]
fn confidence_early_window() {
    // > 300s remaining
    let mwr = make_mwr(dec!(50000), 600);
    // distance_pct = 500/50000 = 0.01, raw = 0.01 * 50 = 0.50
    let confidence = mwr.get_confidence(dec!(50500), dec!(0.50), 600);
    assert_eq!(confidence, dec!(0.50));
}

#[test]
fn confidence_early_window_small_move() {
    let mwr = make_mwr(dec!(50000), 600);
    // distance_pct = 100/50000 = 0.002, raw = 0.002 * 50 = 0.10
    let confidence = mwr.get_confidence(dec!(50100), dec!(0.50), 600);
    assert_eq!(confidence, dec!(0.10));
}

#[test]
fn confidence_discounted_by_quality() {
    // Exact quality: raw confidence unchanged
    let mwr_exact = make_mwr(dec!(50000), 600);
    let c_exact = mwr_exact.get_confidence(dec!(50500), dec!(0.50), 600);
    assert_eq!(c_exact, dec!(0.50));

    // Current quality: discounted by 0.70
    let mut mwr_current = make_mwr(dec!(50000), 600);
    mwr_current.reference_quality = ReferenceQuality::Current;
    let c_current = mwr_current.get_confidence(dec!(50500), dec!(0.50), 600);
    assert_eq!(c_current, dec!(0.350)); // 0.50 * 0.70 = 0.35
}

// ---------------------------------------------------------------------------
// ModeStats tests
// ---------------------------------------------------------------------------

#[test]
fn mode_stats_win_rate() {
    let mut stats = ModeStats::new(50);
    stats.record(dec!(1.0));
    stats.record(dec!(1.0));
    stats.record(dec!(-0.5));
    assert_eq!(stats.total_trades(), 3);
    // 2 wins / 3 total ≈ 0.666...
    let rate = stats.win_rate();
    assert!(rate > dec!(0.66) && rate < dec!(0.67));
}

#[test]
fn mode_stats_avg_pnl() {
    let mut stats = ModeStats::new(50);
    stats.record(dec!(2.0));
    stats.record(dec!(4.0));
    // avg = 6 / 2 = 3.0
    assert_eq!(stats.avg_pnl(), dec!(3.0));
}

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
// Config tests
// ---------------------------------------------------------------------------

#[test]
fn config_default_sub_configs() {
    let config = ArbitrageConfig::default();

    // Fee defaults
    assert_eq!(config.fee.taker_fee_rate, dec!(0.0315));

    // Spike defaults
    assert_eq!(config.spike.threshold_pct, dec!(0.005));
    assert_eq!(config.spike.window_secs, 10);
    assert_eq!(config.spike.history_size, 50);

    // Order defaults
    assert_eq!(config.order.max_age_secs, 30);

    // Sizing defaults
    assert_eq!(config.sizing.base_size, dec!(10));
    assert_eq!(config.sizing.kelly_multiplier, dec!(0.25));
    assert_eq!(config.sizing.min_size, dec!(2));
    assert_eq!(config.sizing.max_size, dec!(25));
    assert!(config.sizing.use_kelly);

    // StopLoss defaults
    assert_eq!(config.stop_loss.reversal_pct, dec!(0.003));
    assert_eq!(config.stop_loss.min_drop, dec!(0.05));
    assert!(config.stop_loss.trailing_enabled);
    assert_eq!(config.stop_loss.trailing_distance, dec!(0.05));
    assert!(config.stop_loss.time_decay);

    // Performance defaults
    assert_eq!(config.performance.min_trades, 20);
    assert_eq!(config.performance.min_win_rate, dec!(0.40));
    assert_eq!(config.performance.window_size, 50);
    assert!(!config.performance.auto_disable);
}

#[test]
fn config_deserialize_missing_sub_configs() {
    let toml_str = r#"
        coins = ["BTC"]
        max_positions = 3
        min_profit_margin = "0.04"
        late_window_margin = "0.03"
        scan_interval_secs = 60
        use_chainlink = false
    "#;
    let config: ArbitrageConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.coins, vec!["BTC"]);
    assert_eq!(config.max_positions, 3);
    assert!(!config.use_chainlink);
    // Sub-configs should have their defaults
    assert_eq!(config.fee.taker_fee_rate, dec!(0.0315));
}

// ---------------------------------------------------------------------------
// CryptoArbBase async tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn base_extract_coin_from_question() {
    let base = make_base_no_chainlink();
    assert_eq!(
        base.extract_coin("Will BTC go up in the next 15 minutes?"),
        Some("BTC".to_string())
    );
    assert_eq!(
        base.extract_coin("Will ETH be above $2000?"),
        Some("ETH".to_string())
    );
    assert_eq!(base.extract_coin("Random question about stocks"), None);
    // Full coin names
    assert_eq!(
        base.extract_coin("Bitcoin Up or Down - January 27, 4:45PM-5:00PM ET"),
        Some("BTC".to_string())
    );
    assert_eq!(
        base.extract_coin("Ethereum Up or Down - January 27, 4:45PM-5:00PM ET"),
        Some("ETH".to_string())
    );
    assert_eq!(
        base.extract_coin("Solana Up or Down - January 27, 4:45PM-5:00PM ET"),
        Some("SOL".to_string())
    );
    assert_eq!(
        base.extract_coin("XRP Up or Down - January 27, 4:45PM-5:00PM ET"),
        Some("XRP".to_string())
    );
}

#[tokio::test]
async fn base_record_price_and_detect_spike() {
    let base = make_base_no_chainlink();

    // Record initial price
    let _ = base
        .record_price("BTC", dec!(50000), "binance", Utc::now())
        .await;

    // Small move - no spike
    let (spike, _) = base
        .record_price("BTC", dec!(50100), "binance", Utc::now())
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
            VecDeque::from([(old_time, dec!(50000), "binance".to_string())]),
        );
    }

    let spike = base.detect_spike("TEST", dec!(50500), Utc::now()).await;
    // 500/50000 = 1% > 0.5% threshold
    assert!(spike.is_some());
    assert!(spike.unwrap().abs() >= dec!(0.005));
}

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
        entries.push_back((
            target_dt + Duration::seconds(5),
            dec!(42600),
            "binance".to_string(),
        ));
        // 20 seconds after window start
        entries.push_back((
            target_dt + Duration::seconds(20),
            dec!(42700),
            "binance".to_string(),
        ));
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

#[tokio::test]
async fn base_can_open_position() {
    let base = make_base_no_chainlink();

    // Should be able to open initially
    assert!(base.can_open_position().await);

    // Add max_positions (5 by default)
    {
        let mut positions = base.positions.write().await;
        for i in 0..5 {
            let pos = ArbitragePosition {
                market_id: format!("market{i}"),
                token_id: format!("token{i}"),
                side: OutcomeSide::Up,
                entry_price: dec!(0.60),
                size: dec!(10),
                reference_price: dec!(50000),
                coin: "BTC".to_string(),
                order_id: None,
                entry_time: Utc::now(),
                kelly_fraction: None,
                peak_bid: dec!(0.60),

                estimated_fee: Decimal::ZERO,
                entry_market_price: dec!(0.60),
                tick_size: dec!(0.01),
                fee_rate_bps: 0,
                entry_order_type: OrderType::Gtc,
                entry_fee_per_share: Decimal::ZERO,
                realized_pnl: Decimal::ZERO,
            };
            positions
                .entry(pos.market_id.clone())
                .or_default()
                .push(pos);
        }
    }

    // Now should be full
    assert!(!base.can_open_position().await);
}

// ---------------------------------------------------------------------------
// Market reservation tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reservation_blocks_concurrent_access() {
    let base = make_base_no_chainlink();

    // First reservation succeeds
    assert!(
        base.try_reserve_market(&"market1".to_string(), 1)
            .await
    );

    // Second reservation for same market fails
    assert!(
        !base
            .try_reserve_market(&"market1".to_string(), 2)
            .await
    );
}

#[tokio::test]
async fn reservation_counted_in_has_market_exposure() {
    let base = make_base_no_chainlink();

    // No exposure initially
    assert!(!base.has_market_exposure(&"market1".to_string()).await);

    // Reserve the market
    assert!(
        base.try_reserve_market(&"market1".to_string(), 1)
            .await
    );

    // Now has exposure
    assert!(base.has_market_exposure(&"market1".to_string()).await);
}

#[tokio::test]
async fn reservation_counted_in_can_open_position() {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.max_positions = 2;
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    assert!(base.can_open_position().await);

    // Reserve 2 slots
    assert!(
        base.try_reserve_market(&"market1".to_string(), 2)
            .await
    );

    // Now at capacity (1 reservation counts as 1 in the map, but total=1 + slot_count check)
    // Actually the reservation uses 1 map entry. Let's reserve another.
    assert!(
        !base
            .try_reserve_market(&"market2".to_string(), 1)
            .await
    );
}

#[tokio::test]
async fn release_reservation_makes_market_available() {
    let base = make_base_no_chainlink();

    // Reserve and then release
    assert!(
        base.try_reserve_market(&"market1".to_string(), 1)
            .await
    );
    assert!(base.has_market_exposure(&"market1".to_string()).await);

    base.release_reservation(&"market1".to_string()).await;

    // Market is now available again
    assert!(!base.has_market_exposure(&"market1".to_string()).await);
    assert!(
        base.try_reserve_market(&"market1".to_string(), 2)
            .await
    );
}

#[tokio::test]
async fn consume_reservation_then_pending_preserves_exposure() {
    let base = make_base_no_chainlink();

    // Reserve market
    assert!(
        base.try_reserve_market(&"market1".to_string(), 1)
            .await
    );

    // Consume reservation and insert pending order
    base.consume_reservation(&"market1".to_string()).await;
    {
        use super::types::PendingOrder;
        let mut pending = base.pending_orders.write().await;
        pending.insert(
            "token1".to_string(),
            PendingOrder {
                market_id: "market1".to_string(),
                token_id: "token1".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.95),
                size: dec!(10),
                reference_price: dec!(50000),
                coin: "BTC".to_string(),
                order_type: OrderType::Gtc,

                kelly_fraction: None,
                estimated_fee: Decimal::ZERO,
                tick_size: dec!(0.01),
                fee_rate_bps: 0,
            },
        );
    }

    // Exposure still exists via pending order
    assert!(base.has_market_exposure(&"market1".to_string()).await);
}

#[tokio::test]
async fn base_is_auto_disabled() {
    let mut config = ArbitrageConfig::default();
    config.performance.auto_disable = true;
    config.performance.min_trades = 3;
    config.performance.min_win_rate = dec!(0.50);
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    // Initially not disabled
    assert!(!base.is_auto_disabled().await);

    // Record losing trades
    base.record_trade_pnl(dec!(-1.0))
        .await;
    base.record_trade_pnl(dec!(-1.0))
        .await;
    base.record_trade_pnl(dec!(-1.0))
        .await;

    // Now should be disabled (0% win rate after 3 trades)
    assert!(base.is_auto_disabled().await);
}

// ---------------------------------------------------------------------------
// ReferenceQuality threshold tests
// ---------------------------------------------------------------------------

#[test]
fn reference_quality_meets_threshold() {
    use super::config::ReferenceQualityLevel;

    // Quality ordering: Current < Historical < OnChain < Exact (from lowest to highest)

    // Exact is highest quality, meets all thresholds
    assert!(ReferenceQuality::Exact.meets_threshold(ReferenceQualityLevel::Exact));
    assert!(ReferenceQuality::Exact.meets_threshold(ReferenceQualityLevel::OnChain));
    assert!(ReferenceQuality::Exact.meets_threshold(ReferenceQualityLevel::Historical));
    assert!(ReferenceQuality::Exact.meets_threshold(ReferenceQualityLevel::Current));

    // OnChain is higher quality than Historical and Current, but not Exact
    // OnChain(10).as_level() = OnChain
    // OnChain >= OnChain: true
    // OnChain >= Historical: true
    // OnChain >= Current: true
    // OnChain >= Exact: false (OnChain < Exact)
    assert!(!ReferenceQuality::OnChain(10).meets_threshold(ReferenceQualityLevel::Exact));
    assert!(ReferenceQuality::OnChain(10).meets_threshold(ReferenceQualityLevel::OnChain));
    assert!(ReferenceQuality::OnChain(10).meets_threshold(ReferenceQualityLevel::Historical));
    assert!(ReferenceQuality::OnChain(10).meets_threshold(ReferenceQualityLevel::Current));

    // Historical is higher than Current, but not OnChain or Exact
    assert!(!ReferenceQuality::Historical(10).meets_threshold(ReferenceQualityLevel::Exact));
    assert!(!ReferenceQuality::Historical(10).meets_threshold(ReferenceQualityLevel::OnChain));
    assert!(ReferenceQuality::Historical(10).meets_threshold(ReferenceQualityLevel::Historical));
    assert!(ReferenceQuality::Historical(10).meets_threshold(ReferenceQualityLevel::Current));

    // Current is lowest quality, only meets Current
    assert!(!ReferenceQuality::Current.meets_threshold(ReferenceQualityLevel::Exact));
    assert!(!ReferenceQuality::Current.meets_threshold(ReferenceQualityLevel::OnChain));
    assert!(!ReferenceQuality::Current.meets_threshold(ReferenceQualityLevel::Historical));
    assert!(ReferenceQuality::Current.meets_threshold(ReferenceQualityLevel::Current));
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
        entries.push_back((now - Duration::seconds(10), dec!(50100), "rtds".to_string()));
        entries.push_back((now - Duration::seconds(6), dec!(50200), "rtds".to_string()));
        entries.push_back((now - Duration::seconds(3), dec!(50300), "rtds".to_string()));
        entries.push_back((now - Duration::seconds(1), dec!(50400), "rtds".to_string()));
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
        entries.push_back((now - Duration::seconds(4), dec!(49900), "rtds".to_string())); // Below
        entries.push_back((now - Duration::seconds(2), dec!(50100), "rtds".to_string())); // Above
        entries.push_back((now - Duration::seconds(1), dec!(50200), "rtds".to_string())); // Above
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
        entries.push_back((now - Duration::seconds(2), dec!(50100), "rtds".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // min_ticks=2, but only 1 entry → should return false
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
        entries.push_back((now - Duration::seconds(4), dec!(50100), "rtds".to_string()));
        entries.push_back((now - Duration::seconds(2), dec!(50200), "rtds".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // min_ticks=2, 2 entries both favoring Up → should return true
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
        entries.push_back((now - Duration::seconds(4), dec!(49900), "rtds".to_string()));
        entries.push_back((now - Duration::seconds(2), dec!(50200), "rtds".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // min_ticks=2, 2 entries but first is against Up → should return false
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
        entries.push_back((now - Duration::seconds(8), dec!(50100), "rtds".to_string()));
        entries.push_back((now - Duration::seconds(5), dec!(50200), "rtds".to_string()));
        entries.push_back((now - Duration::seconds(2), dec!(50150), "rtds".to_string()));
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
        entries.push_back((now - Duration::seconds(8), dec!(50100), "rtds".to_string()));
        entries.push_back((now - Duration::seconds(5), dec!(51000), "rtds".to_string())); // 2% wick
        entries.push_back((now - Duration::seconds(2), dec!(50150), "rtds".to_string()));
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
// TailEndConfig tests
// ---------------------------------------------------------------------------

#[test]
fn tailend_config_defaults() {
    use super::config::{ReferenceQualityLevel, TailEndConfig};

    let config = TailEndConfig::default();
    assert_eq!(config.time_threshold_secs, 120);
    assert_eq!(config.ask_threshold, dec!(0.90));
    assert_eq!(
        config.min_reference_quality,
        ReferenceQualityLevel::Historical
    );
    assert_eq!(config.max_spread_bps, dec!(200));
    assert_eq!(config.min_sustained_secs, 5);
    assert_eq!(config.max_recent_volatility, dec!(0.02));
    assert_eq!(config.stale_ob_secs, 15);
    assert!(!config.dynamic_thresholds.is_empty());
}

// ---------------------------------------------------------------------------
// Dynamic threshold tests
// ---------------------------------------------------------------------------

#[test]
fn dynamic_ask_threshold_tightens_as_expiry_approaches() {
    use super::tailend::TailEndStrategy;
    use std::sync::Arc;

    let mut config = super::config::ArbitrageConfig::default();
    config.tailend.dynamic_thresholds = vec![
        (120, dec!(0.90)), // 0.90 at 120s
        (90, dec!(0.92)),  // 0.92 at 90s
        (60, dec!(0.93)),  // 0.93 at 60s
        (30, dec!(0.95)),  // 0.95 at 30s
    ];

    let base = Arc::new(super::base::CryptoArbBase::new(config, vec![]));
    let strategy = TailEndStrategy::new(base);

    // At 120s, should use 0.90 (120s bucket)
    assert_eq!(strategy.get_ask_threshold(120), dec!(0.90));
    // At 119s, should still use 0.90 (120s bucket is tightest that applies)
    assert_eq!(strategy.get_ask_threshold(119), dec!(0.90));

    // At 90s, should use 0.92 (90s bucket)
    assert_eq!(strategy.get_ask_threshold(90), dec!(0.92));
    // At 89s, should still use 0.92 (90s bucket is tightest that applies)
    assert_eq!(strategy.get_ask_threshold(89), dec!(0.92));

    // At 60s, should use 0.93 (60s bucket)
    assert_eq!(strategy.get_ask_threshold(60), dec!(0.93));
    // At 45s, should use 0.93 (60s bucket is tightest that applies)
    assert_eq!(strategy.get_ask_threshold(45), dec!(0.93));

    // At 30s, should use 0.95 (30s bucket - tightest)
    assert_eq!(strategy.get_ask_threshold(30), dec!(0.95));
    // At 15s, should use 0.95 (30s bucket is tightest that applies)
    assert_eq!(strategy.get_ask_threshold(15), dec!(0.95));

    // At 1s, should use 0.95 (30s bucket is tightest that applies)
    assert_eq!(strategy.get_ask_threshold(1), dec!(0.95));
}

#[test]
fn dynamic_ask_threshold_fallback_to_legacy() {
    use super::tailend::TailEndStrategy;
    use std::sync::Arc;

    let mut config = super::config::ArbitrageConfig::default();
    config.tailend.dynamic_thresholds = vec![]; // Empty - should fallback
    config.tailend.ask_threshold = dec!(0.88); // Legacy threshold

    let base = Arc::new(super::base::CryptoArbBase::new(config, vec![]));
    let strategy = TailEndStrategy::new(base);

    // Should fallback to legacy threshold when dynamic thresholds is empty
    assert_eq!(strategy.get_ask_threshold(60), dec!(0.88));
}

// ---------------------------------------------------------------------------
// Rejection cooldown tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rejection_cooldown_blocks_reevaluation() {
    use std::sync::Arc;

    let config = super::config::ArbitrageConfig::default();
    let base = Arc::new(super::base::CryptoArbBase::new(config, vec![]));

    let market_id = "market-123".to_string();

    // Initially not cooled down
    assert!(!base.is_rejection_cooled_down(&market_id).await);

    // Record a cooldown
    base.record_rejection_cooldown(&market_id, 15).await;

    // Should be cooled down now
    assert!(base.is_rejection_cooled_down(&market_id).await);

    // Different market should not be cooled down
    assert!(
        !base
            .is_rejection_cooled_down(&"other-market".to_string())
            .await
    );
}

#[tokio::test]
async fn rejection_cooldown_expires() {
    use std::sync::Arc;

    let config = super::config::ArbitrageConfig::default();
    let base = Arc::new(super::base::CryptoArbBase::new(config, vec![]));

    let market_id = "market-456".to_string();

    // Record a very short cooldown (1 second)
    base.record_rejection_cooldown(&market_id, 1).await;
    assert!(base.is_rejection_cooled_down(&market_id).await);

    // Advance simulated time by 2 seconds to expire the cooldown
    *base.last_event_time.write().await = Utc::now() + chrono::Duration::seconds(2);
    assert!(!base.is_rejection_cooled_down(&market_id).await);
}

// ---------------------------------------------------------------------------
// Stop-loss dual-trigger tests (check_stop_loss in base.rs)
// ---------------------------------------------------------------------------

/// Helper to create an ArbitragePosition with controlled parameters.
fn make_position(
    market_id: &str,
    token_id: &str,
    side: OutcomeSide,
    entry_price: Decimal,
    size: Decimal,
    reference_price: Decimal,
    peak_bid: Decimal,
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
        peak_bid,

        estimated_fee: Decimal::ZERO,
        entry_market_price: entry_price,
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
        entry_order_type: OrderType::Gtc,
        entry_fee_per_share: Decimal::ZERO,
        realized_pnl: Decimal::ZERO,
    }
}

/// Helper to create an OrderbookSnapshot with a single bid and ask.
fn make_snapshot(token_id: &str, bid: Decimal, ask: Decimal) -> OrderbookSnapshot {
    OrderbookSnapshot {
        token_id: token_id.to_string(),
        bids: vec![OrderbookLevel {
            price: bid,
            size: dec!(100),
        }],
        asks: vec![OrderbookLevel {
            price: ask,
            size: dec!(100),
        }],
        timestamp: Utc::now(),
    }
}

/// Helper to set up a base with an active market having a known end_date.
async fn make_base_with_market(market_id: &str, time_remaining_secs: i64) -> Arc<CryptoArbBase> {
    let mut config = super::config::ArbitrageConfig::default();
    config.use_chainlink = false;
    config.stop_loss.reversal_pct = dec!(0.005); // 0.5%
    config.stop_loss.min_drop = dec!(0.05); // 5¢
    config.stop_loss.trailing_enabled = true;
    config.stop_loss.trailing_distance = dec!(0.03);
    config.stop_loss.time_decay = true;
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

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

#[tokio::test]
async fn stop_loss_triggers_on_both_conditions() {
    let base = make_base_with_market("m1", 300).await;

    // Seed crypto price that has reversed (BTC went down from 50000 → 49700 = 0.6%)
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(49700), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // Position: bought Up at 0.90, now bid dropped to 0.84 (drop = 0.06 >= 0.05)
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_up", dec!(0.84), dec!(0.86));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_some(),
        "Stop-loss should trigger when both conditions met"
    );

    let (action, exit_price, _, trigger) = result.unwrap();
    assert_eq!(exit_price, dec!(0.84));
    assert_eq!(trigger.reason, "dual_trigger");
    // Verify it's a PlaceOrder (FOK sell)
    match action {
        Action::PlaceOrder(order) => {
            assert_eq!(order.side, OrderSide::Sell);
            assert_eq!(order.price, dec!(0.84));
        }
        _ => panic!("Expected PlaceOrder action"),
    }
}

#[tokio::test]
async fn stop_loss_no_trigger_reversal_only() {
    let base = make_base_with_market("m1", 300).await;

    // Crypto reversed (0.6% down) but market price didn't drop enough
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(49700), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // Entry at 0.90, bid at 0.88 — drop = 0.02 < 0.05 (min_drop)
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_up", dec!(0.88), dec!(0.92));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_none(),
        "Stop-loss should NOT trigger when only crypto reversed"
    );
}

#[tokio::test]
async fn stop_loss_no_trigger_drop_only() {
    let base = make_base_with_market("m1", 300).await;

    // Crypto stable (50000 → 50100 = UP, no reversal for Up position)
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // Market dropped from entry 0.90 → bid 0.80 (drop = 0.10 >= 0.05)
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_up", dec!(0.80), dec!(0.85));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_none(),
        "Stop-loss should NOT trigger when only market dropped"
    );
}

#[tokio::test]
async fn stop_loss_active_at_55s_with_default_config() {
    // Default min_remaining_secs=45 means stop-loss is active at 55s (55 > 45)
    let base = make_base_with_market("m1", 55).await;

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(49700), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_up", dec!(0.84), dec!(0.86));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_some(),
        "Stop-loss should trigger at 55s with default min_remaining_secs=45"
    );
}

#[tokio::test]
async fn stop_loss_suppressed_by_min_remaining_secs() {
    // Explicitly set min_remaining_secs=60 to suppress stop-loss in final minute
    let mut config = super::config::ArbitrageConfig::default();
    config.use_chainlink = false;
    config.stop_loss.reversal_pct = dec!(0.005);
    config.stop_loss.min_drop = dec!(0.05);
    config.stop_loss.trailing_enabled = true;
    config.stop_loss.trailing_distance = dec!(0.03);
    config.stop_loss.time_decay = true;
    config.stop_loss.min_remaining_secs = 60;
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "m1".to_string(),
            MarketWithReference {
                market: make_market_info("m1", Utc::now() + Duration::seconds(55)),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Exact,
                discovery_time: Utc::now(),
                coin: "BTC".to_string(),
                window_ts: 0,
            },
        );
    }
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(49700), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_up", dec!(0.84), dec!(0.86));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_none(),
        "Stop-loss should NOT trigger at 55s when min_remaining_secs=60"
    );
}

#[tokio::test]
async fn stop_loss_boundary_at_configured_threshold() {
    // min_remaining_secs=60: should trigger at 62s (above threshold)
    let mut config = super::config::ArbitrageConfig::default();
    config.use_chainlink = false;
    config.stop_loss.reversal_pct = dec!(0.005);
    config.stop_loss.min_drop = dec!(0.05);
    config.stop_loss.trailing_enabled = true;
    config.stop_loss.trailing_distance = dec!(0.03);
    config.stop_loss.time_decay = true;
    config.stop_loss.min_remaining_secs = 60;
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "m1".to_string(),
            MarketWithReference {
                market: make_market_info("m1", Utc::now() + Duration::seconds(62)),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Exact,
                discovery_time: Utc::now(),
                coin: "BTC".to_string(),
                window_ts: 0,
            },
        );
    }
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(49700), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_up", dec!(0.84), dec!(0.86));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_some(),
        "Stop-loss should trigger at 62s when min_remaining_secs=60"
    );
}

#[tokio::test]
async fn stop_loss_reversal_direction_up_position() {
    let base = make_base_with_market("m1", 300).await;

    // For Up position: reversal = (reference - current) / reference
    // 50000 → 49700: reversal = (50000 - 49700) / 50000 = 0.006 >= 0.005 ✓
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(49700), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_up", dec!(0.84), dec!(0.86));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_some(),
        "Up position: crypto reversing DOWN should trigger"
    );
}

#[tokio::test]
async fn stop_loss_reversal_direction_down_position() {
    let base = make_base_with_market("m1", 300).await;

    // For Down position: reversal = (current - reference) / reference
    // 50000 → 50300: reversal = (50300 - 50000) / 50000 = 0.006 >= 0.005 ✓
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50300), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    let pos = make_position(
        "m1",
        "token_down",
        OutcomeSide::Down,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_down", dec!(0.84), dec!(0.86));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_some(),
        "Down position: crypto reversing UP should trigger"
    );
}

#[tokio::test]
async fn stop_loss_uses_fok_order() {
    let base = make_base_with_market("m1", 300).await;

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(49700), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_up", dec!(0.84), dec!(0.86));

    let (action, _, _, _trigger) = base
        .check_stop_loss(&pos, &snapshot, Utc::now())
        .await
        .unwrap();
    match action {
        Action::PlaceOrder(order) => {
            assert_eq!(order.order_type, OrderType::Fok);
            assert_eq!(order.side, OrderSide::Sell);
            assert_eq!(order.size, dec!(10));
        }
        _ => panic!("Expected PlaceOrder action with FOK type"),
    }
}

#[tokio::test]
async fn stop_loss_order_uses_market_neg_risk_flag() {
    let base = make_base_no_chainlink();

    // Insert active market with neg_risk enabled
    {
        let mut market = make_market_info("m1", Utc::now() + Duration::seconds(300));
        market.neg_risk = true;
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "m1".to_string(),
            MarketWithReference {
                market,
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Exact,
                discovery_time: Utc::now(),
                coin: "BTC".to_string(),
                window_ts: 0,
            },
        );
    }

    // Seed crypto reversal so dual-trigger path is active
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(49700), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_up", dec!(0.84), dec!(0.86));

    let (action, _, _, _) = base
        .check_stop_loss(&pos, &snapshot, Utc::now())
        .await
        .unwrap();
    match action {
        Action::PlaceOrder(order) => assert!(order.neg_risk),
        _ => panic!("Expected PlaceOrder action"),
    }
}

// ---------------------------------------------------------------------------
// Trailing stop tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trailing_stop_triggers_on_drop_from_peak() {
    let base = make_base_with_market("m1", 450).await; // 450s = decay_factor 0.5

    // No crypto reversal needed for trailing stop
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // Position profitable: entry=0.90, peak=0.96
    // At 450s: decay_factor = 450/900 = 0.5, raw = 0.03 * 0.5 = 0.015
    // Floor = 0.015, so effective_distance = max(0.015, 0.015) = 0.015
    // Current bid = 0.90, drop_from_peak = 0.96 - 0.90 = 0.06 >= 0.015 ✓
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.96),
    );
    let snapshot = make_snapshot("token_up", dec!(0.90), dec!(0.96));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_some(),
        "Trailing stop should trigger when drop from peak >= effective distance"
    );
}

#[tokio::test]
async fn trailing_stop_requires_profitable_position() {
    let base = make_base_with_market("m1", 450).await;

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // Position NOT profitable: entry=0.90, peak=0.89 (below entry)
    // Trailing stop guard: peak_bid must > entry_price
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.89),
    );
    let snapshot = make_snapshot("token_up", dec!(0.85), dec!(0.87));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_none(),
        "Trailing stop should NOT trigger for unprofitable position"
    );
}

#[tokio::test]
async fn trailing_stop_time_decay_at_900s() {
    let base = make_base_with_market("m1", 900).await; // Full window

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // At 900s: decay_factor = 900/900 = 1.0, raw = 0.03 * 1.0 = 0.03
    // Floor = 0.015, so effective_distance = max(0.03, 0.015) = 0.03
    // peak=0.96, bid=0.91: drop = 0.05 >= 0.03 ✓
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.96),
    );
    let snapshot = make_snapshot("token_up", dec!(0.91), dec!(0.95));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_some(),
        "Trailing stop should trigger at 900s with full distance"
    );
}

#[tokio::test]
async fn trailing_stop_time_decay_at_450s() {
    let base = make_base_with_market("m1", 450).await;

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // At 450s: decay_factor = 450/900 = 0.5, raw = 0.03 * 0.5 = 0.015
    // Floor = 0.015, so effective_distance = max(0.015, 0.015) = 0.015
    // peak=0.96, bid=0.91: drop = 0.05 >= 0.015 ✓
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.96),
    );
    let snapshot = make_snapshot("token_up", dec!(0.91), dec!(0.96));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_some(),
        "Trailing stop should trigger at 450s with floor distance"
    );
}

#[tokio::test]
async fn trailing_stop_time_decay_at_90s_floored() {
    let base = make_base_with_market("m1", 90).await;

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // At 90s: decay_factor = 90/900 = 0.1, raw = 0.03 * 0.1 = 0.003
    // But trailing_min_distance floor = 0.015, so effective_distance = 0.015
    // peak=0.96, bid=0.95: drop = 0.01 < 0.015 → does NOT trigger (floor prevents noise)
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.96),
    );
    let snapshot = make_snapshot("token_up", dec!(0.95), dec!(0.96));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_none(),
        "Trailing stop should NOT trigger: drop (0.01) < floor (0.015)"
    );

    // With a bigger drop: peak=0.96, bid=0.944 → drop = 0.016 >= 0.015 ✓
    let snapshot_bigger = make_snapshot("token_up", dec!(0.944), dec!(0.96));
    let result2 = base
        .check_stop_loss(&pos, &snapshot_bigger, Utc::now())
        .await;
    assert!(
        result2.is_some(),
        "Trailing stop should trigger when drop exceeds floor"
    );
    let (_, _, _, trigger) = result2.unwrap();
    assert_eq!(trigger.reason, "trailing_stop");
    assert_eq!(trigger.effective_distance, dec!(0.015));
}

#[tokio::test]
async fn trailing_stop_disabled_when_config_false() {
    let mut config = super::config::ArbitrageConfig::default();
    config.use_chainlink = false;
    config.stop_loss.trailing_enabled = false; // Disabled
    config.stop_loss.reversal_pct = dec!(0.005);
    config.stop_loss.min_drop = dec!(0.05);
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    // Set up market
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
                window_ts: 0,
            },
        );
    }

    // No crypto reversal
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // Would-be trailing stop: profitable + peak drop
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.96),
    );
    let snapshot = make_snapshot("token_up", dec!(0.92), dec!(0.94));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_none(),
        "Trailing stop should NOT trigger when disabled in config"
    );
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
// Stop-loss rejection handling tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_loss_rejection_balance_allowance_keeps_position_and_cools_down() {
    let base = make_base_no_chainlink();

    // Add a position
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

    // Add to pending_stop_loss
    {
        let mut pending_sl = base.pending_stop_loss.write().await;
        pending_sl.insert(
            "token1".to_string(),
            PendingStopLoss {
                exit_price: dec!(0.85),
                order_type: OrderType::Fok,
            },
        );
    }

    // Handle rejection with balance error
    base.handle_stop_loss_rejection(&"token1".to_string(), "not enough balance", "TailEnd")
        .await;

    // Position should remain tracked for retry
    let positions = base.positions.read().await;
    assert!(
        !positions.is_empty(),
        "Balance/allowance rejection should keep position"
    );

    // Pending stop-loss should be cleared
    let pending_sl = base.pending_stop_loss.read().await;
    assert!(!pending_sl.contains_key("token1"));

    // Stop-loss retry cooldown should be applied
    assert!(base.is_stop_loss_cooled_down(&"token1".to_string()).await);
}

#[tokio::test]
async fn stop_loss_rejection_transient_applies_cooldown() {
    let base = make_base_no_chainlink();

    // Add a position
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

    // Add to pending_stop_loss
    {
        let mut pending_sl = base.pending_stop_loss.write().await;
        pending_sl.insert(
            "token1".to_string(),
            PendingStopLoss {
                exit_price: dec!(0.85),
                order_type: OrderType::Fok,
            },
        );
    }

    // Handle rejection with transient error
    base.handle_stop_loss_rejection(&"token1".to_string(), "rate limited", "TailEnd")
        .await;

    // Position should still be there
    let positions = base.positions.read().await;
    assert!(
        !positions.is_empty(),
        "Transient rejection should keep position"
    );

    // Cooldown should be applied
    assert!(base.is_stop_loss_cooled_down(&"token1".to_string()).await);
}

#[tokio::test]
async fn stop_loss_cooldown_prevents_retry() {
    let base = make_base_no_chainlink();

    // Record a cooldown
    base.record_stop_loss_cooldown(&"token1".to_string(), 30)
        .await;

    // Should be cooled down
    assert!(base.is_stop_loss_cooled_down(&"token1".to_string()).await);
}

// ---------------------------------------------------------------------------
// Kelly sizing edge case tests
// ---------------------------------------------------------------------------

#[test]
fn kelly_payout_below_minimum_returns_zero() {
    let config = SizingConfig::default();
    // price ~0.999 → payout = 1/0.999 - 1 ≈ 0.001, which is exactly at the 0.001 threshold
    // price at 0.9995 → payout = 1/0.9995 - 1 ≈ 0.0005 < 0.001 → returns 0
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
    // confidence=0.95, price=0.50 → payout=1.0
    // kelly = (0.95 * 1.0 - 0.05) / 1.0 = 0.90
    // size = 100 * 0.90 * 1.0 = 90, clamped to max_size=15
    let size = kelly_position_size(dec!(0.95), dec!(0.50), &config);
    assert_eq!(size, dec!(15), "Should be clamped to max_size");
}

#[test]
fn kelly_clamped_to_min_size() {
    let config = SizingConfig::default();
    // Already tested above (positive edge test), but explicitly verify the clamping
    // confidence=0.55, price=0.50 → payout=1.0
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

    // confidence=0.80, price=0.50 → payout=1.0
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
    // Negative edge → kelly returns 0
    let kelly = kelly_position_size(dec!(0.30), dec!(0.80), &config);
    assert_eq!(kelly, Decimal::ZERO, "Negative edge should return zero");
    // In this case, caller falls back to: base_size / price = 10 / 0.80 = 12.5
    let fixed = config.base_size / dec!(0.80);
    assert_eq!(fixed, dec!(12.5));
}

// ---------------------------------------------------------------------------
// Performance tracking tests
// ---------------------------------------------------------------------------

#[test]
fn mode_stats_rolling_window_eviction() {
    let mut stats = ModeStats::new(3); // Small window
    stats.record(dec!(1.0));
    stats.record(dec!(2.0));
    stats.record(dec!(3.0));
    assert_eq!(stats.recent_pnl.len(), 3);

    // Fourth entry should evict the oldest
    stats.record(dec!(4.0));
    assert_eq!(stats.recent_pnl.len(), 3);
    assert_eq!(
        *stats.recent_pnl.front().unwrap(),
        dec!(2.0),
        "Oldest (1.0) should be evicted"
    );
    assert_eq!(*stats.recent_pnl.back().unwrap(), dec!(4.0));
}

#[tokio::test]
async fn auto_disable_boundary_at_min_trades() {
    let mut config = super::config::ArbitrageConfig::default();
    config.performance.auto_disable = true;
    config.performance.min_trades = 20;
    config.performance.min_win_rate = dec!(0.40);
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    // Record exactly 20 trades: 8 wins (40%), 12 losses
    for _ in 0..8 {
        base.record_trade_pnl(dec!(1.0))
            .await;
    }
    for _ in 0..12 {
        base.record_trade_pnl(dec!(-1.0))
            .await;
    }

    // 40% win rate = exactly at threshold → NOT disabled (need to be strictly below)
    assert!(
        !base.is_auto_disabled().await,
        "At exactly min_win_rate should NOT be disabled"
    );
}

#[tokio::test]
async fn auto_disable_below_threshold() {
    let mut config = super::config::ArbitrageConfig::default();
    config.performance.auto_disable = true;
    config.performance.min_trades = 20;
    config.performance.min_win_rate = dec!(0.40);
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    // Record 20 trades: 7 wins (35%), 13 losses
    for _ in 0..7 {
        base.record_trade_pnl(dec!(1.0))
            .await;
    }
    for _ in 0..13 {
        base.record_trade_pnl(dec!(-1.0))
            .await;
    }

    assert!(
        base.is_auto_disabled().await,
        "35% win rate after 20 trades should trigger auto-disable"
    );
}

#[test]
fn pnl_zero_counts_as_win() {
    let mut stats = ModeStats::new(50);
    stats.record(Decimal::ZERO);
    assert_eq!(stats.won, 1, "P&L = 0 should count as a win");
    assert_eq!(stats.lost, 0);
}

// ---------------------------------------------------------------------------
// Position management tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn has_market_exposure_checks_all_types() {
    let base = make_base_no_chainlink();
    let market_id = "market-test".to_string();

    // No exposure initially
    assert!(!base.has_market_exposure(&market_id).await);

    // Add a position
    let pos = make_position(
        &market_id,
        "token1",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    base.record_position(pos).await;
    assert!(
        base.has_market_exposure(&market_id).await,
        "Should detect position exposure"
    );

    // Remove position, add pending order
    base.remove_position_by_token("token1").await;
    assert!(!base.has_market_exposure(&market_id).await);

    {
        let mut pending = base.pending_orders.write().await;
        pending.insert(
            "token2".to_string(),
            super::types::PendingOrder {
                market_id: market_id.clone(),
                token_id: "token2".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.90),
                size: dec!(10),
                reference_price: dec!(50000),
                coin: "BTC".to_string(),
                order_type: polyrust_core::types::OrderType::Gtc,

                kelly_fraction: None,
                estimated_fee: Decimal::ZERO,
                tick_size: dec!(0.01),
                fee_rate_bps: 0,
            },
        );
    }
    assert!(
        base.has_market_exposure(&market_id).await,
        "Should detect pending order exposure"
    );
}

#[tokio::test]
async fn remove_position_by_token_cleans_empty_market() {
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

    // Remove the only position
    let removed = base.remove_position_by_token("token1").await;
    assert!(removed.is_some());

    // Market entry should be cleaned up
    let positions = base.positions.read().await;
    assert!(
        !positions.contains_key("m1"),
        "Empty market entry should be removed"
    );
}

#[tokio::test]
async fn can_open_position_counts_all_order_types() {
    let mut config = super::config::ArbitrageConfig::default();
    config.max_positions = 3;
    config.use_chainlink = false;
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    assert!(base.can_open_position().await);

    // Add 1 position
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
    assert!(base.can_open_position().await, "1/3 should allow opening");

    // Add 1 pending order
    {
        let mut pending = base.pending_orders.write().await;
        pending.insert(
            "token2".to_string(),
            super::types::PendingOrder {
                market_id: "m2".to_string(),
                token_id: "token2".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.90),
                size: dec!(10),
                reference_price: dec!(50000),
                coin: "BTC".to_string(),
                order_type: polyrust_core::types::OrderType::Gtc,

                kelly_fraction: None,
                estimated_fee: Decimal::ZERO,
                tick_size: dec!(0.01),
                fee_rate_bps: 0,
            },
        );
    }
    assert!(base.can_open_position().await, "2/3 should allow opening");

    // Add 1 limit order → total = 3
    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert(
            "order3".to_string(),
            super::types::OpenLimitOrder {
                order_id: "order3".to_string(),
                market_id: "m3".to_string(),
                token_id: "token3".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.90),
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
            },
        );
    }
    assert!(
        !base.can_open_position().await,
        "3/3 should NOT allow opening"
    );
}

// ---------------------------------------------------------------------------
// Stale order management tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_limit_order_cancelled_after_max_age() {
    let mut config = super::config::ArbitrageConfig::default();
    config.order.max_age_secs = 1; // 1 second for quick test
    config.use_chainlink = false;
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    // Add a limit order with a past placed_at
    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert(
            "old-order".to_string(),
            super::types::OpenLimitOrder {
                order_id: "old-order".to_string(),
                market_id: "m1".to_string(),
                token_id: "token1".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.90),
                size: dec!(10),
                reference_price: dec!(50000),
                coin: "BTC".to_string(),
                placed_at: Utc::now() - chrono::Duration::seconds(5),

                kelly_fraction: None,
                estimated_fee: Decimal::ZERO,
                tick_size: dec!(0.01),
                fee_rate_bps: 0,
                cancel_pending: false,
                reconcile_miss_count: 0,
            },
        );
    }

    let actions = base.check_stale_limit_orders().await;
    assert_eq!(actions.len(), 1, "Should produce one cancel action");
    match &actions[0] {
        Action::CancelOrder(id) => assert_eq!(id, "old-order"),
        _ => panic!("Expected CancelOrder action"),
    }

    // Verify cancel_pending is set
    let limits = base.open_limit_orders.read().await;
    assert!(limits["old-order"].cancel_pending);
}

#[tokio::test]
async fn stale_order_cancel_pending_prevents_double() {
    let mut config = super::config::ArbitrageConfig::default();
    config.order.max_age_secs = 1;
    config.use_chainlink = false;
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert(
            "old-order".to_string(),
            super::types::OpenLimitOrder {
                order_id: "old-order".to_string(),
                market_id: "m1".to_string(),
                token_id: "token1".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.90),
                size: dec!(10),
                reference_price: dec!(50000),
                coin: "BTC".to_string(),
                placed_at: Utc::now() - chrono::Duration::seconds(5),

                kelly_fraction: None,
                estimated_fee: Decimal::ZERO,
                tick_size: dec!(0.01),
                fee_rate_bps: 0,
                cancel_pending: true, // Already has cancel in flight
                reconcile_miss_count: 0,
            },
        );
    }

    let actions = base.check_stale_limit_orders().await;
    assert!(
        actions.is_empty(),
        "Should not produce cancel when cancel_pending is true"
    );
}

// ---------------------------------------------------------------------------
// Trailing stop floor tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trailing_stop_floor_prevents_noise_trigger() {
    // With floor=0.015, trailing stop requires peak >= entry + 0.015
    // and a drop of at least floor from peak. This prevents noise triggers.
    let mut config = super::config::ArbitrageConfig::default();
    config.use_chainlink = false;
    config.stop_loss.trailing_enabled = true;
    config.stop_loss.trailing_distance = dec!(0.03);
    config.stop_loss.time_decay = true;
    // Uses default trailing_min_distance = 0.015
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "m1".to_string(),
            MarketWithReference {
                market: make_market_info("m1", Utc::now() + Duration::seconds(90)),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Exact,
                discovery_time: Utc::now(),
                coin: "BTC".to_string(),
                window_ts: 0,
            },
        );
    }
    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // entry=0.90, peak=0.91 → peak < entry + 0.015 (0.91 < 0.915)
    // So the trailing stop should NOT arm at all
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.91),
    );
    let snapshot = make_snapshot("token_up", dec!(0.88), dec!(0.99));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_none(),
        "Trailing stop should NOT arm when peak_bid < entry + min_distance (0.015)"
    );
}

#[tokio::test]
async fn trailing_stop_arms_at_min_distance_above_entry() {
    let base = make_base_with_market("m1", 300).await;

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // entry=0.90, peak=0.96 → peak >= entry + 0.015 ✓ (0.96 >= 0.915)
    // At 300s: decay_factor = 300/900 = 0.333, raw = 0.03 * 0.333 = 0.01
    // Floored to max(0.01, 0.015) = 0.015
    // bid=0.909, drop = 0.96 - 0.909 = 0.051 >= 0.015 ✓
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.96),
    );
    let snapshot = make_snapshot("token_up", dec!(0.909), dec!(0.92));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(
        result.is_some(),
        "Trailing stop should arm when peak >= entry + min_distance"
    );
    let (_, _, _, trigger) = result.unwrap();
    assert_eq!(trigger.reason, "trailing_stop");
}

#[tokio::test]
async fn trailing_stop_does_not_arm_below_min_distance() {
    let base = make_base_with_market("m1", 300).await;

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // entry=0.90, peak=0.91 → peak < entry + 0.015 (0.91 < 0.915)
    // Trailing stop should NOT arm
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.91),
    );
    let snapshot = make_snapshot("token_up", dec!(0.85), dec!(0.87));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    // Even though bid dropped significantly from peak, trailing should not arm
    // However, dual trigger could fire if crypto reversed + market dropped
    // Here: crypto is at 50100 > 50000, so no reversal for Up. No dual trigger.
    assert!(
        result.is_none(),
        "Trailing stop should NOT arm when peak < entry + min_distance"
    );
}

// ---------------------------------------------------------------------------
// Stale market cooldown tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_market_cooldown_blocks_reentry() {
    let base = make_base_no_chainlink();
    let market_id = "market-stale".to_string();

    // Initially not cooled down
    assert!(!base.is_stale_market_cooled_down(&market_id).await);

    // Record a cooldown
    base.record_stale_market_cooldown(&market_id, 120).await;

    // Should be cooled down now
    assert!(base.is_stale_market_cooled_down(&market_id).await);

    // Different market should not be cooled down
    assert!(
        !base
            .is_stale_market_cooled_down(&"other-market".to_string())
            .await
    );
}

#[tokio::test]
async fn stale_market_cooldown_expires() {
    let base = make_base_no_chainlink();
    let market_id = "market-expire".to_string();

    // Record a very short cooldown (1 second)
    base.record_stale_market_cooldown(&market_id, 1).await;
    assert!(base.is_stale_market_cooled_down(&market_id).await);

    // Advance simulated time by 2 seconds to expire the cooldown
    *base.last_event_time.write().await = Utc::now() + chrono::Duration::seconds(2);
    assert!(!base.is_stale_market_cooled_down(&market_id).await);
}

#[tokio::test]
async fn handle_stop_loss_rejection_balance_allowance_does_not_mark_stale() {
    let base = make_base_no_chainlink();

    // Add a position
    let pos = make_position(
        "m1",
        "token_stale",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    base.record_position(pos).await;

    // Add to pending_stop_loss
    {
        let mut pending_sl = base.pending_stop_loss.write().await;
        pending_sl.insert(
            "token_stale".to_string(),
            PendingStopLoss {
                exit_price: dec!(0.85),
                order_type: OrderType::Fok,
            },
        );
    }

    // Handle rejection with balance error
    base.handle_stop_loss_rejection(&"token_stale".to_string(), "not enough balance", "TailEnd")
        .await;

    // Position should remain tracked
    let positions = base.positions.read().await;
    assert!(
        !positions.is_empty(),
        "Balance/allowance rejection should keep position"
    );

    // Market should not be marked stale
    assert!(
        !base.is_stale_market_cooled_down(&"m1".to_string()).await,
        "Balance/allowance rejection should not mark market stale"
    );
}

// ---------------------------------------------------------------------------
// Config default tests for new fields
// ---------------------------------------------------------------------------

#[test]
fn stop_loss_config_new_field_defaults() {
    let config = super::config::StopLossConfig::default();
    assert_eq!(config.trailing_min_distance, dec!(0.015));
    assert_eq!(config.stale_market_cooldown_secs, 120);
    assert_eq!(config.min_remaining_secs, 45);
    assert_eq!(config.liquidity_cooldowns, vec![1, 5, 15, 30]);
    assert_eq!(config.balance_cooldowns, vec![5, 15, 30, 60]);
    assert!(config.gtc_fallback);
    assert_eq!(config.gtc_fallback_tick_offset, 1);
    assert_eq!(config.gtc_stop_loss_max_age_secs, 2);
}

#[test]
fn stop_loss_config_deserialize_missing_new_fields() {
    // Ensure backwards compatibility: old configs without new fields should use defaults
    let toml_str = r#"
        reversal_pct = "0.005"
        min_drop = "0.05"
        trailing_enabled = true
        trailing_distance = "0.03"
        time_decay = true
    "#;
    let config: super::config::StopLossConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.trailing_min_distance, dec!(0.015));
    assert_eq!(config.stale_market_cooldown_secs, 120);
    assert_eq!(config.min_remaining_secs, 45);
    assert_eq!(config.liquidity_cooldowns, vec![1, 5, 15, 30]);
    assert_eq!(config.balance_cooldowns, vec![5, 15, 30, 60]);
    assert!(config.gtc_fallback);
}

// ---------------------------------------------------------------------------
// Default consistency invariant tests
// ---------------------------------------------------------------------------

#[test]
fn trailing_floor_less_than_base_distance() {
    let config = super::config::StopLossConfig::default();
    assert!(
        config.trailing_min_distance < config.trailing_distance,
        "trailing_min_distance ({}) must be less than trailing_distance ({}) \
         so time decay has room to operate",
        config.trailing_min_distance,
        config.trailing_distance,
    );
}

#[test]
fn post_entry_window_greater_than_sell_delay() {
    let config = super::config::ArbitrageConfig::default();
    assert!(
        config.tailend.post_entry_window_secs > config.tailend.min_sell_delay_secs,
        "post_entry_window_secs ({}) must be greater than min_sell_delay_secs ({}) \
         so the post-entry exit window is reachable",
        config.tailend.post_entry_window_secs,
        config.tailend.min_sell_delay_secs,
    );
}

// ---------------------------------------------------------------------------
// Enhanced stop-loss trigger metadata tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stop_loss_trigger_returns_trailing_metadata() {
    let base = make_base_with_market("m1", 450).await;

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(50100), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // Trailing stop: entry=0.90, peak=0.96
    // At 450s: decay_factor=0.5, raw=0.015, floored to max(0.015, 0.015)=0.015
    // bid=0.909, drop=0.051 >= 0.015 ✓
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.96),
    );
    let snapshot = make_snapshot("token_up", dec!(0.909), dec!(0.96));

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(result.is_some());
    let (_, _, _, trigger) = result.unwrap();
    assert_eq!(trigger.reason, "trailing_stop");
    assert_eq!(trigger.peak_bid, dec!(0.96));
    // effective_distance is base * (time/900), floored to min_distance=0.015
    assert_eq!(
        trigger.effective_distance,
        dec!(0.015),
        "effective_distance should be 0.015 (floor)"
    );
    // time_remaining is approximate (±1s) so just check it's reasonable
    assert!(trigger.time_remaining >= 448 && trigger.time_remaining <= 451);
}

// ---------------------------------------------------------------------------
// Reference quality retroactive upgrade tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quality_upgrades_current_to_exact_on_boundary() {
    let base = make_base_no_chainlink();

    // Use a window_ts that is a 15-min boundary
    let window_ts = 1706000100i64;
    let boundary_ts = window_ts - (window_ts % super::base::WINDOW_SECS);

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
        entries.push_back((
            target_dt + Duration::seconds(5),
            dec!(49800),
            "binance".to_string(),
        ));
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
        entries.push_back((
            target + Duration::seconds(2),
            dec!(49999),
            "binance".to_string(),
        ));
        history.insert("BTC".to_string(), entries);
    }

    // Call upgrade — should be a no-op
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
    let boundary_ts = window_ts - (window_ts % super::base::WINDOW_SECS);

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
        entries.push_back((
            target_dt + Duration::seconds(5),
            dec!(49900),
            "binance".to_string(),
        ));
        history.insert("BTC".to_string(), entries);
    }

    base.try_upgrade_quality("BTC").await;

    // Historical→Historical upgrade is skipped (only Current→Historical is attempted)
    let markets = base.active_markets.read().await;
    let mwr = markets.get("m1").unwrap();
    assert_eq!(mwr.reference_quality, ReferenceQuality::Historical(10));
    assert_eq!(mwr.reference_price, dec!(50000));
}

// ---------------------------------------------------------------------------
// Reconciliation tests
// ---------------------------------------------------------------------------

fn make_open_limit_order(order_id: &str, market_id: &str, token_id: &str) -> OpenLimitOrder {
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

#[tokio::test]
async fn reconcile_detects_filled_order_after_two_misses() {
    let base = make_base_no_chainlink();

    // Pre-populate with 2 open limit orders
    let lo1 = make_open_limit_order("order-1", "market-A", "token-A-up");
    let lo2 = make_open_limit_order("order-2", "market-B", "token-B-up");

    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert("order-1".to_string(), lo1);
        limits.insert("order-2".to_string(), lo2);
    }

    // CLOB reports only order-1 as open — order-2 is missing
    let mut clob_open = std::collections::HashSet::new();
    clob_open.insert("order-1".to_string());

    // First reconciliation: order-2 gets miss_count=1, no position created yet
    let actions = base.reconcile_limit_orders(&clob_open).await;
    assert!(actions.is_empty(), "First miss should not create position");
    {
        let limits = base.open_limit_orders.read().await;
        assert!(
            limits.contains_key("order-2"),
            "order-2 should still be tracked after first miss"
        );
        assert_eq!(limits["order-2"].reconcile_miss_count, 1);
    }
    let positions = base.positions.read().await;
    assert!(
        !positions.contains_key("market-B"),
        "no position after first miss"
    );
    drop(positions);

    // Second reconciliation: order-2 gets miss_count=2, now confirmed fill
    let actions = base.reconcile_limit_orders(&clob_open).await;

    // Verify order-2 was removed from tracking
    let limits = base.open_limit_orders.read().await;
    assert!(
        limits.contains_key("order-1"),
        "order-1 should still be tracked"
    );
    assert!(
        !limits.contains_key("order-2"),
        "order-2 should be removed (reconciled fill)"
    );
    drop(limits);

    // Verify position was created for the filled order
    let positions = base.positions.read().await;
    assert!(
        positions.contains_key("market-B"),
        "position should exist for market-B"
    );
    let market_positions = positions.get("market-B").unwrap();
    assert_eq!(market_positions.len(), 1);
    assert_eq!(market_positions[0].entry_price, dec!(0.92));
    assert_eq!(market_positions[0].size, dec!(10));
    drop(positions);

    // Verify RecordFill + "reconciled-fill" signal were emitted
    assert_eq!(actions.len(), 2);
    match &actions[0] {
        Action::RecordFill {
            order_id,
            market_id,
            side,
            ..
        } => {
            assert_eq!(order_id, "order-2");
            assert_eq!(market_id, "market-B");
            assert_eq!(*side, OrderSide::Buy);
        }
        other => panic!("expected RecordFill, got {:?}", other),
    }
    match &actions[1] {
        Action::EmitSignal {
            signal_type,
            payload,
        } => {
            assert_eq!(signal_type, "reconciled-fill");
            assert_eq!(payload["order_id"], "order-2");
            assert_eq!(payload["market_id"], "market-B");
        }
        other => panic!("expected EmitSignal, got {:?}", other),
    }
}

#[tokio::test]
async fn reconcile_skips_cancel_pending_orders() {
    let base = make_base_no_chainlink();

    let mut lo = make_open_limit_order("order-1", "market-A", "token-A-up");
    lo.cancel_pending = true; // Cancel already in flight

    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert("order-1".to_string(), lo);
    }

    // CLOB has no open orders — but order-1 has cancel_pending, so skip it
    let clob_open = std::collections::HashSet::new();
    let actions = base.reconcile_limit_orders(&clob_open).await;

    // Order should still be tracked (cancel_pending orders are skipped)
    let limits = base.open_limit_orders.read().await;
    assert!(
        limits.contains_key("order-1"),
        "cancel_pending order should not be reconciled"
    );
    assert!(actions.is_empty(), "no actions for cancel_pending orders");
}

#[tokio::test]
async fn handle_cancel_failed_matched_creates_position() {
    let base = make_base_no_chainlink();

    let lo = make_open_limit_order("order-1", "market-A", "token-A-up");

    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert("order-1".to_string(), lo);
    }

    // Simulate cancel failure because order was matched by counterparty
    let (found, actions) = base
        .handle_cancel_failed("order-1", "order was matched")
        .await;

    assert!(found, "order should have been found in tracking");

    // Verify order removed from tracking
    let limits = base.open_limit_orders.read().await;
    assert!(
        !limits.contains_key("order-1"),
        "matched order should be removed"
    );
    drop(limits);

    // Verify position was created (this was the bug — previously only emitted signal)
    let positions = base.positions.read().await;
    assert!(
        positions.contains_key("market-A"),
        "position should exist for market-A"
    );
    let market_positions = positions.get("market-A").unwrap();
    assert_eq!(market_positions.len(), 1);
    assert_eq!(market_positions[0].entry_price, dec!(0.92));
    assert_eq!(market_positions[0].size, dec!(10));
    assert_eq!(market_positions[0].token_id, "token-A-up");
    drop(positions);

    // Verify RecordFill + "matched-fill" signal emitted
    assert_eq!(actions.len(), 2);
    match &actions[0] {
        Action::RecordFill {
            order_id,
            market_id,
            side,
            ..
        } => {
            assert_eq!(order_id, "order-1");
            assert_eq!(market_id, "market-A");
            assert_eq!(*side, OrderSide::Buy);
        }
        other => panic!("expected RecordFill, got {:?}", other),
    }
    match &actions[1] {
        Action::EmitSignal {
            signal_type,
            payload,
        } => {
            assert_eq!(signal_type, "matched-fill");
            assert_eq!(payload["order_id"], "order-1");
            assert_eq!(payload["market_id"], "market-A");
        }
        other => panic!("expected EmitSignal, got {:?}", other),
    }
}

#[tokio::test]
async fn handle_cancel_failed_not_matched_does_not_create_position() {
    let base = make_base_no_chainlink();

    let lo = make_open_limit_order("order-1", "market-A", "token-A-up");

    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert("order-1".to_string(), lo);
    }

    // Cancel failed for a transient reason (not matched/canceled/not found)
    let (found, actions) = base
        .handle_cancel_failed("order-1", "timeout connecting to CLOB")
        .await;

    assert!(found);
    assert!(actions.is_empty(), "no actions for transient failure");

    // Order should still be tracked with cancel_pending reset
    let limits = base.open_limit_orders.read().await;
    assert!(
        limits.contains_key("order-1"),
        "order should still be tracked"
    );
    assert!(
        !limits["order-1"].cancel_pending,
        "cancel_pending should be reset"
    );

    // No position created
    let positions = base.positions.read().await;
    assert!(
        !positions.contains_key("market-A"),
        "no position for transient failure"
    );
}

// ---------------------------------------------------------------------------
// Strike proximity filter tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strike_proximity_rejects_within_threshold() {
    use super::tailend::TailEndStrategy;

    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.min_sustained_secs = 0;
    config.tailend.min_sustained_ticks = 0;
    config.tailend.max_recent_volatility = dec!(1.0);
    config.tailend.min_strike_distance_pct = dec!(0.0008); // 0.08%
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    let market = MarketWithReference {
        market: make_market_info("m1", Utc::now() + Duration::seconds(60)),
        reference_price: dec!(2000), // Strike at $2000
        reference_quality: ReferenceQuality::Exact,
        discovery_time: Utc::now(),
        coin: "ETH".to_string(),
        window_ts: 0,
    };
    base.active_markets
        .write()
        .await
        .insert("m1".to_string(), market);

    let ctx = StrategyContext::new();
    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks.insert(
            "token_down".to_string(),
            OrderbookSnapshot {
                token_id: "token_down".to_string(),
                bids: vec![OrderbookLevel {
                    price: dec!(0.92),
                    size: dec!(100),
                }],
                asks: vec![OrderbookLevel {
                    price: dec!(0.94),
                    size: dec!(100),
                }],
                timestamp: Utc::now(),
            },
        );
    }

    let strategy = TailEndStrategy::new(base);

    // ETH at $1998.78 → distance from $2000 = $1.22 = 0.061% < 0.08% → reject
    let opp = strategy
        .evaluate_opportunity(&"m1".to_string(), dec!(1998.78), &ctx)
        .await;
    assert!(opp.is_none(), "Should reject: 0.061% < 0.08% threshold");
}

#[tokio::test]
async fn strike_proximity_allows_beyond_threshold() {
    use super::tailend::TailEndStrategy;

    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.min_sustained_secs = 0;
    config.tailend.min_sustained_ticks = 0;
    config.tailend.max_recent_volatility = dec!(1.0);
    config.tailend.min_strike_distance_pct = dec!(0.0008); // 0.08%
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    let market = MarketWithReference {
        market: make_market_info("m1", Utc::now() + Duration::seconds(60)),
        reference_price: dec!(2000), // Strike at $2000
        reference_quality: ReferenceQuality::Exact,
        discovery_time: Utc::now(),
        coin: "ETH".to_string(),
        window_ts: 0,
    };
    base.active_markets
        .write()
        .await
        .insert("m1".to_string(), market);

    let ctx = StrategyContext::new();
    // Tight spread: bid=0.935, ask=0.94 → spread=0.53% < 2% max
    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks.insert(
            "token_down".to_string(),
            OrderbookSnapshot {
                token_id: "token_down".to_string(),
                bids: vec![OrderbookLevel {
                    price: dec!(0.935),
                    size: dec!(100),
                }],
                asks: vec![OrderbookLevel {
                    price: dec!(0.94),
                    size: dec!(100),
                }],
                timestamp: Utc::now(),
            },
        );
    }

    // Populate price history for ETH so sustained direction check passes
    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        let now = Utc::now();
        entries.push_back((now - Duration::seconds(3), dec!(1996), "test".to_string()));
        entries.push_back((now - Duration::seconds(1), dec!(1996), "test".to_string()));
        history.insert("ETH".to_string(), entries);
    }

    let strategy = TailEndStrategy::new(base);

    // ETH at $1996 → distance from $2000 = $4 = 0.2% > 0.08% → allow
    let opp = strategy
        .evaluate_opportunity(&"m1".to_string(), dec!(1996), &ctx)
        .await;
    assert!(opp.is_some(), "Should allow: 0.2% > 0.08% threshold");
}

// ---------------------------------------------------------------------------
// StopLossRejectionKind classification tests
// ---------------------------------------------------------------------------

#[test]
fn rejection_kind_classifies_liquidity() {
    use super::base::StopLossRejectionKind;

    assert_eq!(
        StopLossRejectionKind::classify("couldn't be fully filled"),
        StopLossRejectionKind::Liquidity
    );
    assert_eq!(
        StopLossRejectionKind::classify("no match found for order"),
        StopLossRejectionKind::Liquidity
    );
}

#[test]
fn rejection_kind_classifies_balance() {
    use super::base::StopLossRejectionKind;

    assert_eq!(
        StopLossRejectionKind::classify("not enough balance"),
        StopLossRejectionKind::BalanceAllowance
    );
    assert_eq!(
        StopLossRejectionKind::classify("insufficient allowance for token"),
        StopLossRejectionKind::BalanceAllowance
    );
}

#[test]
fn rejection_kind_classifies_transient() {
    use super::base::StopLossRejectionKind;

    assert_eq!(
        StopLossRejectionKind::classify("rate limited"),
        StopLossRejectionKind::Transient
    );
    assert_eq!(
        StopLossRejectionKind::classify("unknown error"),
        StopLossRejectionKind::Transient
    );
}

// ---------------------------------------------------------------------------
// GTC fallback flag tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn liquidity_rejection_marks_gtc_fallback() {
    let base = make_base_no_chainlink();

    // Add position
    let pos = make_position(
        "m1",
        "token_gtc",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    base.record_position(pos).await;

    // Add to pending_stop_loss
    {
        let mut psl = base.pending_stop_loss.write().await;
        psl.insert(
            "token_gtc".to_string(),
            PendingStopLoss {
                exit_price: dec!(0.85),
                order_type: OrderType::Fok,
            },
        );
    }

    // Handle liquidity rejection
    base.handle_stop_loss_rejection(
        &"token_gtc".to_string(),
        "couldn't be fully filled",
        "TailEnd",
    )
    .await;

    // Should be marked for GTC fallback
    let gtc_set = base.stop_loss_use_gtc.read().await;
    assert!(
        gtc_set.contains("token_gtc"),
        "Liquidity rejection should mark for GTC"
    );

    // Should use fast cooldown (1s for first rejection)
    assert!(
        base.is_stop_loss_cooled_down(&"token_gtc".to_string())
            .await
    );
}

#[tokio::test]
async fn balance_rejection_does_not_mark_gtc_fallback() {
    let base = make_base_no_chainlink();

    let pos = make_position(
        "m1",
        "token_bal",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    base.record_position(pos).await;

    {
        let mut psl = base.pending_stop_loss.write().await;
        psl.insert(
            "token_bal".to_string(),
            PendingStopLoss {
                exit_price: dec!(0.85),
                order_type: OrderType::Fok,
            },
        );
    }

    base.handle_stop_loss_rejection(&"token_bal".to_string(), "not enough balance", "TailEnd")
        .await;

    // Should NOT be marked for GTC
    let gtc_set = base.stop_loss_use_gtc.read().await;
    assert!(
        !gtc_set.contains("token_bal"),
        "Balance rejection should NOT mark for GTC"
    );
}

// ---------------------------------------------------------------------------
// check_stop_loss GTC order type selection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn check_stop_loss_constructs_gtc_when_flagged() {
    let base = make_base_with_market("m1", 300).await;

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(49700), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    // Position with both conditions met for dual trigger
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_up", dec!(0.84), dec!(0.86));

    // Mark for GTC fallback
    {
        let mut gtc_set = base.stop_loss_use_gtc.write().await;
        gtc_set.insert("token_up".to_string());
    }

    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(result.is_some());

    let (action, exit_price, _, _trigger) = result.unwrap();
    // GTC price = current_bid - tick_size * offset = 0.84 - 0.01 * 1 = 0.83
    assert_eq!(
        exit_price,
        dec!(0.83),
        "GTC fallback should price below bid"
    );

    match action {
        Action::PlaceOrder(order) => {
            assert_eq!(
                order.order_type,
                OrderType::Gtc,
                "Should use GTC order type"
            );
            assert_eq!(order.side, OrderSide::Sell);
            assert_eq!(order.price, dec!(0.83));
        }
        _ => panic!("Expected PlaceOrder action"),
    }

    // Flag should be consumed
    let gtc_set = base.stop_loss_use_gtc.read().await;
    assert!(
        !gtc_set.contains("token_up"),
        "GTC flag should be consumed after use"
    );
}

#[tokio::test]
async fn check_stop_loss_uses_fok_when_not_flagged() {
    let base = make_base_with_market("m1", 300).await;

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((Utc::now(), dec!(49700), "test".to_string()));
        history.insert("BTC".to_string(), entries);
    }

    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    let snapshot = make_snapshot("token_up", dec!(0.84), dec!(0.86));

    // NOT marked for GTC
    let result = base.check_stop_loss(&pos, &snapshot, Utc::now()).await;
    assert!(result.is_some());

    let (action, exit_price, _, _) = result.unwrap();
    assert_eq!(exit_price, dec!(0.84), "FOK should use current bid");

    match action {
        Action::PlaceOrder(order) => {
            assert_eq!(
                order.order_type,
                OrderType::Fok,
                "Should use FOK by default"
            );
        }
        _ => panic!("Expected PlaceOrder action"),
    }
}

// ---------------------------------------------------------------------------
// Configurable cooldown schedule tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cooldown_schedule_escalates_per_rejection_kind() {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.stop_loss.liquidity_cooldowns = vec![1, 3, 7];
    config.stop_loss.balance_cooldowns = vec![10, 20, 40];
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    // First liquidity rejection → 1s cooldown
    {
        let mut psl = base.pending_stop_loss.write().await;
        psl.insert(
            "tok1".to_string(),
            PendingStopLoss {
                exit_price: dec!(0.85),
                order_type: OrderType::Fok,
            },
        );
    }
    base.handle_stop_loss_rejection(&"tok1".to_string(), "couldn't be fully filled", "Test")
        .await;
    {
        let counts = base.stop_loss_retry_counts.read().await;
        assert_eq!(*counts.get("tok1").unwrap(), 1);
    }

    // Second liquidity rejection → 3s cooldown
    {
        let mut psl = base.pending_stop_loss.write().await;
        psl.insert(
            "tok1".to_string(),
            PendingStopLoss {
                exit_price: dec!(0.85),
                order_type: OrderType::Fok,
            },
        );
    }
    base.handle_stop_loss_rejection(&"tok1".to_string(), "couldn't be fully filled", "Test")
        .await;
    {
        let counts = base.stop_loss_retry_counts.read().await;
        assert_eq!(*counts.get("tok1").unwrap(), 2);
    }

    // Balance rejection on different token → uses balance schedule
    {
        let mut psl = base.pending_stop_loss.write().await;
        psl.insert(
            "tok2".to_string(),
            PendingStopLoss {
                exit_price: dec!(0.85),
                order_type: OrderType::Fok,
            },
        );
    }
    base.handle_stop_loss_rejection(&"tok2".to_string(), "not enough balance", "Test")
        .await;
    {
        let counts = base.stop_loss_retry_counts.read().await;
        assert_eq!(*counts.get("tok2").unwrap(), 1);
    }
}

// ---------------------------------------------------------------------------
// TailEnd config backwards compat
// ---------------------------------------------------------------------------

#[test]
fn tailend_config_new_fields_default() {
    let config = super::config::TailEndConfig::default();
    assert_eq!(config.min_strike_distance_pct, dec!(0.005));
}

#[test]
fn tailend_config_deserialize_missing_strike_distance() {
    let toml_str = r#"
        enabled = true
        time_threshold_secs = 120
        ask_threshold = "0.90"
    "#;
    let config: super::config::TailEndConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.min_strike_distance_pct, dec!(0.005));
}

// ---------------------------------------------------------------------------
// Position removal cleans up GTC state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_position_clears_gtc_flag() {
    let base = make_base_no_chainlink();

    let pos = make_position(
        "m1",
        "token_cleanup",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    base.record_position(pos).await;

    // Mark for GTC and set retry count
    {
        let mut gtc_set = base.stop_loss_use_gtc.write().await;
        gtc_set.insert("token_cleanup".to_string());
    }
    {
        let mut counts = base.stop_loss_retry_counts.write().await;
        counts.insert("token_cleanup".to_string(), 3);
    }

    // Remove position
    let removed = base.remove_position_by_token("token_cleanup").await;
    assert!(removed.is_some());

    // Both GTC flag and retry count should be cleared
    let gtc_set = base.stop_loss_use_gtc.read().await;
    assert!(!gtc_set.contains("token_cleanup"));
    let counts = base.stop_loss_retry_counts.read().await;
    assert!(!counts.contains_key("token_cleanup"));
}

// ---------------------------------------------------------------------------
// Fix 1: Reconcile miss counter grace period
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconcile_miss_counter_resets_on_reappear() {
    let base = make_base_no_chainlink();

    let lo = make_open_limit_order("order-1", "market-A", "token-A-up");
    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert("order-1".to_string(), lo);
    }

    let empty_clob = std::collections::HashSet::new();

    // First miss: miss_count goes to 1
    let actions = base.reconcile_limit_orders(&empty_clob).await;
    assert!(actions.is_empty());
    {
        let limits = base.open_limit_orders.read().await;
        assert_eq!(limits["order-1"].reconcile_miss_count, 1);
    }

    // Order reappears in the next snapshot — miss_count resets to 0
    let mut clob_with_order = std::collections::HashSet::new();
    clob_with_order.insert("order-1".to_string());
    let actions = base.reconcile_limit_orders(&clob_with_order).await;
    assert!(actions.is_empty());
    {
        let limits = base.open_limit_orders.read().await;
        assert_eq!(limits["order-1"].reconcile_miss_count, 0);
    }

    // Order disappears again — needs 2 new misses, not 1
    let actions = base.reconcile_limit_orders(&empty_clob).await;
    assert!(
        actions.is_empty(),
        "Should not reconcile on first miss after reset"
    );
    {
        let limits = base.open_limit_orders.read().await;
        assert_eq!(limits["order-1"].reconcile_miss_count, 1);
    }
}

#[tokio::test]
async fn market_expiry_cleans_up_entry_orders() {
    let base = make_base_no_chainlink();

    // Add an active market
    let market = MarketWithReference {
        market: make_market_info("market-X", Utc::now() + Duration::seconds(10)),
        reference_price: dec!(50000),
        reference_quality: ReferenceQuality::Exact,
        discovery_time: Utc::now(),
        coin: "BTC".to_string(),
        window_ts: 0,
    };
    base.active_markets
        .write()
        .await
        .insert("market-X".to_string(), market);

    // Add GTC entry orders for this market
    let lo = make_open_limit_order("entry-order-1", "market-X", "token-X-up");
    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert("entry-order-1".to_string(), lo);
    }

    // Expire the market
    let actions = base.on_market_expired("market-X").await;

    // Entry order should be cancelled
    let has_cancel = actions
        .iter()
        .any(|a| matches!(a, Action::CancelOrder(id) if id == "entry-order-1"));
    assert!(has_cancel, "Should cancel entry order on market expiry");

    // Entry order should be removed from tracking (not converted to position)
    let limits = base.open_limit_orders.read().await;
    assert!(!limits.contains_key("entry-order-1"));

    // No phantom position should be created
    let positions = base.positions.read().await;
    assert!(
        !positions.contains_key("market-X"),
        "Expired entry orders should not create positions"
    );
}

// ---------------------------------------------------------------------------
// Fix 2: FOK stop-loss not tracked in gtc_stop_loss_orders
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pending_stop_loss_stores_order_type() {
    let base = make_base_no_chainlink();

    // Insert FOK pending stop-loss
    {
        let mut psl = base.pending_stop_loss.write().await;
        psl.insert(
            "token-fok".to_string(),
            PendingStopLoss {
                exit_price: dec!(0.85),
                order_type: OrderType::Fok,
            },
        );
        psl.insert(
            "token-gtc".to_string(),
            PendingStopLoss {
                exit_price: dec!(0.82),
                order_type: OrderType::Gtc,
            },
        );
    }

    // Verify we can read back order types
    let psl = base.pending_stop_loss.read().await;
    assert_eq!(psl["token-fok"].order_type, OrderType::Fok);
    assert_eq!(psl["token-fok"].exit_price, dec!(0.85));
    assert_eq!(psl["token-gtc"].order_type, OrderType::Gtc);
    assert_eq!(psl["token-gtc"].exit_price, dec!(0.82));
}

// ---------------------------------------------------------------------------
// Fix 3: Partial fill uses actual size, position reduced
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reduce_or_remove_full_close() {
    let base = make_base_no_chainlink();

    let pos = make_position(
        "m1",
        "t1",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    base.record_position(pos).await;

    // Full close: fill_size == pos.size
    let result = base
        .reduce_or_remove_position_by_token("t1", dec!(10))
        .await;
    assert!(result.is_some());
    let (snapshot, fully_closed) = result.unwrap();
    assert!(fully_closed);
    assert_eq!(snapshot.size, dec!(10));

    // Position should be completely removed
    let positions = base.positions.read().await;
    assert!(!positions.contains_key("m1"));

    // Stop-loss state should be cleared
    let counts = base.stop_loss_retry_counts.read().await;
    assert!(!counts.contains_key("t1"));
}

#[tokio::test]
async fn reduce_or_remove_partial_close() {
    let base = make_base_no_chainlink();

    let pos = make_position(
        "m1",
        "t1",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    base.record_position(pos).await;

    // Partial close: fill_size < pos.size
    let result = base.reduce_or_remove_position_by_token("t1", dec!(6)).await;
    assert!(result.is_some());
    let (snapshot, fully_closed) = result.unwrap();
    assert!(!fully_closed);
    assert_eq!(
        snapshot.size,
        dec!(10),
        "Snapshot should have original size"
    );

    // Position should still exist with reduced size
    let positions = base.positions.read().await;
    let pos_list = positions.get("m1").unwrap();
    assert_eq!(pos_list.len(), 1);
    assert_eq!(
        pos_list[0].size,
        dec!(4),
        "Remaining size should be 10 - 6 = 4"
    );

    // Stop-loss state should NOT be cleared (position still open)
    drop(positions);
    // Insert a retry count to verify it's preserved
    {
        let mut counts = base.stop_loss_retry_counts.write().await;
        counts.insert("t1".to_string(), 2);
    }
    let result2 = base.reduce_or_remove_position_by_token("t1", dec!(2)).await;
    let (_, fully_closed2) = result2.unwrap();
    assert!(!fully_closed2);
    let counts = base.stop_loss_retry_counts.read().await;
    assert_eq!(
        *counts.get("t1").unwrap(),
        2,
        "Retry count preserved on partial close"
    );
}

// ---------------------------------------------------------------------------
// StopLossConfig lifecycle field tests
// ---------------------------------------------------------------------------

#[test]
fn stop_loss_config_lifecycle_field_defaults() {
    let config = super::config::StopLossConfig::default();

    // Hard crash
    assert_eq!(config.hard_drop_abs, dec!(0.08));
    assert_eq!(config.hard_reversal_pct, dec!(0.006));
    assert_eq!(config.hard_window_ms, 2000);

    // Freshness gating
    assert_eq!(config.sl_max_book_age_ms, 1200);
    assert_eq!(config.sl_max_external_age_ms, 1500);
    assert_eq!(config.sl_min_sources, 2);
    assert_eq!(config.sl_max_dispersion_bps, dec!(50));

    // Hysteresis
    assert_eq!(config.dual_trigger_consecutive_ticks, 2);

    // Short-lived limit
    assert_eq!(config.short_limit_refresh_secs, 2);
    assert_eq!(config.short_limit_tick_offset, 1);

    // Trailing arming
    assert_eq!(config.trailing_arm_distance, dec!(0.015));

    // Execution ladder
    assert_eq!(config.exit_depth_cap_factor, dec!(0.80));
    assert_eq!(config.max_exit_retries, 5);

    // Recovery
    assert!(config.recovery_enabled);
    assert_eq!(config.recovery_max_set_cost, dec!(1.01));
    assert_eq!(config.recovery_max_extra_frac, dec!(0.15));
    assert_eq!(config.reentry_confirm_ticks, 2);
    assert_eq!(config.reentry_cooldown_secs, 8);
}

#[test]
fn stop_loss_config_lifecycle_defaults_are_sane() {
    let config = super::config::StopLossConfig::default();

    // All numeric values should be positive where required
    assert!(config.hard_drop_abs > Decimal::ZERO, "hard_drop_abs must be positive");
    assert!(config.hard_reversal_pct > Decimal::ZERO, "hard_reversal_pct must be positive");
    assert!(config.hard_window_ms > 0, "hard_window_ms must be positive");
    assert!(config.sl_max_book_age_ms > 0, "sl_max_book_age_ms must be positive");
    assert!(config.sl_max_external_age_ms > 0, "sl_max_external_age_ms must be positive");
    assert!(config.sl_min_sources > 0, "sl_min_sources must be positive");
    assert!(config.sl_max_dispersion_bps > Decimal::ZERO, "sl_max_dispersion_bps must be positive");
    assert!(config.dual_trigger_consecutive_ticks > 0, "dual_trigger_consecutive_ticks must be positive");
    assert!(config.short_limit_refresh_secs >= 1, "short_limit_refresh_secs must be >= 1");
    assert!(config.trailing_arm_distance > Decimal::ZERO, "trailing_arm_distance must be positive");
    assert!(config.exit_depth_cap_factor > Decimal::ZERO && config.exit_depth_cap_factor <= Decimal::ONE,
        "exit_depth_cap_factor must be in (0, 1]");
    assert!(config.max_exit_retries > 0, "max_exit_retries must be positive");
    assert!(config.recovery_max_set_cost > Decimal::ZERO, "recovery_max_set_cost must be positive");
    assert!(config.recovery_max_extra_frac > Decimal::ZERO && config.recovery_max_extra_frac < Decimal::ONE,
        "recovery_max_extra_frac must be in (0, 1)");
    assert!(config.reentry_confirm_ticks > 0, "reentry_confirm_ticks must be positive");
    assert!(config.reentry_cooldown_secs > 0, "reentry_cooldown_secs must be positive");
}

#[test]
fn stop_loss_config_deserialize_with_lifecycle_fields() {
    let toml_str = r#"
        reversal_pct = "0.005"
        min_drop = "0.05"
        trailing_enabled = true
        trailing_distance = "0.03"
        time_decay = true
        hard_drop_abs = "0.10"
        hard_reversal_pct = "0.008"
        hard_window_ms = 3000
        sl_max_book_age_ms = 1000
        sl_max_external_age_ms = 2000
        sl_min_sources = 3
        sl_max_dispersion_bps = "75"
        dual_trigger_consecutive_ticks = 3
        short_limit_refresh_secs = 3
        short_limit_tick_offset = 2
        trailing_arm_distance = "0.020"
        exit_depth_cap_factor = "0.70"
        max_exit_retries = 8
        recovery_enabled = false
        recovery_max_set_cost = "1.02"
        recovery_max_extra_frac = "0.20"
        reentry_confirm_ticks = 3
        reentry_cooldown_secs = 12
    "#;
    let config: super::config::StopLossConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.hard_drop_abs, dec!(0.10));
    assert_eq!(config.hard_reversal_pct, dec!(0.008));
    assert_eq!(config.hard_window_ms, 3000);
    assert_eq!(config.sl_max_book_age_ms, 1000);
    assert_eq!(config.sl_max_external_age_ms, 2000);
    assert_eq!(config.sl_min_sources, 3);
    assert_eq!(config.sl_max_dispersion_bps, dec!(75));
    assert_eq!(config.dual_trigger_consecutive_ticks, 3);
    assert_eq!(config.short_limit_refresh_secs, 3);
    assert_eq!(config.short_limit_tick_offset, 2);
    assert_eq!(config.trailing_arm_distance, dec!(0.020));
    assert_eq!(config.exit_depth_cap_factor, dec!(0.70));
    assert_eq!(config.max_exit_retries, 8);
    assert!(!config.recovery_enabled);
    assert_eq!(config.recovery_max_set_cost, dec!(1.02));
    assert_eq!(config.recovery_max_extra_frac, dec!(0.20));
    assert_eq!(config.reentry_confirm_ticks, 3);
    assert_eq!(config.reentry_cooldown_secs, 12);
}

#[test]
fn stop_loss_config_deserialize_missing_lifecycle_fields_uses_defaults() {
    // Old config without any lifecycle fields should still parse with defaults
    let toml_str = r#"
        reversal_pct = "0.005"
        min_drop = "0.05"
        trailing_enabled = true
        trailing_distance = "0.03"
        time_decay = true
    "#;
    let config: super::config::StopLossConfig = toml::from_str(toml_str).unwrap();
    // All new lifecycle fields should have their defaults
    assert_eq!(config.hard_drop_abs, dec!(0.08));
    assert_eq!(config.hard_reversal_pct, dec!(0.006));
    assert_eq!(config.hard_window_ms, 2000);
    assert_eq!(config.sl_max_book_age_ms, 1200);
    assert_eq!(config.sl_max_external_age_ms, 1500);
    assert_eq!(config.sl_min_sources, 2);
    assert_eq!(config.sl_max_dispersion_bps, dec!(50));
    assert_eq!(config.dual_trigger_consecutive_ticks, 2);
    assert_eq!(config.short_limit_refresh_secs, 2);
    assert_eq!(config.short_limit_tick_offset, 1);
    assert_eq!(config.trailing_arm_distance, dec!(0.015));
    assert_eq!(config.exit_depth_cap_factor, dec!(0.80));
    assert_eq!(config.max_exit_retries, 5);
    assert!(config.recovery_enabled);
    assert_eq!(config.recovery_max_set_cost, dec!(1.01));
    assert_eq!(config.recovery_max_extra_frac, dec!(0.15));
    assert_eq!(config.reentry_confirm_ticks, 2);
    assert_eq!(config.reentry_cooldown_secs, 8);
}

// ---------------------------------------------------------------------------
// Config validation tests (Task 3)
// ---------------------------------------------------------------------------

#[test]
fn stop_loss_validate_trailing_min_greater_than_distance_errors() {
    let mut config = super::config::StopLossConfig::default();
    config.trailing_min_distance = dec!(0.10);
    config.trailing_distance = dec!(0.05);
    let result = config.validate();
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(
        msg.contains("trailing_min_distance") && msg.contains("trailing_distance"),
        "Error message should mention both fields, got: {msg}"
    );
}

#[test]
fn stop_loss_validate_short_limit_refresh_zero_errors() {
    let mut config = super::config::StopLossConfig::default();
    config.short_limit_refresh_secs = 0;
    let result = config.validate();
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("short_limit_refresh_secs"));
}

#[test]
fn stop_loss_validate_empty_cooldowns_errors() {
    let mut config = super::config::StopLossConfig::default();
    config.liquidity_cooldowns = vec![];
    assert!(config.validate().is_err());

    let mut config = super::config::StopLossConfig::default();
    config.balance_cooldowns = vec![];
    assert!(config.validate().is_err());
}

#[test]
fn stop_loss_validate_exit_depth_cap_factor_bounds() {
    // Zero is invalid
    let mut config = super::config::StopLossConfig::default();
    config.exit_depth_cap_factor = Decimal::ZERO;
    assert!(config.validate().is_err());

    // Negative is invalid
    let mut config = super::config::StopLossConfig::default();
    config.exit_depth_cap_factor = dec!(-0.5);
    assert!(config.validate().is_err());

    // > 1 is invalid
    let mut config = super::config::StopLossConfig::default();
    config.exit_depth_cap_factor = dec!(1.5);
    assert!(config.validate().is_err());

    // Exactly 1.0 is valid
    let mut config = super::config::StopLossConfig::default();
    config.exit_depth_cap_factor = Decimal::ONE;
    assert!(config.validate().is_ok());
}

#[test]
fn stop_loss_validate_valid_config_passes() {
    let config = super::config::StopLossConfig::default();
    assert!(config.validate().is_ok(), "Default StopLossConfig should pass validation");
}

#[test]
fn arb_config_validate_post_entry_lte_sell_delay_errors() {
    let mut config = super::config::ArbitrageConfig::default();
    config.tailend.post_entry_window_secs = 10;
    config.tailend.min_sell_delay_secs = 10; // equal — unreachable
    let result = config.validate();
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(
        msg.contains("post_entry_window_secs") && msg.contains("min_sell_delay_secs"),
        "Error message should mention both fields, got: {msg}"
    );

    // Also test when post < sell delay
    let mut config = super::config::ArbitrageConfig::default();
    config.tailend.post_entry_window_secs = 5;
    config.tailend.min_sell_delay_secs = 10;
    assert!(config.validate().is_err());
}

#[test]
fn arb_config_validate_valid_config_passes() {
    let config = super::config::ArbitrageConfig::default();
    assert!(config.validate().is_ok(), "Default ArbitrageConfig should pass validation");
}

#[test]
fn arb_config_validate_dead_zone_warning() {
    // This test verifies the dead zone check doesn't return an error
    // (it only warns), even with a large dead zone.
    let mut config = super::config::ArbitrageConfig::default();
    config.stop_loss.reversal_pct = dec!(0.010); // 1%
    config.tailend.min_strike_distance_pct = dec!(0.001); // 0.1%
    // Dead zone = 0.009 > 0.003 — triggers warning but not error
    assert!(config.validate().is_ok(), "Dead zone should warn but not error");
}

// ---------------------------------------------------------------------------
// Position Lifecycle State Machine Tests
// ---------------------------------------------------------------------------

fn now() -> DateTime<Utc> {
    Utc::now()
}

#[test]
fn lifecycle_new_starts_healthy() {
    let lc = PositionLifecycle::new();
    assert_eq!(lc.state, PositionLifecycleState::Healthy);
    assert_eq!(lc.dual_trigger_ticks, 0);
    assert!(!lc.trailing_unarmable);
    assert!(lc.last_composite.is_none());
    assert!(lc.last_composite_at.is_none());
    assert!(lc.pending_exit_order_id.is_none());
    assert!(lc.transition_log.is_empty());
}

#[test]
fn lifecycle_all_valid_transitions_succeed() {
    let t = now();

    // Healthy -> DeferredExit
    let mut lc = PositionLifecycle::new();
    let result = lc.transition(
        PositionLifecycleState::DeferredExit {
            trigger: StopLossTriggerKind::PostEntryExit { bid_drop: dec!(0.05) },
            armed_at: t,
        },
        "trigger during sell delay",
        t,
    );
    assert!(result.is_ok());

    // DeferredExit -> ExitExecuting
    let result = lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "order-1".to_string(),
            order_type: OrderType::Fok,
            exit_price: dec!(0.90),
            submitted_at: t,
        },
        "delay elapsed",
        t,
    );
    assert!(result.is_ok());

    // ExitExecuting -> ResidualRisk
    let result = lc.transition(
        PositionLifecycleState::ResidualRisk {
            remaining_size: dec!(5.0),
            retry_count: 1,
            last_attempt: t,
            use_gtc_next: true,
        },
        "partial fill",
        t,
    );
    assert!(result.is_ok());

    // ResidualRisk -> ExitExecuting (retry)
    let result = lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "order-2".to_string(),
            order_type: OrderType::Gtc,
            exit_price: dec!(0.89),
            submitted_at: t,
        },
        "retry exit",
        t,
    );
    assert!(result.is_ok());

    // ExitExecuting -> ResidualRisk again
    let result = lc.transition(
        PositionLifecycleState::ResidualRisk {
            remaining_size: dec!(3.0),
            retry_count: 2,
            last_attempt: t,
            use_gtc_next: false,
        },
        "rejected again",
        t,
    );
    assert!(result.is_ok());

    // ResidualRisk -> RecoveryProbe
    let result = lc.transition(
        PositionLifecycleState::RecoveryProbe {
            recovery_order_id: "recovery-1".to_string(),
            probe_side: OutcomeSide::Down,
            submitted_at: t,
        },
        "max retries, attempt recovery",
        t,
    );
    assert!(result.is_ok());

    // RecoveryProbe -> Cooldown
    let result = lc.transition(
        PositionLifecycleState::Cooldown { until: t + Duration::seconds(8) },
        "recovery filled",
        t,
    );
    assert!(result.is_ok());

    // Cooldown -> Healthy
    let result = lc.transition(PositionLifecycleState::Healthy, "cooldown elapsed", t);
    assert!(result.is_ok());

    // Verify transitions are logged
    assert_eq!(lc.transition_log.len(), 8);
}

#[test]
fn lifecycle_healthy_to_exit_executing() {
    let t = now();
    let mut lc = PositionLifecycle::new();
    let result = lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "order-1".to_string(),
            order_type: OrderType::Fok,
            exit_price: dec!(0.92),
            submitted_at: t,
        },
        "hard crash trigger",
        t,
    );
    assert!(result.is_ok());
    assert!(matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }));
}

#[test]
fn lifecycle_deferred_exit_to_healthy() {
    let t = now();
    let mut lc = PositionLifecycle::new();
    lc.transition(
        PositionLifecycleState::DeferredExit {
            trigger: StopLossTriggerKind::DualTrigger { consecutive_ticks: 2 },
            armed_at: t,
        },
        "dual trigger armed",
        t,
    )
    .unwrap();

    // Trigger cleared
    let result = lc.transition(PositionLifecycleState::Healthy, "trigger cleared", t);
    assert!(result.is_ok());
    assert_eq!(lc.state, PositionLifecycleState::Healthy);
}

#[test]
fn lifecycle_recovery_probe_to_exit_executing() {
    let t = now();
    let mut lc = PositionLifecycle::new();
    // Walk through: Healthy -> ExitExecuting -> ResidualRisk -> RecoveryProbe -> ExitExecuting
    lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "o1".into(),
            order_type: OrderType::Fok,
            exit_price: dec!(0.90),
            submitted_at: t,
        },
        "trigger",
        t,
    )
    .unwrap();
    lc.transition(
        PositionLifecycleState::ResidualRisk {
            remaining_size: dec!(5),
            retry_count: 5,
            last_attempt: t,
            use_gtc_next: false,
        },
        "rejected",
        t,
    )
    .unwrap();
    lc.transition(
        PositionLifecycleState::RecoveryProbe {
            recovery_order_id: "r1".into(),
            probe_side: OutcomeSide::Down,
            submitted_at: t,
        },
        "try recovery",
        t,
    )
    .unwrap();

    // Recovery fails -> back to ExitExecuting
    let result = lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "o2".into(),
            order_type: OrderType::Fok,
            exit_price: dec!(0.88),
            submitted_at: t,
        },
        "recovery failed",
        t,
    );
    assert!(result.is_ok());
}

#[test]
fn lifecycle_invalid_transitions_return_error() {
    let t = now();

    // Healthy -> ResidualRisk (invalid: must go through ExitExecuting)
    let mut lc = PositionLifecycle::new();
    let result = lc.transition(
        PositionLifecycleState::ResidualRisk {
            remaining_size: dec!(5),
            retry_count: 0,
            last_attempt: t,
            use_gtc_next: false,
        },
        "skip ExitExecuting",
        t,
    );
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("Healthy") && msg.contains("ResidualRisk"));

    // Healthy -> RecoveryProbe (invalid)
    let mut lc = PositionLifecycle::new();
    let result = lc.transition(
        PositionLifecycleState::RecoveryProbe {
            recovery_order_id: "r1".into(),
            probe_side: OutcomeSide::Up,
            submitted_at: t,
        },
        "skip to recovery",
        t,
    );
    assert!(result.is_err());

    // Healthy -> Cooldown (invalid)
    let mut lc = PositionLifecycle::new();
    let result = lc.transition(
        PositionLifecycleState::Cooldown { until: t },
        "skip to cooldown",
        t,
    );
    assert!(result.is_err());

    // Cooldown -> ExitExecuting (invalid: must go through Healthy)
    let mut lc = PositionLifecycle::new();
    // Walk to Cooldown state
    lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "o1".into(),
            order_type: OrderType::Fok,
            exit_price: dec!(0.9),
            submitted_at: t,
        },
        "trigger",
        t,
    )
    .unwrap();
    lc.transition(
        PositionLifecycleState::ResidualRisk {
            remaining_size: dec!(5),
            retry_count: 5,
            last_attempt: t,
            use_gtc_next: false,
        },
        "rejected",
        t,
    )
    .unwrap();
    lc.transition(
        PositionLifecycleState::RecoveryProbe {
            recovery_order_id: "r1".into(),
            probe_side: OutcomeSide::Down,
            submitted_at: t,
        },
        "try recovery",
        t,
    )
    .unwrap();
    lc.transition(
        PositionLifecycleState::Cooldown { until: t },
        "recovered",
        t,
    )
    .unwrap();

    let result = lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "o2".into(),
            order_type: OrderType::Fok,
            exit_price: dec!(0.85),
            submitted_at: t,
        },
        "try exit from cooldown",
        t,
    );
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("Cooldown") && msg.contains("ExitExecuting"));

    // DeferredExit -> ResidualRisk (invalid: must go through ExitExecuting)
    let mut lc = PositionLifecycle::new();
    lc.transition(
        PositionLifecycleState::DeferredExit {
            trigger: StopLossTriggerKind::PostEntryExit { bid_drop: dec!(0.05) },
            armed_at: t,
        },
        "deferred",
        t,
    )
    .unwrap();
    let result = lc.transition(
        PositionLifecycleState::ResidualRisk {
            remaining_size: dec!(5),
            retry_count: 0,
            last_attempt: t,
            use_gtc_next: false,
        },
        "skip ExitExecuting",
        t,
    );
    assert!(result.is_err());
}

#[test]
fn lifecycle_transition_log_caps_at_50() {
    let t = now();
    let mut lc = PositionLifecycle::new();

    // Generate 60 transitions by cycling Healthy -> ExitExecuting -> ResidualRisk -> ExitExecuting...
    for i in 0..60 {
        if matches!(lc.state, PositionLifecycleState::Healthy) {
            lc.transition(
                PositionLifecycleState::ExitExecuting {
                    order_id: format!("o-{i}"),
                    order_type: OrderType::Fok,
                    exit_price: dec!(0.90),
                    submitted_at: t,
                },
                &format!("transition {i}"),
                t,
            )
            .unwrap();
        } else if matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }) {
            lc.transition(
                PositionLifecycleState::ResidualRisk {
                    remaining_size: dec!(5),
                    retry_count: i as u32,
                    last_attempt: t,
                    use_gtc_next: false,
                },
                &format!("transition {i}"),
                t,
            )
            .unwrap();
        } else if matches!(lc.state, PositionLifecycleState::ResidualRisk { .. }) {
            lc.transition(
                PositionLifecycleState::ExitExecuting {
                    order_id: format!("o-{i}"),
                    order_type: OrderType::Gtc,
                    exit_price: dec!(0.89),
                    submitted_at: t,
                },
                &format!("transition {i}"),
                t,
            )
            .unwrap();
        }
    }

    assert!(
        lc.transition_log.len() <= 50,
        "Transition log should be capped at 50, got {}",
        lc.transition_log.len()
    );
}

#[test]
fn lifecycle_invalid_transition_preserves_state() {
    let t = now();
    let mut lc = PositionLifecycle::new();

    // Try invalid transition — state should remain Healthy
    let _ = lc.transition(
        PositionLifecycleState::ResidualRisk {
            remaining_size: dec!(5),
            retry_count: 0,
            last_attempt: t,
            use_gtc_next: false,
        },
        "should fail",
        t,
    );
    assert_eq!(lc.state, PositionLifecycleState::Healthy);
    assert!(lc.transition_log.is_empty(), "Failed transition should not log");
}

#[test]
fn stop_loss_trigger_kind_display() {
    let trigger = StopLossTriggerKind::HardCrash {
        bid_drop: dec!(0.08),
        reversal_pct: dec!(0.006),
    };
    let s = format!("{trigger}");
    assert!(s.contains("HardCrash"));
    assert!(s.contains("0.08"));

    let trigger = StopLossTriggerKind::TrailingStop {
        peak_bid: dec!(0.97),
        current_bid: dec!(0.92),
        effective_distance: dec!(0.03),
    };
    let s = format!("{trigger}");
    assert!(s.contains("TrailingStop"));
    assert!(s.contains("0.97"));
}

#[test]
fn lifecycle_state_display() {
    assert_eq!(format!("{}", PositionLifecycleState::Healthy), "Healthy");

    let state = PositionLifecycleState::ExitExecuting {
        order_id: "o1".into(),
        order_type: OrderType::Fok,
        exit_price: dec!(0.92),
        submitted_at: now(),
    };
    let s = format!("{state}");
    assert!(s.contains("ExitExecuting"));
    assert!(s.contains("Fok"));
    assert!(s.contains("0.92"));
}

#[test]
fn exit_order_meta_fields() {
    let meta = ExitOrderMeta {
        token_id: "token-123".to_string(),
        order_type: OrderType::Fok,
        source_state: "ExitExecuting".to_string(),
    };
    assert_eq!(meta.token_id, "token-123");
    assert_eq!(meta.order_type, OrderType::Fok);
    assert_eq!(meta.source_state, "ExitExecuting");
}

// ---------------------------------------------------------------------------
// ArbitragePosition entry fee/order metadata tests (Task 5)
// ---------------------------------------------------------------------------

#[test]
fn gtc_entry_has_zero_fee_per_share() {
    let lo = OpenLimitOrder {
        order_id: "gtc-order-1".to_string(),
        market_id: "market-1".to_string(),
        token_id: "token-1".to_string(),
        side: OutcomeSide::Up,
        price: dec!(0.92),
        size: dec!(10),
        reference_price: dec!(50000),
        coin: "BTC".to_string(),
        placed_at: Utc::now(),
        kelly_fraction: Some(dec!(0.15)),
        estimated_fee: Decimal::ZERO,
        tick_size: dec!(0.01),
        fee_rate_bps: 315,
        cancel_pending: false,
        reconcile_miss_count: 0,
    };

    let pos = ArbitragePosition::from_limit_order(
        &lo,
        dec!(0.92),
        dec!(10),
        Some("gtc-order-1".to_string()),
        Utc::now(),
    );

    assert_eq!(pos.entry_order_type, OrderType::Gtc);
    assert_eq!(pos.entry_fee_per_share, Decimal::ZERO);
    assert_eq!(pos.realized_pnl, Decimal::ZERO);
}

#[test]
fn fok_entry_has_computed_taker_fee_per_share() {
    let price = dec!(0.92);
    let fee_rate = dec!(0.0315);
    let expected_fee = taker_fee(price, fee_rate);

    // Simulate FOK position construction (struct literal path used in tailend.rs)
    let pos = ArbitragePosition {
        market_id: "market-1".to_string(),
        token_id: "token-1".to_string(),
        side: OutcomeSide::Up,
        entry_price: price,
        size: dec!(10),
        reference_price: dec!(50000),
        coin: "BTC".to_string(),
        order_id: Some("fok-order-1".to_string()),
        entry_time: Utc::now(),
        kelly_fraction: None,
        peak_bid: price,
        estimated_fee: expected_fee,
        entry_market_price: price,
        tick_size: dec!(0.01),
        fee_rate_bps: 315,
        entry_order_type: OrderType::Fok,
        entry_fee_per_share: expected_fee,
        realized_pnl: Decimal::ZERO,
    };

    assert_eq!(pos.entry_order_type, OrderType::Fok);
    assert_eq!(pos.entry_fee_per_share, expected_fee);
    assert!(pos.entry_fee_per_share > Decimal::ZERO);
    // At p=0.92: fee = 2 * 0.92 * 0.08 * 0.0315 = 0.0046368
    assert_eq!(pos.entry_fee_per_share, dec!(0.0046368));
    assert_eq!(pos.realized_pnl, Decimal::ZERO);
}

#[test]
fn composite_price_snapshot_clone() {
    let snap = CompositePriceSnapshot {
        price: dec!(50000.50),
        sources_used: 3,
        max_lag_ms: 800,
        dispersion_bps: dec!(12.5),
    };
    let cloned = snap.clone();
    assert_eq!(cloned.price, dec!(50000.50));
    assert_eq!(cloned.sources_used, 3);
}

// ---------------------------------------------------------------------------
// Lifecycle store tests (Task 9)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn position_creation_creates_lifecycle_in_healthy_state() {
    let base = make_base_with_market("m1", 300).await;

    // Create and record a position
    let pos = make_position("m1", "token_up", OutcomeSide::Up, dec!(0.90), dec!(10), dec!(50000), dec!(0.90));
    base.record_position(pos).await;

    // Verify lifecycle was created in Healthy state
    let lifecycles = base.position_lifecycle.read().await;
    assert!(lifecycles.contains_key("token_up"));
    let lc = lifecycles.get("token_up").unwrap();
    assert_eq!(lc.state, PositionLifecycleState::Healthy);
}

#[tokio::test]
async fn position_removal_cleans_up_lifecycle() {
    let base = make_base_with_market("m1", 300).await;

    // Create and record a position
    let pos = make_position("m1", "token_up", OutcomeSide::Up, dec!(0.90), dec!(10), dec!(50000), dec!(0.90));
    base.record_position(pos).await;

    // Verify lifecycle exists
    {
        let lifecycles = base.position_lifecycle.read().await;
        assert!(lifecycles.contains_key("token_up"));
    }

    // Remove the position
    base.remove_position_by_token("token_up").await;

    // Verify lifecycle was cleaned up
    let lifecycles = base.position_lifecycle.read().await;
    assert!(!lifecycles.contains_key("token_up"));
}

#[tokio::test]
async fn partial_close_preserves_lifecycle() {
    let base = make_base_with_market("m1", 300).await;

    // Create and record a position of size 10
    let pos = make_position("m1", "token_up", OutcomeSide::Up, dec!(0.90), dec!(10), dec!(50000), dec!(0.90));
    base.record_position(pos).await;

    // Partially close (5 of 10)
    let result = base
        .reduce_or_remove_position_by_token("token_up", dec!(5))
        .await;
    assert!(result.is_some());
    let (_, fully_closed) = result.unwrap();
    assert!(!fully_closed);

    // Lifecycle should still exist (not fully closed)
    let lifecycles = base.position_lifecycle.read().await;
    assert!(lifecycles.contains_key("token_up"));
}

#[tokio::test]
async fn full_close_via_reduce_removes_lifecycle() {
    let base = make_base_with_market("m1", 300).await;

    // Create and record a position of size 10
    let pos = make_position("m1", "token_up", OutcomeSide::Up, dec!(0.90), dec!(10), dec!(50000), dec!(0.90));
    base.record_position(pos).await;

    // Fully close (10 of 10)
    let result = base
        .reduce_or_remove_position_by_token("token_up", dec!(10))
        .await;
    assert!(result.is_some());
    let (_, fully_closed) = result.unwrap();
    assert!(fully_closed);

    // Lifecycle should be cleaned up
    let lifecycles = base.position_lifecycle.read().await;
    assert!(!lifecycles.contains_key("token_up"));
}

#[tokio::test]
async fn ensure_lifecycle_creates_healthy_if_missing() {
    let base = make_base_with_market("m1", 300).await;

    // No lifecycle exists yet
    {
        let lifecycles = base.position_lifecycle.read().await;
        assert!(!lifecycles.contains_key("token_orphan"));
    }

    // ensure_lifecycle creates one
    let lc = base.ensure_lifecycle("token_orphan").await;
    assert_eq!(lc.state, PositionLifecycleState::Healthy);

    // It persists in the store
    let lifecycles = base.position_lifecycle.read().await;
    assert!(lifecycles.contains_key("token_orphan"));
}

#[tokio::test]
async fn remove_lifecycle_also_cleans_exit_orders() {
    let base = make_base_with_market("m1", 300).await;

    // Create a position and its lifecycle
    let pos = make_position("m1", "token_up", OutcomeSide::Up, dec!(0.90), dec!(10), dec!(50000), dec!(0.90));
    base.record_position(pos).await;

    // Simulate adding an exit order for this token
    {
        use super::types::ExitOrderMeta;
        let mut exit_orders = base.exit_orders_by_id.write().await;
        exit_orders.insert(
            "exit-order-1".to_string(),
            ExitOrderMeta {
                token_id: "token_up".to_string(),
                order_type: OrderType::Fok,
                source_state: "ExitExecuting".to_string(),
            },
        );
        // Add an unrelated exit order too
        exit_orders.insert(
            "exit-order-2".to_string(),
            ExitOrderMeta {
                token_id: "other_token".to_string(),
                order_type: OrderType::Gtc,
                source_state: "ResidualRisk".to_string(),
            },
        );
    }

    // Remove the position — should also clean up exit orders for that token
    base.remove_position_by_token("token_up").await;

    // exit-order-1 should be gone, exit-order-2 should remain
    let exit_orders = base.exit_orders_by_id.read().await;
    assert!(!exit_orders.contains_key("exit-order-1"));
    assert!(exit_orders.contains_key("exit-order-2"));
}

// ---------------------------------------------------------------------------
// Task 10: Composite price cache for stop-loss
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sl_composite_cache_fresh_returns_result() {
    use super::base::CompositePriceResult;

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

    // Request with generous age limit — should return the cached composite
    let result = base.get_sl_composite("BTC", 5000, now).await;
    assert!(result.is_some(), "Fresh composite should be returned");
    let r = result.unwrap();
    assert_eq!(r.price, dec!(88000));
    assert_eq!(r.sources_used, 3);
}

#[tokio::test]
async fn sl_composite_cache_stale_returns_none() {
    use super::base::CompositePriceResult;

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

    // Request with 1200ms age limit — 10s old entry should be stale
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
        entries.push_back((
            now - chrono::Duration::milliseconds(500),
            dec!(88500),
            "binance-spot".to_string(),
        ));
        history.insert("BTC".to_string(), entries);
    }

    // 500ms old entry, 1500ms limit — should return
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
        entries.push_back((
            now - chrono::Duration::seconds(5),
            dec!(88500),
            "binance-spot".to_string(),
        ));
        history.insert("BTC".to_string(), entries);
    }

    // 5s old entry, 1500ms limit — should be stale
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
    use super::base::CompositePriceResult;

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
        let snapshot =
            super::types::CompositePriceSnapshot::from_result(&composite);
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
// Task 11: evaluate_triggers tests
// ---------------------------------------------------------------------------

use super::config::{StopLossConfig, TailEndConfig};

/// Helper to create a default TriggerEvalContext for testing.
fn make_trigger_ctx(
    entry_price: Decimal,
    peak_bid: Decimal,
    current_bid: Decimal,
    reference_price: Decimal,
    external_price: Option<Decimal>,
    time_remaining: i64,
    seconds_since_entry: i64,
) -> TriggerEvalContext {
    let now = Utc::now();
    let entry_time = now - Duration::seconds(seconds_since_entry);
    TriggerEvalContext {
        entry_price,
        peak_bid,
        side: OutcomeSide::Up,
        reference_price,
        tick_size: dec!(0.01),
        entry_time,
        current_bid,
        book_age_ms: 500, // fresh by default
        external_price,
        external_age_ms: external_price.map(|_| 500i64), // fresh by default
        composite_sources: external_price.map(|_| 3usize),
        time_remaining,
        now,
    }
}

#[test]
fn evaluate_triggers_hard_crash_bid_drop() {
    // Hard crash fires when bid drops >= hard_drop_abs (0.08) from entry
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    // Entry at 0.95, bid dropped to 0.86 => drop = 0.09 >= 0.08
    let ctx = make_trigger_ctx(
        dec!(0.95),  // entry
        dec!(0.95),  // peak (no profit yet)
        dec!(0.86),  // current bid (dropped 0.09)
        dec!(90000), // reference BTC price
        Some(dec!(90000)), // no external reversal
        300,         // time remaining
        15,          // seconds since entry (past sell delay)
    );

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(trigger.is_some(), "Hard crash should fire on 0.09 bid drop");
    match trigger.unwrap() {
        StopLossTriggerKind::HardCrash { bid_drop, .. } => {
            assert_eq!(bid_drop, dec!(0.09));
        }
        other => panic!("Expected HardCrash, got {other}"),
    }
}

#[test]
fn evaluate_triggers_hard_crash_external_reversal() {
    // Hard crash fires on external reversal >= 0.6%
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    // Up position: reference=90000, external dropped to 89400 => reversal = 600/90000 = 0.667%
    let ctx = make_trigger_ctx(
        dec!(0.95),  // entry
        dec!(0.95),  // peak
        dec!(0.94),  // bid only dropped 0.01 (not enough for bid-based hard crash)
        dec!(90000), // reference
        Some(dec!(89400)), // external dropped 0.667%
        300,
        15,
    );

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(trigger.is_some(), "Hard crash should fire on 0.667% reversal");
    match trigger.unwrap() {
        StopLossTriggerKind::HardCrash { reversal_pct, .. } => {
            // reversal = (90000-89400)/90000 = 600/90000 ≈ 0.00667
            assert!(reversal_pct >= dec!(0.006), "Reversal {reversal_pct} should be >= 0.006");
        }
        other => panic!("Expected HardCrash, got {other}"),
    }
}

#[test]
fn evaluate_triggers_hard_crash_works_with_stale_composite() {
    // Hard crash only needs 1 fresh source, NOT a full composite.
    // Set composite_sources to None (single source) but external price is fresh.
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    let now = Utc::now();
    let ctx = TriggerEvalContext {
        entry_price: dec!(0.95),
        peak_bid: dec!(0.95),
        side: OutcomeSide::Up,
        reference_price: dec!(90000),
        tick_size: dec!(0.01),
        entry_time: now - Duration::seconds(15),
        current_bid: dec!(0.86), // bid drop = 0.09 >= 0.08
        book_age_ms: 500,
        external_price: Some(dec!(90000)),
        external_age_ms: Some(500), // fresh single source
        composite_sources: None,    // NO composite available
        time_remaining: 300,
        now,
    };

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(trigger.is_some(), "Hard crash should work with single fresh source (no composite)");
    assert!(matches!(trigger.unwrap(), StopLossTriggerKind::HardCrash { .. }));
}

#[test]
fn evaluate_triggers_dual_trigger_requires_consecutive_ticks() {
    // Dual trigger needs 2 consecutive ticks (default) where both conditions hold.
    // First tick: returns None (counter = 1), second tick: returns trigger (counter = 2).
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    // Both conditions met: crypto reversed 0.5% and market dropped 0.05
    // Use a small bid drop that doesn't reach hard_drop_abs (0.08)
    let ctx = make_trigger_ctx(
        dec!(0.95),  // entry
        dec!(0.95),  // peak
        dec!(0.90),  // bid dropped 0.05 (= min_drop, satisfies market_dropped but not hard crash)
        dec!(90000), // reference
        Some(dec!(89700)), // reversal = 300/90000 = 0.333% >= reversal_pct (0.003)
        300,
        15,
    );

    // First tick: counter goes to 1, not yet at threshold (2)
    let trigger1 = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(trigger1.is_none(), "First tick should not trigger (need 2 consecutive)");
    assert_eq!(lifecycle.dual_trigger_ticks, 1);

    // Second tick: counter goes to 2, threshold met
    let trigger2 = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(trigger2.is_some(), "Second tick should trigger (2 consecutive)");
    match trigger2.unwrap() {
        StopLossTriggerKind::DualTrigger { consecutive_ticks } => {
            assert_eq!(consecutive_ticks, 2);
        }
        other => panic!("Expected DualTrigger, got {other}"),
    }
}

#[test]
fn evaluate_triggers_dual_trigger_resets_on_clear() {
    // Dual trigger counter resets when either condition clears.
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    // Tick 1: both conditions met
    let ctx1 = make_trigger_ctx(
        dec!(0.95),
        dec!(0.95),
        dec!(0.90), // market dropped 0.05
        dec!(90000),
        Some(dec!(89700)), // crypto reversed
        300,
        15,
    );
    lifecycle.evaluate_triggers(&ctx1, &sl, &te);
    assert_eq!(lifecycle.dual_trigger_ticks, 1);

    // Tick 2: market condition clears (bid recovers)
    let ctx2 = make_trigger_ctx(
        dec!(0.95),
        dec!(0.95),
        dec!(0.94), // market only dropped 0.01, below min_drop (0.05)
        dec!(90000),
        Some(dec!(89700)), // crypto still reversed
        300,
        16,
    );
    lifecycle.evaluate_triggers(&ctx2, &sl, &te);
    assert_eq!(lifecycle.dual_trigger_ticks, 0, "Counter should reset when market condition clears");
}

#[test]
fn evaluate_triggers_trailing_unarmable_at_high_entry() {
    // Entry at 0.99 with tick_size 0.01: price_cap = 0.99, headroom = 0,
    // effective_arm_distance = min(0.015, 0) = 0 < 0.01 => trailing_unarmable
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    let ctx = make_trigger_ctx(
        dec!(0.99),  // entry (very high)
        dec!(0.99),  // peak
        dec!(0.97),  // bid dropped 0.02
        dec!(90000),
        Some(dec!(90000)), // no reversal
        300,
        15,
    );

    let _trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);

    // Hard crash doesn't fire (bid_drop=0.02 < 0.08, no external reversal)
    // Dual trigger doesn't fire (market_dropped but no crypto reversal)
    // Trailing should be marked unarmable
    assert!(lifecycle.trailing_unarmable, "Trailing should be unarmable at entry 0.99");

    // Hard crash should still work at entry 0.99 (higher priority than trailing).
    // Since hard crash returns early, trailing_unarmable may not be set on the same call.
    // But we already verified it was set on the previous call above.
    let ctx2 = make_trigger_ctx(
        dec!(0.99),
        dec!(0.99),
        dec!(0.90), // bid drop = 0.09 >= 0.08 (hard crash)
        dec!(90000),
        Some(dec!(90000)),
        300,
        15,
    );
    let trigger2 = lifecycle.evaluate_triggers(&ctx2, &sl, &te);
    assert!(trigger2.is_some(), "Hard crash should work even when trailing is unarmable");
    assert!(matches!(trigger2.unwrap(), StopLossTriggerKind::HardCrash { .. }));
}

#[test]
fn evaluate_triggers_trailing_at_normal_entry() {
    // Entry at 0.90: price_cap=0.99, headroom=0.09,
    // effective_arm_distance = min(0.015, 0.09) = 0.015
    // Arms at peak >= 0.90 + 0.015 = 0.915
    // At 450s: decay_factor=0.5, raw=0.05*0.5=0.025, floor=max(0.025,0.015)=0.025
    // Triggers when drop_from_peak >= 0.025
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    // Peak at 0.96, current bid at 0.93 => drop = 0.03 >= 0.025
    let ctx = make_trigger_ctx(
        dec!(0.90),  // entry
        dec!(0.96),  // peak (armed: 0.96 >= 0.915)
        dec!(0.93),  // current bid
        dec!(90000),
        Some(dec!(90000)),
        450, // time remaining
        15,
    );

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(trigger.is_some(), "Trailing should trigger on 0.03 drop from peak at 450s");
    match trigger.unwrap() {
        StopLossTriggerKind::TrailingStop { peak_bid, current_bid, effective_distance } => {
            assert_eq!(peak_bid, dec!(0.96));
            assert_eq!(current_bid, dec!(0.93));
            assert_eq!(effective_distance, dec!(0.025));
        }
        other => panic!("Expected TrailingStop, got {other}"),
    }
    assert!(!lifecycle.trailing_unarmable, "Should NOT be unarmable at entry 0.90");
}

#[test]
fn evaluate_triggers_trailing_time_decay() {
    // At 90s remaining: decay_factor=90/900=0.1, raw=0.05*0.1=0.005,
    // floor=max(0.005, 0.015)=0.015 (floor kicks in)
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    // Peak at 0.96, current bid at 0.944 => drop = 0.016 >= 0.015
    let ctx = make_trigger_ctx(
        dec!(0.90),  // entry
        dec!(0.96),  // peak (armed)
        dec!(0.944), // drop = 0.016
        dec!(90000),
        Some(dec!(90000)),
        90, // 90s remaining => heavy time decay
        15,
    );

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(trigger.is_some(), "Trailing should trigger with floor distance at 90s");
    match trigger.unwrap() {
        StopLossTriggerKind::TrailingStop { effective_distance, .. } => {
            assert_eq!(effective_distance, dec!(0.015), "Floor should prevent decay below 0.015");
        }
        other => panic!("Expected TrailingStop, got {other}"),
    }
}

#[test]
fn evaluate_triggers_post_entry_deferred() {
    // Post-entry deferred triggers within sell delay window when bid drops
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    // Within sell delay (5s < min_sell_delay_secs=10) and post_entry_window (5s <= 20s)
    // Bid dropped 0.06 from entry (>= post_entry_exit_drop=0.05)
    let ctx = make_trigger_ctx(
        dec!(0.95),  // entry
        dec!(0.95),  // peak
        dec!(0.89),  // bid dropped 0.06
        dec!(90000),
        Some(dec!(90000)),
        300,
        5, // 5s since entry (within sell delay of 10s)
    );

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(trigger.is_some(), "Post-entry deferred should fire within sell delay");
    match trigger.unwrap() {
        StopLossTriggerKind::PostEntryExit { bid_drop } => {
            assert_eq!(bid_drop, dec!(0.06));
        }
        other => panic!("Expected PostEntryExit, got {other}"),
    }
}

#[test]
fn evaluate_triggers_stale_orderbook_suppresses_all_except_hard_crash() {
    // Stale orderbook (book_age_ms > sl_max_book_age_ms) should suppress
    // dual trigger, trailing, and post-entry. Only hard crash with fresh external works.
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    let now = Utc::now();
    // Stale book, but conditions that would normally trigger dual trigger
    let ctx = TriggerEvalContext {
        entry_price: dec!(0.95),
        peak_bid: dec!(0.96),
        side: OutcomeSide::Up,
        reference_price: dec!(90000),
        tick_size: dec!(0.01),
        entry_time: now - Duration::seconds(15),
        current_bid: dec!(0.90),
        book_age_ms: 5000, // STALE book (> 1200ms)
        external_price: Some(dec!(89700)), // crypto reversed
        external_age_ms: Some(500),
        composite_sources: Some(3),
        time_remaining: 300,
        now,
    };

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    // Book is stale, so: hard crash (needs fresh book) = no,
    // dual trigger = no, trailing = no, post-entry = no
    assert!(trigger.is_none(), "All triggers should be suppressed with stale orderbook");
}

#[test]
fn evaluate_triggers_no_external_price_suppresses_hard_and_dual() {
    // No external price means hard crash and dual trigger can't fire.
    // But trailing stop can still work (it only needs the orderbook).
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    // Setup: trailing would trigger (peak=0.96, bid=0.90, drop=0.06 > trailing distance)
    // at 900s: decay_factor=1, raw=0.05, floor=0.015, effective=0.05
    let ctx = make_trigger_ctx(
        dec!(0.90),
        dec!(0.96),       // peak (armed)
        dec!(0.90),       // drop = 0.06 >= 0.05
        dec!(90000),
        None,             // NO external price
        900,
        15,
    );

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(trigger.is_some(), "Trailing should work without external price");
    assert!(matches!(trigger.unwrap(), StopLossTriggerKind::TrailingStop { .. }));
}

// ---------------------------------------------------------------------------
// compute_exit_clip tests
// ---------------------------------------------------------------------------

#[test]
fn exit_clip_remaining_is_limit() {
    // remaining=10, bid_depth=20, cap=0.8 → depth_capped=16, clip=min(10,16)=10
    let clip = compute_exit_clip(dec!(10), dec!(20), dec!(0.8), dec!(1));
    assert_eq!(clip, dec!(10), "Remaining should be the limiting factor");
}

#[test]
fn exit_clip_depth_is_limit() {
    // remaining=10, bid_depth=5, cap=0.8 → depth_capped=4, clip=min(10,4)=4
    let clip = compute_exit_clip(dec!(10), dec!(5), dec!(0.8), dec!(1));
    assert_eq!(clip, dec!(4), "Depth * cap_factor should be the limiting factor");
}

#[test]
fn exit_clip_below_min_size_returns_zero() {
    // remaining=10, bid_depth=0.5, cap=0.8 → depth_capped=0.4, below min_size=1 → 0
    let clip = compute_exit_clip(dec!(10), dec!(0.5), dec!(0.8), dec!(1));
    assert_eq!(clip, Decimal::ZERO, "Below min_size should return zero (dust)");
}

#[test]
fn exit_clip_dust_remaining_returns_zero() {
    // remaining=0.001, bid_depth=100, cap=0.8 → capped=0.001, below min_size=1 → 0
    let clip = compute_exit_clip(dec!(0.001), dec!(100), dec!(0.8), dec!(1));
    assert_eq!(clip, Decimal::ZERO, "Dust remaining should return zero");
}
