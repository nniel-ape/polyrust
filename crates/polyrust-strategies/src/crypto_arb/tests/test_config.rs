use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use super::*;

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

#[test]
fn tailend_config_defaults() {
    use crate::crypto_arb::config::{ReferenceQualityLevel, TailEndConfig};

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
    use crate::crypto_arb::strategy::tailend::TailEndStrategy;
    use std::sync::Arc;

    let mut config = crate::crypto_arb::config::ArbitrageConfig::default();
    config.tailend.dynamic_thresholds = vec![
        (120, dec!(0.90)), // 0.90 at 120s
        (90, dec!(0.92)),  // 0.92 at 90s
        (60, dec!(0.93)),  // 0.93 at 60s
        (30, dec!(0.95)),  // 0.95 at 30s
    ];

    let base = Arc::new(crate::crypto_arb::runtime::CryptoArbRuntime::new(config, vec![]));
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
    use crate::crypto_arb::strategy::tailend::TailEndStrategy;
    use std::sync::Arc;

    let mut config = crate::crypto_arb::config::ArbitrageConfig::default();
    config.tailend.dynamic_thresholds = vec![]; // Empty - should fallback
    config.tailend.ask_threshold = dec!(0.88); // Legacy threshold

    let base = Arc::new(crate::crypto_arb::runtime::CryptoArbRuntime::new(config, vec![]));
    let strategy = TailEndStrategy::new(base);

    // Should fallback to legacy threshold when dynamic thresholds is empty
    assert_eq!(strategy.get_ask_threshold(60), dec!(0.88));
}

#[test]
fn stop_loss_config_new_field_defaults() {
    let config = crate::crypto_arb::config::StopLossConfig::default();
    assert_eq!(config.trailing_min_distance, dec!(0.015));
    assert_eq!(config.stale_market_cooldown_secs, 120);
    assert_eq!(config.min_remaining_secs, 45);
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
    let config: crate::crypto_arb::config::StopLossConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.trailing_min_distance, dec!(0.015));
    assert_eq!(config.stale_market_cooldown_secs, 120);
    assert_eq!(config.min_remaining_secs, 45);
}

// ---------------------------------------------------------------------------
// Default consistency invariant tests
// ---------------------------------------------------------------------------

#[test]
fn trailing_floor_less_than_base_distance() {
    let config = crate::crypto_arb::config::StopLossConfig::default();
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
    let config = crate::crypto_arb::config::ArbitrageConfig::default();
    assert!(
        config.tailend.post_entry_window_secs > config.tailend.min_sell_delay_secs,
        "post_entry_window_secs ({}) must be greater than min_sell_delay_secs ({}) \
         so the post-entry exit window is reachable",
        config.tailend.post_entry_window_secs,
        config.tailend.min_sell_delay_secs,
    );
}

#[test]
fn tailend_config_new_fields_default() {
    let config = crate::crypto_arb::config::TailEndConfig::default();
    assert_eq!(config.min_strike_distance_pct, dec!(0.005));
}

#[test]
fn tailend_config_deserialize_missing_strike_distance() {
    let toml_str = r#"
        enabled = true
        time_threshold_secs = 120
        ask_threshold = "0.90"
    "#;
    let config: crate::crypto_arb::config::TailEndConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.min_strike_distance_pct, dec!(0.005));
}

#[test]
fn stop_loss_config_lifecycle_field_defaults() {
    let config = crate::crypto_arb::config::StopLossConfig::default();

    // Hard crash
    assert_eq!(config.hard_drop_abs, dec!(0.08));
    assert_eq!(config.hard_reversal_pct, dec!(0.006));

    // Freshness gating
    assert_eq!(config.sl_max_book_age_ms, 1200);
    assert_eq!(config.sl_max_external_age_ms, 1500);
    assert_eq!(config.sl_min_sources, 2);
    assert_eq!(config.sl_max_dispersion_bps, dec!(50));

    // Hysteresis
    assert_eq!(config.dual_trigger_consecutive_ticks, 2);

    // Trailing arming
    assert_eq!(config.trailing_arm_distance, dec!(0.015));

    // Execution ladder
    assert_eq!(config.exit_depth_cap_factor, dec!(0.80));

    // Recovery
    assert!(config.recovery_enabled);
    assert_eq!(config.recovery_max_set_cost, dec!(1.01));
    assert_eq!(config.recovery_max_extra_frac, dec!(0.15));
}

#[test]
fn stop_loss_config_lifecycle_defaults_are_sane() {
    let config = crate::crypto_arb::config::StopLossConfig::default();

    // All numeric values should be positive where required
    assert!(
        config.hard_drop_abs > Decimal::ZERO,
        "hard_drop_abs must be positive"
    );
    assert!(
        config.hard_reversal_pct > Decimal::ZERO,
        "hard_reversal_pct must be positive"
    );
    assert!(
        config.sl_max_book_age_ms > 0,
        "sl_max_book_age_ms must be positive"
    );
    assert!(
        config.sl_max_external_age_ms > 0,
        "sl_max_external_age_ms must be positive"
    );
    assert!(config.sl_min_sources > 0, "sl_min_sources must be positive");
    assert!(
        config.sl_max_dispersion_bps > Decimal::ZERO,
        "sl_max_dispersion_bps must be positive"
    );
    assert!(
        config.dual_trigger_consecutive_ticks > 0,
        "dual_trigger_consecutive_ticks must be positive"
    );
    assert!(
        config.trailing_arm_distance > Decimal::ZERO,
        "trailing_arm_distance must be positive"
    );
    assert!(
        config.exit_depth_cap_factor > Decimal::ZERO
            && config.exit_depth_cap_factor <= Decimal::ONE,
        "exit_depth_cap_factor must be in (0, 1]"
    );
    assert!(
        config.recovery_max_set_cost > Decimal::ZERO,
        "recovery_max_set_cost must be positive"
    );
    assert!(
        config.recovery_max_extra_frac > Decimal::ZERO
            && config.recovery_max_extra_frac < Decimal::ONE,
        "recovery_max_extra_frac must be in (0, 1)"
    );
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
        sl_max_book_age_ms = 1000
        sl_max_external_age_ms = 2000
        sl_min_sources = 3
        sl_max_dispersion_bps = "75"
        dual_trigger_consecutive_ticks = 3
        trailing_arm_distance = "0.020"
        exit_depth_cap_factor = "0.70"
        recovery_enabled = false
        recovery_max_set_cost = "1.02"
        recovery_max_extra_frac = "0.20"
    "#;
    let config: crate::crypto_arb::config::StopLossConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.hard_drop_abs, dec!(0.10));
    assert_eq!(config.hard_reversal_pct, dec!(0.008));
    assert_eq!(config.sl_max_book_age_ms, 1000);
    assert_eq!(config.sl_max_external_age_ms, 2000);
    assert_eq!(config.sl_min_sources, 3);
    assert_eq!(config.sl_max_dispersion_bps, dec!(75));
    assert_eq!(config.dual_trigger_consecutive_ticks, 3);
    assert_eq!(config.trailing_arm_distance, dec!(0.020));
    assert_eq!(config.exit_depth_cap_factor, dec!(0.70));
    assert!(!config.recovery_enabled);
    assert_eq!(config.recovery_max_set_cost, dec!(1.02));
    assert_eq!(config.recovery_max_extra_frac, dec!(0.20));
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
    let config: crate::crypto_arb::config::StopLossConfig = toml::from_str(toml_str).unwrap();
    // All new lifecycle fields should have their defaults
    assert_eq!(config.hard_drop_abs, dec!(0.08));
    assert_eq!(config.hard_reversal_pct, dec!(0.006));
    assert_eq!(config.sl_max_book_age_ms, 1200);
    assert_eq!(config.sl_max_external_age_ms, 1500);
    assert_eq!(config.sl_min_sources, 2);
    assert_eq!(config.sl_max_dispersion_bps, dec!(50));
    assert_eq!(config.dual_trigger_consecutive_ticks, 2);
    assert_eq!(config.trailing_arm_distance, dec!(0.015));
    assert_eq!(config.exit_depth_cap_factor, dec!(0.80));
    assert!(config.recovery_enabled);
    assert_eq!(config.recovery_max_set_cost, dec!(1.01));
    assert_eq!(config.recovery_max_extra_frac, dec!(0.15));
}

/// Backward compatibility: old configs with removed params still parse.
/// These params were removed in fast-exit-v2: short_limit_refresh_secs,
/// short_limit_tick_offset, max_exit_retries, reentry_confirm_ticks, reentry_cooldown_secs.
#[test]
fn stop_loss_config_old_config_with_removed_params_still_parses() {
    let toml_str = r#"
        reversal_pct = "0.003"
        min_drop = "0.05"
        trailing_enabled = true
        trailing_distance = "0.05"
        time_decay = true
        hard_drop_abs = "0.08"
        hard_reversal_pct = "0.006"
        short_limit_refresh_secs = 2
        short_limit_tick_offset = 1
        max_exit_retries = 5
        reentry_confirm_ticks = 2
        reentry_cooldown_secs = 8
        exit_depth_cap_factor = "0.80"
        recovery_enabled = true
        recovery_max_set_cost = "1.01"
        recovery_max_extra_frac = "0.15"
    "#;
    let config: crate::crypto_arb::config::StopLossConfig = toml::from_str(toml_str).unwrap();
    // Verify known fields parsed correctly
    assert_eq!(config.hard_drop_abs, dec!(0.08));
    assert_eq!(config.exit_depth_cap_factor, dec!(0.80));
    assert!(config.recovery_enabled);
    assert_eq!(config.recovery_max_set_cost, dec!(1.01));
    assert_eq!(config.recovery_max_extra_frac, dec!(0.15));
    // Removed fields are silently ignored — no parse error
}

// ---------------------------------------------------------------------------
// Config validation tests
// ---------------------------------------------------------------------------

#[test]
fn stop_loss_validate_trailing_min_greater_than_distance_errors() {
    let mut config = crate::crypto_arb::config::StopLossConfig::default();
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
fn sizing_validate_depth_cap_factor_bounds() {
    // Zero is invalid
    let mut config = crate::crypto_arb::config::SizingConfig::default();
    config.depth_cap_factor = Decimal::ZERO;
    assert!(config.validate().is_err());

    // Negative is invalid
    let mut config = crate::crypto_arb::config::SizingConfig::default();
    config.depth_cap_factor = dec!(-0.5);
    assert!(config.validate().is_err());

    // > 1 is invalid
    let mut config = crate::crypto_arb::config::SizingConfig::default();
    config.depth_cap_factor = dec!(1.5);
    assert!(config.validate().is_err());

    // Exactly 1.0 is valid
    let mut config = crate::crypto_arb::config::SizingConfig::default();
    config.depth_cap_factor = Decimal::ONE;
    assert!(config.validate().is_ok());

    // Default 0.50 is valid
    let config = crate::crypto_arb::config::SizingConfig::default();
    assert!(config.validate().is_ok());
}

#[test]
fn stop_loss_validate_exit_depth_cap_factor_bounds() {
    // Zero is invalid
    let mut config = crate::crypto_arb::config::StopLossConfig::default();
    config.exit_depth_cap_factor = Decimal::ZERO;
    assert!(config.validate().is_err());

    // Negative is invalid
    let mut config = crate::crypto_arb::config::StopLossConfig::default();
    config.exit_depth_cap_factor = dec!(-0.5);
    assert!(config.validate().is_err());

    // > 1 is invalid
    let mut config = crate::crypto_arb::config::StopLossConfig::default();
    config.exit_depth_cap_factor = dec!(1.5);
    assert!(config.validate().is_err());

    // Exactly 1.0 is valid
    let mut config = crate::crypto_arb::config::StopLossConfig::default();
    config.exit_depth_cap_factor = Decimal::ONE;
    assert!(config.validate().is_ok());
}

#[test]
fn stop_loss_validate_valid_config_passes() {
    let config = crate::crypto_arb::config::StopLossConfig::default();
    assert!(
        config.validate().is_ok(),
        "Default StopLossConfig should pass validation"
    );
}

#[test]
fn arb_config_validate_post_entry_lte_sell_delay_errors() {
    let mut config = crate::crypto_arb::config::ArbitrageConfig::default();
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
    let mut config = crate::crypto_arb::config::ArbitrageConfig::default();
    config.tailend.post_entry_window_secs = 5;
    config.tailend.min_sell_delay_secs = 10;
    assert!(config.validate().is_err());
}

#[test]
fn arb_config_validate_valid_config_passes() {
    let config = crate::crypto_arb::config::ArbitrageConfig::default();
    assert!(
        config.validate().is_ok(),
        "Default ArbitrageConfig should pass validation"
    );
}

#[test]
fn arb_config_validate_dead_zone_warning() {
    // This test verifies the dead zone check doesn't return an error
    // (it only warns), even with a large dead zone.
    // Dead zone = min_strike_distance_pct - reversal_pct: when large, entries
    // near strike distance can trigger stop-loss reversal immediately.
    let mut config = crate::crypto_arb::config::ArbitrageConfig::default();
    config.stop_loss.reversal_pct = dec!(0.001); // 0.1%
    config.tailend.min_strike_distance_pct = dec!(0.010); // 1%
    // Dead zone = 0.009 > 0.003 — triggers warning but not error
    assert!(
        config.validate().is_ok(),
        "Dead zone should warn but not error"
    );
}
