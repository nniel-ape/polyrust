use std::collections::HashSet;

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use super::config::DutchBookConfig;
use super::scanner::{GammaMarketResponse, GammaScanner};
use super::types::{ExecutionState, FilledSide, MarketEntry, PairedPosition};

// ---------------------------------------------------------------------------
// Config tests
// ---------------------------------------------------------------------------

#[test]
fn config_default_is_valid() {
    let config = DutchBookConfig::default();
    assert!(config.validate().is_ok());
    assert!(!config.enabled);
    assert_eq!(config.max_combined_cost, dec!(0.99));
    assert_eq!(config.min_profit_threshold, dec!(0.005));
    assert_eq!(config.max_position_size, dec!(100));
    assert_eq!(config.min_liquidity_usd, dec!(10000));
    assert_eq!(config.max_days_until_resolution, 7);
    assert_eq!(config.scan_interval_secs, 600);
    assert_eq!(config.max_concurrent_positions, 10);
    assert_eq!(config.unwind_discount, dec!(0.03));
    assert_eq!(config.unwind_settle_secs, 5);
}

#[test]
fn config_max_combined_cost_must_be_in_open_unit_interval() {
    let mut config = DutchBookConfig::default();

    config.max_combined_cost = Decimal::ZERO;
    assert!(config.validate().is_err());

    config.max_combined_cost = Decimal::ONE;
    assert!(config.validate().is_err());

    config.max_combined_cost = dec!(-0.5);
    assert!(config.validate().is_err());

    config.max_combined_cost = dec!(1.01);
    assert!(config.validate().is_err());

    config.max_combined_cost = dec!(0.97);
    assert!(config.validate().is_ok());
}

#[test]
fn config_min_profit_threshold_must_be_positive() {
    let mut config = DutchBookConfig::default();

    config.min_profit_threshold = Decimal::ZERO;
    assert!(config.validate().is_err());

    config.min_profit_threshold = dec!(-0.01);
    assert!(config.validate().is_err());

    config.min_profit_threshold = dec!(0.001);
    assert!(config.validate().is_ok());
}

#[test]
fn config_max_position_size_must_be_positive() {
    let mut config = DutchBookConfig::default();

    config.max_position_size = Decimal::ZERO;
    assert!(config.validate().is_err());

    config.max_position_size = dec!(-10);
    assert!(config.validate().is_err());

    config.max_position_size = dec!(50);
    assert!(config.validate().is_ok());
}

#[test]
fn config_min_liquidity_must_be_non_negative() {
    let mut config = DutchBookConfig::default();

    config.min_liquidity_usd = dec!(-1);
    assert!(config.validate().is_err());

    config.min_liquidity_usd = Decimal::ZERO;
    assert!(config.validate().is_ok());
}

#[test]
fn config_zero_max_days_is_invalid() {
    let mut config = DutchBookConfig::default();
    config.max_days_until_resolution = 0;
    assert!(config.validate().is_err());
}

#[test]
fn config_zero_scan_interval_is_invalid() {
    let mut config = DutchBookConfig::default();
    config.scan_interval_secs = 0;
    assert!(config.validate().is_err());
}

#[test]
fn config_zero_max_concurrent_positions_is_invalid() {
    let mut config = DutchBookConfig::default();
    config.max_concurrent_positions = 0;
    assert!(config.validate().is_err());
}

#[test]
fn config_unwind_discount_must_be_in_open_unit_interval() {
    let mut config = DutchBookConfig::default();

    config.unwind_discount = Decimal::ZERO;
    assert!(config.validate().is_err());

    config.unwind_discount = Decimal::ONE;
    assert!(config.validate().is_err());

    config.unwind_discount = dec!(0.05);
    assert!(config.validate().is_ok());
}

#[test]
fn config_deserializes_from_toml() {
    let toml_str = r#"
        enabled = true
        max_combined_cost = 0.98
        min_profit_threshold = 0.01
        max_position_size = 200
        min_liquidity_usd = 5000
        max_days_until_resolution = 14
        scan_interval_secs = 300
        max_concurrent_positions = 5
        unwind_discount = 0.05
        unwind_settle_secs = 10
    "#;

    let config: DutchBookConfig = toml::from_str(toml_str).unwrap();
    assert!(config.enabled);
    assert_eq!(config.max_combined_cost, dec!(0.98));
    assert_eq!(config.min_profit_threshold, dec!(0.01));
    assert_eq!(config.max_position_size, dec!(200));
    assert_eq!(config.min_liquidity_usd, dec!(5000));
    assert_eq!(config.max_days_until_resolution, 14);
    assert_eq!(config.scan_interval_secs, 300);
    assert_eq!(config.max_concurrent_positions, 5);
    assert_eq!(config.unwind_discount, dec!(0.05));
    assert_eq!(config.unwind_settle_secs, 10);
    assert!(config.validate().is_ok());
}

#[test]
fn config_deserializes_partial_toml_with_defaults() {
    let toml_str = r#"
        enabled = true
    "#;

    let config: DutchBookConfig = toml::from_str(toml_str).unwrap();
    assert!(config.enabled);
    // Everything else should be default
    assert_eq!(config.max_combined_cost, dec!(0.99));
    assert_eq!(config.max_concurrent_positions, 10);
    assert!(config.validate().is_ok());
}

// ---------------------------------------------------------------------------
// ExecutionState tests
// ---------------------------------------------------------------------------

#[test]
fn execution_state_new_starts_awaiting() {
    let state = ExecutionState::new();
    assert_eq!(
        state,
        ExecutionState::AwaitingFills {
            yes_filled: false,
            no_filled: false
        }
    );
    assert!(!state.is_terminal());
    assert!(!state.needs_unwind());
}

#[test]
fn execution_state_both_fill_yes_first() {
    let state = ExecutionState::new();
    let state = state.fill_yes();
    assert_eq!(
        state,
        ExecutionState::AwaitingFills {
            yes_filled: true,
            no_filled: false
        }
    );
    assert!(!state.is_terminal());

    let state = state.fill_no();
    assert_eq!(state, ExecutionState::BothFilled);
    assert!(state.is_terminal());
}

#[test]
fn execution_state_both_fill_no_first() {
    let state = ExecutionState::new();
    let state = state.fill_no();
    let state = state.fill_yes();
    assert_eq!(state, ExecutionState::BothFilled);
    assert!(state.is_terminal());
}

#[test]
fn execution_state_partial_fill_yes_then_cancel_no() {
    let state = ExecutionState::new();
    let state = state.fill_yes();
    let state = state.cancel_no("yes_order_123".to_string());
    assert_eq!(
        state,
        ExecutionState::PartialFill {
            filled_side: FilledSide::Yes,
            filled_order_id: "yes_order_123".to_string()
        }
    );
    assert!(state.needs_unwind());
    assert!(!state.is_terminal());
}

#[test]
fn execution_state_partial_fill_no_then_cancel_yes() {
    let state = ExecutionState::new();
    let state = state.fill_no();
    let state = state.cancel_yes("no_order_456".to_string());
    assert_eq!(
        state,
        ExecutionState::PartialFill {
            filled_side: FilledSide::No,
            filled_order_id: "no_order_456".to_string()
        }
    );
    assert!(state.needs_unwind());
}

#[test]
fn execution_state_both_cancelled_is_complete() {
    let state = ExecutionState::new();
    // Cancel YES first when NO hasn't filled either
    let state = state.cancel_yes("no_order".to_string());
    assert_eq!(state, ExecutionState::Complete);
    assert!(state.is_terminal());
}

#[test]
fn execution_state_both_cancelled_no_first() {
    let state = ExecutionState::new();
    let state = state.cancel_no("yes_order".to_string());
    assert_eq!(state, ExecutionState::Complete);
    assert!(state.is_terminal());
}

#[test]
fn execution_state_unwind_lifecycle() {
    let state = ExecutionState::new();
    let state = state.fill_yes();
    let state = state.cancel_no("yes_order".to_string());
    assert!(state.needs_unwind());

    let state = state.start_unwind("sell_order_789".to_string());
    assert_eq!(
        state,
        ExecutionState::Unwinding {
            sell_order_id: "sell_order_789".to_string()
        }
    );
    assert!(!state.needs_unwind());
    assert!(!state.is_terminal());

    let state = state.complete_unwind();
    assert_eq!(state, ExecutionState::Complete);
    assert!(state.is_terminal());
}

#[test]
fn execution_state_fill_on_terminal_is_noop() {
    let state = ExecutionState::BothFilled;
    let state = state.fill_yes();
    assert_eq!(state, ExecutionState::BothFilled);

    let state = ExecutionState::Complete;
    let state = state.fill_no();
    assert_eq!(state, ExecutionState::Complete);
}

#[test]
fn execution_state_start_unwind_only_from_partial_fill() {
    let state = ExecutionState::new();
    let state = state.start_unwind("sell_order".to_string());
    // Should be a no-op — can't unwind from AwaitingFills
    assert_eq!(
        state,
        ExecutionState::AwaitingFills {
            yes_filled: false,
            no_filled: false
        }
    );
}

// ---------------------------------------------------------------------------
// PairedPosition tests
// ---------------------------------------------------------------------------

#[test]
fn paired_position_profit_calculation() {
    let now = Utc::now();
    let pos = PairedPosition {
        market_id: "market_1".to_string(),
        yes_entry_price: dec!(0.48),
        no_entry_price: dec!(0.49),
        size: dec!(100),
        combined_cost: dec!(97),  // (0.48 + 0.49) * 100
        expected_profit: dec!(3), // 100 - 97 = 3 USDC
        opened_at: now,
    };

    assert_eq!(pos.combined_cost, dec!(97));
    assert_eq!(pos.expected_profit, dec!(3));
    assert_eq!(
        pos.yes_entry_price + pos.no_entry_price,
        dec!(0.97)
    );
}

// ---------------------------------------------------------------------------
// MarketEntry tests
// ---------------------------------------------------------------------------

#[test]
fn market_entry_construction() {
    let end_date = Utc::now() + Duration::days(3);
    let entry = MarketEntry {
        market_id: "cond_123".to_string(),
        token_a: "token_yes".to_string(),
        token_b: "token_no".to_string(),
        neg_risk: false,
        end_date,
        liquidity: dec!(50000),
    };

    assert_eq!(entry.market_id, "cond_123");
    assert_eq!(entry.token_a, "token_yes");
    assert_eq!(entry.token_b, "token_no");
    assert!(!entry.neg_risk);
    assert_eq!(entry.liquidity, dec!(50000));
}

// ---------------------------------------------------------------------------
// Scanner tests — convert_and_filter
// ---------------------------------------------------------------------------

/// Helper to build a valid GammaMarketResponse for testing.
fn make_gamma_response(end_date_offset_days: i64, liquidity: &str) -> GammaMarketResponse {
    let end_date = Utc::now() + Duration::days(end_date_offset_days);
    GammaMarketResponse {
        condition_id: Some("cond_001".to_string()),
        slug: Some("will-btc-go-up".to_string()),
        question: Some("Will BTC go up?".to_string()),
        start_date: Some(Utc::now().to_rfc3339()),
        end_date: Some(end_date.to_rfc3339()),
        clob_token_ids: Some("[\"token_yes\", \"token_no\"]".to_string()),
        neg_risk: Some(false),
        accepting_orders: Some(true),
        liquidity: Some(liquidity.to_string()),
        order_min_size: Some(5.0),
        order_price_min_tick_size: Some(0.01),
        maker_base_fee: Some(0.0),
    }
}

#[test]
fn scanner_accepts_valid_market() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let raw = make_gamma_response(3, "50000");
    let result = scanner.convert_and_filter(raw, now, max_end);
    assert!(result.is_some());

    let info = result.unwrap();
    assert_eq!(info.id, "cond_001");
    assert_eq!(info.slug, "will-btc-go-up");
    assert_eq!(info.question, "Will BTC go up?");
    assert_eq!(info.token_ids.outcome_a, "token_yes");
    assert_eq!(info.token_ids.outcome_b, "token_no");
    assert!(info.accepting_orders);
    assert!(!info.neg_risk);
}

#[test]
fn scanner_rejects_not_accepting_orders() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let mut raw = make_gamma_response(3, "50000");
    raw.accepting_orders = Some(false);
    assert!(scanner.convert_and_filter(raw, now, max_end).is_none());
}

#[test]
fn scanner_rejects_missing_condition_id() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let mut raw = make_gamma_response(3, "50000");
    raw.condition_id = None;
    assert!(scanner.convert_and_filter(raw, now, max_end).is_none());
}

#[test]
fn scanner_rejects_expired_market() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    // End date in the past
    let mut raw = make_gamma_response(3, "50000");
    raw.end_date = Some((now - Duration::hours(1)).to_rfc3339());
    assert!(scanner.convert_and_filter(raw, now, max_end).is_none());
}

#[test]
fn scanner_rejects_market_too_far_out() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    // End date beyond max_days_until_resolution
    let mut raw = make_gamma_response(3, "50000");
    raw.end_date = Some((now + Duration::days(10)).to_rfc3339());
    assert!(scanner.convert_and_filter(raw, now, max_end).is_none());
}

#[test]
fn scanner_rejects_low_liquidity() {
    let config = DutchBookConfig::default(); // min_liquidity_usd = 10000
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let raw = make_gamma_response(3, "5000"); // Below threshold
    assert!(scanner.convert_and_filter(raw, now, max_end).is_none());
}

#[test]
fn scanner_accepts_exact_liquidity_threshold() {
    let config = DutchBookConfig::default(); // min_liquidity_usd = 10000
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let raw = make_gamma_response(3, "10000"); // Exactly at threshold
    assert!(scanner.convert_and_filter(raw, now, max_end).is_some());
}

#[test]
fn scanner_rejects_missing_clob_token_ids() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let mut raw = make_gamma_response(3, "50000");
    raw.clob_token_ids = None;
    assert!(scanner.convert_and_filter(raw, now, max_end).is_none());
}

#[test]
fn scanner_rejects_single_token_market() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let mut raw = make_gamma_response(3, "50000");
    raw.clob_token_ids = Some("[\"only_one\"]".to_string());
    assert!(scanner.convert_and_filter(raw, now, max_end).is_none());
}

#[test]
fn scanner_rejects_three_token_market() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let mut raw = make_gamma_response(3, "50000");
    raw.clob_token_ids = Some("[\"a\", \"b\", \"c\"]".to_string());
    assert!(scanner.convert_and_filter(raw, now, max_end).is_none());
}

#[test]
fn scanner_rejects_missing_end_date() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let mut raw = make_gamma_response(3, "50000");
    raw.end_date = None;
    assert!(scanner.convert_and_filter(raw, now, max_end).is_none());
}

#[test]
fn scanner_handles_missing_optional_fields() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let mut raw = make_gamma_response(3, "50000");
    raw.start_date = None;
    raw.order_min_size = None;
    raw.order_price_min_tick_size = None;
    raw.maker_base_fee = None;
    raw.neg_risk = None;

    let result = scanner.convert_and_filter(raw, now, max_end);
    assert!(result.is_some());

    let info = result.unwrap();
    assert!(info.start_date.is_none());
    assert_eq!(info.min_order_size, dec!(5)); // default
    assert_eq!(info.tick_size, dec!(0.01)); // default
    assert_eq!(info.fee_rate_bps, 0); // default
    assert!(!info.neg_risk); // default
}

#[test]
fn scanner_respects_custom_liquidity_threshold() {
    let mut config = DutchBookConfig::default();
    config.min_liquidity_usd = dec!(1000); // Lower threshold
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let raw = make_gamma_response(3, "5000");
    assert!(scanner.convert_and_filter(raw, now, max_end).is_some());
}

#[test]
fn scanner_handles_missing_liquidity_as_zero() {
    let config = DutchBookConfig::default(); // min_liquidity_usd = 10000
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let mut raw = make_gamma_response(3, "50000");
    raw.liquidity = None; // Missing liquidity treated as 0
    assert!(scanner.convert_and_filter(raw, now, max_end).is_none());
}

#[test]
fn scanner_handles_zero_liquidity_threshold() {
    let mut config = DutchBookConfig::default();
    config.min_liquidity_usd = Decimal::ZERO;
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    // Even with missing liquidity (treated as 0), should pass with 0 threshold
    let mut raw = make_gamma_response(3, "50000");
    raw.liquidity = None;
    assert!(scanner.convert_and_filter(raw, now, max_end).is_some());
}

#[test]
fn scanner_deduplication_skips_known_markets() {
    // This tests the deduplication logic used in scan_markets().
    // We test it directly on the filtering logic since scan_markets
    // requires HTTP, but the deduplication is a simple set check.
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let raw = make_gamma_response(3, "50000");
    let info = scanner.convert_and_filter(raw, now, max_end).unwrap();

    let mut known = HashSet::new();
    known.insert(info.id.clone());

    // A second market with the same ID should be filtered out
    let raw2 = make_gamma_response(3, "50000");
    let info2 = scanner.convert_and_filter(raw2, now, max_end).unwrap();
    assert!(known.contains(&info2.id));

    // A market with a different ID should pass
    let mut raw3 = make_gamma_response(3, "50000");
    raw3.condition_id = Some("cond_002".to_string());
    let info3 = scanner.convert_and_filter(raw3, now, max_end).unwrap();
    assert!(!known.contains(&info3.id));
}

#[test]
fn scanner_neg_risk_parsed_correctly() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config).unwrap();
    let now = Utc::now();
    let max_end = now + Duration::days(7);

    let mut raw = make_gamma_response(3, "50000");
    raw.neg_risk = Some(true);
    let info = scanner.convert_and_filter(raw, now, max_end).unwrap();
    assert!(info.neg_risk);
}

#[test]
fn scanner_creation_succeeds() {
    let config = DutchBookConfig::default();
    let scanner = GammaScanner::new(config);
    assert!(scanner.is_ok());
}
