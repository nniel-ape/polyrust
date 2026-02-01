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
    kelly_position_size, net_profit_margin, parse_slug_timestamp, taker_fee, CryptoArbBase,
};
use super::config::{ArbitrageConfig, SizingConfig};
use super::types::{
    ArbitrageMode, ArbitragePosition, BoundarySnapshot, MarketWithReference, ModeStats,
    ReferenceQuality,
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
    assert_eq!(ReferenceQuality::Historical(10).quality_factor(), dec!(0.85));
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
// ArbitrageMode tests
// ---------------------------------------------------------------------------

#[test]
fn arbitrage_mode_canonical() {
    let mode = ArbitrageMode::CrossCorrelated {
        leader: "BTC".to_string(),
    };
    let canonical = mode.canonical();
    assert_eq!(
        canonical,
        ArbitrageMode::CrossCorrelated {
            leader: String::new()
        }
    );
}

#[test]
fn arbitrage_mode_display() {
    assert_eq!(ArbitrageMode::TailEnd.to_string(), "TailEnd");
    assert_eq!(ArbitrageMode::TwoSided.to_string(), "TwoSided");
    assert_eq!(ArbitrageMode::Confirmed.to_string(), "Confirmed");
    assert_eq!(
        ArbitrageMode::CrossCorrelated {
            leader: "BTC".to_string()
        }
        .to_string(),
        "Cross(BTC)"
    );
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
    assert!(config.order.hybrid_mode);
    assert_eq!(config.order.limit_offset, dec!(0.01));
    assert_eq!(config.order.max_age_secs, 30);

    // Sizing defaults
    assert_eq!(config.sizing.base_size, dec!(10));
    assert_eq!(config.sizing.kelly_multiplier, dec!(0.25));
    assert_eq!(config.sizing.min_size, dec!(2));
    assert_eq!(config.sizing.max_size, dec!(25));
    assert!(config.sizing.use_kelly);

    // StopLoss defaults
    assert_eq!(config.stop_loss.reversal_pct, dec!(0.005));
    assert_eq!(config.stop_loss.min_drop, dec!(0.05));
    assert!(config.stop_loss.trailing_enabled);
    assert_eq!(config.stop_loss.trailing_distance, dec!(0.03));
    assert!(config.stop_loss.time_decay);

    // Correlation defaults
    assert!(!config.correlation.enabled);
    assert_eq!(config.correlation.min_spike_pct, dec!(0.01));
    assert_eq!(config.correlation.pairs.len(), 2);
    assert_eq!(config.correlation.discount_factor, dec!(0.7));

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
    assert!(config.order.hybrid_mode);
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
    let _ = base.record_price("BTC", dec!(50000), "binance").await;

    // Small move - no spike
    let (spike, _) = base.record_price("BTC", dec!(50100), "binance").await;
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

    let spike = base.detect_spike("TEST", dec!(50500)).await;
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
                mode: ArbitrageMode::Confirmed,
                estimated_fee: Decimal::ZERO,
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

#[tokio::test]
async fn base_is_mode_disabled() {
    let mut config = ArbitrageConfig::default();
    config.performance.auto_disable = true;
    config.performance.min_trades = 3;
    config.performance.min_win_rate = dec!(0.50);
    let base = Arc::new(CryptoArbBase::new(config, vec![]));

    // Initially not disabled
    assert!(!base.is_mode_disabled(&ArbitrageMode::Confirmed).await);

    // Record losing trades
    base.record_trade_pnl(&ArbitrageMode::Confirmed, dec!(-1.0))
        .await;
    base.record_trade_pnl(&ArbitrageMode::Confirmed, dec!(-1.0))
        .await;
    base.record_trade_pnl(&ArbitrageMode::Confirmed, dec!(-1.0))
        .await;

    // Now should be disabled (0% win rate after 3 trades)
    assert!(base.is_mode_disabled(&ArbitrageMode::Confirmed).await);
}
