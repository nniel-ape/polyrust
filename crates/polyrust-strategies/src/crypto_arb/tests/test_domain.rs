use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use polyrust_core::prelude::*;

use super::*;
use crate::crypto_arb::domain::ModeStats;

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
// ReferenceQuality threshold tests
// ---------------------------------------------------------------------------

#[test]
fn reference_quality_meets_threshold() {
    use crate::crypto_arb::config::ReferenceQualityLevel;

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
