use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tokio::sync::RwLock;

use polyrust_core::prelude::*;

use super::analyzer::ArbitrageAnalyzer;
use super::config::DutchBookConfig;
use super::dashboard::DutchBookDashboard;
use super::scanner::{GammaMarketResponse, GammaScanner};
use super::strategy::DutchBookStrategy;
use super::types::{
    ArbitrageOpportunity, DutchBookState, ExecutionState, FilledSide, MarketEntry,
    PairedOrder, PairedPosition,
};

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
    assert!(!state.needs_unwind());
}

#[test]
fn execution_state_both_fill_yes_first() {
    let state = ExecutionState::new();
    let state = state.fill_yes("yes_order".to_string());
    assert_eq!(
        state,
        ExecutionState::AwaitingFills {
            yes_filled: true,
            no_filled: false
        }
    );

    let state = state.fill_no("no_order".to_string());
    assert_eq!(state, ExecutionState::BothFilled);
}

#[test]
fn execution_state_both_fill_no_first() {
    let state = ExecutionState::new();
    let state = state.fill_no("no_order".to_string());
    let state = state.fill_yes("yes_order".to_string());
    assert_eq!(state, ExecutionState::BothFilled);
}

#[test]
fn execution_state_partial_fill_yes_then_cancel_no() {
    let state = ExecutionState::new();
    let state = state.fill_yes("yes_order_123".to_string());
    let state = state.cancel_no("yes_order_123".to_string());
    assert_eq!(
        state,
        ExecutionState::PartialFill {
            filled_side: FilledSide::Yes,
            filled_order_id: "yes_order_123".to_string()
        }
    );
    assert!(state.needs_unwind());
}

#[test]
fn execution_state_partial_fill_no_then_cancel_yes() {
    let state = ExecutionState::new();
    let state = state.fill_no("no_order_456".to_string());
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
    // Cancel YES first → OneCancelled, then cancel NO → Complete
    let state = state.cancel_yes("no_order".to_string());
    assert_eq!(
        state,
        ExecutionState::OneCancelled {
            cancelled_side: FilledSide::Yes,
        }
    );
    let state = state.cancel_no("yes_order".to_string());
    assert_eq!(state, ExecutionState::Complete);
}

#[test]
fn execution_state_both_cancelled_no_first() {
    let state = ExecutionState::new();
    // Cancel NO first → OneCancelled, then cancel YES → Complete
    let state = state.cancel_no("yes_order".to_string());
    assert_eq!(
        state,
        ExecutionState::OneCancelled {
            cancelled_side: FilledSide::No,
        }
    );
    let state = state.cancel_yes("no_order".to_string());
    assert_eq!(state, ExecutionState::Complete);
}

#[test]
fn execution_state_cancel_then_fill_triggers_partial() {
    // Race condition test: YES cancelled first, then NO fills → PartialFill
    let state = ExecutionState::new();
    let state = state.cancel_yes("no_order".to_string());
    assert_eq!(
        state,
        ExecutionState::OneCancelled {
            cancelled_side: FilledSide::Yes,
        }
    );
    let state = state.fill_no("no_order_123".to_string());
    assert_eq!(
        state,
        ExecutionState::PartialFill {
            filled_side: FilledSide::No,
            filled_order_id: "no_order_123".to_string(),
        }
    );
    assert!(state.needs_unwind());
}

#[test]
fn execution_state_cancel_no_then_fill_yes_triggers_partial() {
    // Race condition test: NO cancelled first, then YES fills → PartialFill
    let state = ExecutionState::new();
    let state = state.cancel_no("yes_order".to_string());
    assert_eq!(
        state,
        ExecutionState::OneCancelled {
            cancelled_side: FilledSide::No,
        }
    );
    let state = state.fill_yes("yes_order_123".to_string());
    assert_eq!(
        state,
        ExecutionState::PartialFill {
            filled_side: FilledSide::Yes,
            filled_order_id: "yes_order_123".to_string(),
        }
    );
    assert!(state.needs_unwind());
}

#[test]
fn execution_state_unwind_lifecycle() {
    let state = ExecutionState::new();
    let state = state.fill_yes("yes_order".to_string());
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
}

#[test]
fn execution_state_fill_on_terminal_is_noop() {
    let state = ExecutionState::BothFilled;
    let state = state.fill_yes("order".to_string());
    assert_eq!(state, ExecutionState::BothFilled);

    let state = ExecutionState::Complete;
    let state = state.fill_no("order".to_string());
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
        yes_token_id: "tok_yes".to_string(),
        no_token_id: "tok_no".to_string(),
        neg_risk: false,
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
    let entry = MarketEntry {
        market_id: "cond_123".to_string(),
        token_a: "token_yes".to_string(),
        token_b: "token_no".to_string(),
        neg_risk: false,
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
        min_order_size: dec!(5),
    };

    assert_eq!(entry.market_id, "cond_123");
    assert_eq!(entry.token_a, "token_yes");
    assert_eq!(entry.token_b, "token_no");
    assert!(!entry.neg_risk);
    assert_eq!(entry.tick_size, dec!(0.01));
    assert_eq!(entry.min_order_size, dec!(5));
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

// ---------------------------------------------------------------------------
// Analyzer tests — helpers
// ---------------------------------------------------------------------------

/// Create a MarketInfo for testing the analyzer.
fn make_market_info(id: &str, token_a: &str, token_b: &str) -> MarketInfo {
    MarketInfo {
        id: id.to_string(),
        slug: format!("{id}-slug"),
        question: format!("Market {id}?"),
        start_date: Some(Utc::now()),
        end_date: Utc::now() + Duration::days(3),
        token_ids: TokenIds {
            outcome_a: token_a.to_string(),
            outcome_b: token_b.to_string(),
        },
        accepting_orders: true,
        neg_risk: false,
        min_order_size: dec!(5),
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
    }
}

/// Create an OrderbookSnapshot with a single ask level.
fn make_orderbook(token_id: &str, ask_price: Decimal, ask_size: Decimal) -> OrderbookSnapshot {
    OrderbookSnapshot {
        token_id: token_id.to_string(),
        bids: vec![],
        asks: vec![OrderbookLevel {
            price: ask_price,
            size: ask_size,
        }],
        timestamp: Utc::now(),
    }
}

/// Create an empty OrderbookSnapshot (no asks, no bids).
fn make_empty_orderbook(token_id: &str) -> OrderbookSnapshot {
    OrderbookSnapshot {
        token_id: token_id.to_string(),
        bids: vec![],
        asks: vec![],
        timestamp: Utc::now(),
    }
}

// ---------------------------------------------------------------------------
// Analyzer tests — add/remove markets
// ---------------------------------------------------------------------------

#[test]
fn analyzer_add_market_registers_both_tokens() {
    let mut analyzer = ArbitrageAnalyzer::new();
    assert_eq!(analyzer.tracked_count(), 0);

    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    assert_eq!(analyzer.tracked_count(), 1);
    assert!(analyzer.market_for_token("tok_yes").is_some());
    assert!(analyzer.market_for_token("tok_no").is_some());
    assert_eq!(analyzer.market_for_token("tok_yes").unwrap().market_id, "m1");
    assert_eq!(analyzer.market_for_token("tok_no").unwrap().market_id, "m1");
}

#[test]
fn analyzer_remove_market_cleans_up_tokens() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    analyzer.remove_market("m1");
    assert_eq!(analyzer.tracked_count(), 0);
    assert!(analyzer.market_for_token("tok_yes").is_none());
    assert!(analyzer.market_for_token("tok_no").is_none());
}

#[test]
fn analyzer_remove_nonexistent_market_is_noop() {
    let mut analyzer = ArbitrageAnalyzer::new();
    analyzer.remove_market("nonexistent");
    assert_eq!(analyzer.tracked_count(), 0);
}

#[test]
fn analyzer_add_multiple_markets() {
    let mut analyzer = ArbitrageAnalyzer::new();
    analyzer.add_market(&make_market_info("m1", "t1a", "t1b"));
    analyzer.add_market(&make_market_info("m2", "t2a", "t2b"));
    analyzer.add_market(&make_market_info("m3", "t3a", "t3b"));

    assert_eq!(analyzer.tracked_count(), 3);
    assert_eq!(analyzer.market_for_token("t2a").unwrap().market_id, "m2");
}

#[test]
fn analyzer_unknown_token_returns_none() {
    let analyzer = ArbitrageAnalyzer::new();
    assert!(analyzer.market_for_token("unknown").is_none());
}

// ---------------------------------------------------------------------------
// Analyzer tests — arbitrage detection
// ---------------------------------------------------------------------------

#[test]
fn analyzer_detects_opportunity_when_combined_ask_below_threshold() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig::default(); // max_combined_cost=0.99, min_profit=0.005

    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.48), dec!(200)));
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.49), dec!(150)));

    // combined = 0.97, profit = (1 - 0.97) / 0.97 = 3.09%
    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config);
    assert!(opp.is_some());

    let opp = opp.unwrap();
    assert_eq!(opp.market_id, "m1");
    assert_eq!(opp.yes_ask, dec!(0.48));
    assert_eq!(opp.no_ask, dec!(0.49));
    assert_eq!(opp.combined_cost, dec!(0.97));
    // max_size = min(200, 150, 100) = 100 (config limit)
    assert_eq!(opp.max_size, dec!(100));
}

#[test]
fn analyzer_profit_pct_calculation_accuracy() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig {
        max_combined_cost: dec!(0.99),
        min_profit_threshold: dec!(0.001), // very low threshold for this test
        max_position_size: dec!(1000),
        ..DutchBookConfig::default()
    };

    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.45), dec!(500)));
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.50), dec!(500)));

    // combined = 0.95, profit = (1 - 0.95) / 0.95 = 0.05/0.95 = 5.2631...%
    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config).unwrap();
    assert_eq!(opp.combined_cost, dec!(0.95));

    // Verify profit_pct = 0.05 / 0.95 with Decimal precision
    let expected_profit = dec!(0.05) / dec!(0.95);
    assert_eq!(opp.profit_pct, expected_profit);
}

#[test]
fn analyzer_no_opportunity_when_combined_at_threshold() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig::default(); // max_combined_cost = 0.99

    let mut orderbooks = HashMap::new();
    // combined = 0.99 — exactly at threshold, should be rejected (>= check)
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.50), dec!(100)));
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.49), dec!(100)));

    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config);
    assert!(opp.is_none());
}

#[test]
fn analyzer_no_opportunity_when_combined_above_threshold() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig::default(); // max_combined_cost = 0.99

    let mut orderbooks = HashMap::new();
    // combined = 1.01 — above $1, definitely no opportunity
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.52), dec!(100)));
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.49), dec!(100)));

    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config);
    assert!(opp.is_none());
}

#[test]
fn analyzer_no_opportunity_when_profit_below_threshold() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig {
        max_combined_cost: dec!(0.999), // very permissive
        min_profit_threshold: dec!(0.05), // require 5% profit
        ..DutchBookConfig::default()
    };

    let mut orderbooks = HashMap::new();
    // combined = 0.97, profit = 3.09% — below 5% threshold
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.48), dec!(100)));
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.49), dec!(100)));

    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config);
    assert!(opp.is_none());
}

#[test]
fn analyzer_no_opportunity_when_yes_side_has_no_asks() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig::default();

    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_empty_orderbook("tok_yes"));
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.49), dec!(100)));

    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config);
    assert!(opp.is_none());
}

#[test]
fn analyzer_no_opportunity_when_no_side_has_no_asks() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig::default();

    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.48), dec!(100)));
    orderbooks.insert("tok_no".to_string(), make_empty_orderbook("tok_no"));

    let opp = analyzer.check_arbitrage("tok_no", &orderbooks, &config);
    assert!(opp.is_none());
}

#[test]
fn analyzer_no_opportunity_when_both_orderbooks_empty() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig::default();

    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_empty_orderbook("tok_yes"));
    orderbooks.insert("tok_no".to_string(), make_empty_orderbook("tok_no"));

    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config);
    assert!(opp.is_none());
}

#[test]
fn analyzer_returns_none_for_unknown_token() {
    let analyzer = ArbitrageAnalyzer::new();
    let config = DutchBookConfig::default();
    let orderbooks = HashMap::new();

    let opp = analyzer.check_arbitrage("unknown_token", &orderbooks, &config);
    assert!(opp.is_none());
}

#[test]
fn analyzer_returns_none_when_only_one_orderbook_present() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig::default();

    // Only YES side has an orderbook
    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.48), dec!(100)));

    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config);
    assert!(opp.is_none());
}

// ---------------------------------------------------------------------------
// Analyzer tests — size limiting
// ---------------------------------------------------------------------------

#[test]
fn analyzer_size_limited_by_yes_liquidity() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig {
        max_position_size: dec!(1000),
        ..DutchBookConfig::default()
    };

    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.45), dec!(30))); // small YES liquidity
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.45), dec!(500)));

    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config).unwrap();
    assert_eq!(opp.max_size, dec!(30)); // limited by YES side
}

#[test]
fn analyzer_size_limited_by_no_liquidity() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig {
        max_position_size: dec!(1000),
        ..DutchBookConfig::default()
    };

    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.45), dec!(500)));
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.45), dec!(25))); // small NO liquidity

    let opp = analyzer.check_arbitrage("tok_no", &orderbooks, &config).unwrap();
    assert_eq!(opp.max_size, dec!(25)); // limited by NO side
}

#[test]
fn analyzer_size_limited_by_config_max_position_size() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig {
        max_position_size: dec!(50), // low cap
        ..DutchBookConfig::default()
    };

    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.45), dec!(500)));
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.45), dec!(500)));

    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config).unwrap();
    assert_eq!(opp.max_size, dec!(50)); // limited by config
}

#[test]
fn analyzer_triggered_from_either_token_side() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig::default();

    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.48), dec!(200)));
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.49), dec!(150)));

    // Trigger from YES side
    let opp_a = analyzer.check_arbitrage("tok_yes", &orderbooks, &config);
    assert!(opp_a.is_some());

    // Trigger from NO side — same opportunity
    let opp_b = analyzer.check_arbitrage("tok_no", &orderbooks, &config);
    assert!(opp_b.is_some());

    // Both should produce the same result
    let opp_a = opp_a.unwrap();
    let opp_b = opp_b.unwrap();
    assert_eq!(opp_a.market_id, opp_b.market_id);
    assert_eq!(opp_a.combined_cost, opp_b.combined_cost);
    assert_eq!(opp_a.profit_pct, opp_b.profit_pct);
    assert_eq!(opp_a.max_size, opp_b.max_size);
}

#[test]
fn analyzer_multiple_markets_independent() {
    let mut analyzer = ArbitrageAnalyzer::new();
    analyzer.add_market(&make_market_info("m1", "t1a", "t1b"));
    analyzer.add_market(&make_market_info("m2", "t2a", "t2b"));

    let config = DutchBookConfig::default();

    let mut orderbooks = HashMap::new();
    // m1: opportunity (combined = 0.95)
    orderbooks.insert("t1a".to_string(), make_orderbook("t1a", dec!(0.45), dec!(100)));
    orderbooks.insert("t1b".to_string(), make_orderbook("t1b", dec!(0.50), dec!(100)));
    // m2: no opportunity (combined = 1.01)
    orderbooks.insert("t2a".to_string(), make_orderbook("t2a", dec!(0.52), dec!(100)));
    orderbooks.insert("t2b".to_string(), make_orderbook("t2b", dec!(0.49), dec!(100)));

    assert!(analyzer.check_arbitrage("t1a", &orderbooks, &config).is_some());
    assert!(analyzer.check_arbitrage("t2a", &orderbooks, &config).is_none());
}

#[test]
fn analyzer_exact_profit_threshold_boundary() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    // Use the exact same calculation the analyzer uses internally:
    // combined_cost = 0.49 + 0.49 = 0.98
    // profit_pct = (1 - 0.98) / 0.98
    let combined = dec!(0.98);
    let exact_profit = (Decimal::ONE - combined) / combined;

    // Set threshold above the profit — should reject
    let config_above = DutchBookConfig {
        max_combined_cost: dec!(0.999),
        min_profit_threshold: exact_profit + dec!(0.001),
        max_position_size: dec!(100),
        ..DutchBookConfig::default()
    };

    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.49), dec!(100)));
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.49), dec!(100)));

    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config_above);
    assert!(opp.is_none(), "should reject when threshold > profit");

    // Set threshold below the profit — should accept
    let config_below = DutchBookConfig {
        min_profit_threshold: exact_profit - dec!(0.001),
        ..config_above
    };
    let opp = analyzer.check_arbitrage("tok_yes", &orderbooks, &config_below);
    assert!(opp.is_some(), "should accept when threshold < profit");
}

#[test]
fn analyzer_check_after_remove_returns_none() {
    let mut analyzer = ArbitrageAnalyzer::new();
    let market = make_market_info("m1", "tok_yes", "tok_no");
    analyzer.add_market(&market);

    let config = DutchBookConfig::default();
    let mut orderbooks = HashMap::new();
    orderbooks.insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.45), dec!(100)));
    orderbooks.insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.45), dec!(100)));

    // Should find opportunity before removal
    assert!(analyzer.check_arbitrage("tok_yes", &orderbooks, &config).is_some());

    // After removal, same token should return None
    analyzer.remove_market("m1");
    assert!(analyzer.check_arbitrage("tok_yes", &orderbooks, &config).is_none());
}

// ---------------------------------------------------------------------------
// Strategy tests — helpers
// ---------------------------------------------------------------------------

/// Create a strategy with default config and populate the StrategyContext with
/// orderbooks for two tokens of a market.
async fn setup_strategy_with_market(
    yes_ask: Decimal,
    yes_size: Decimal,
    no_ask: Decimal,
    no_size: Decimal,
) -> (DutchBookStrategy, StrategyContext, MarketInfo) {
    let config = DutchBookConfig {
        max_combined_cost: dec!(0.99),
        min_profit_threshold: dec!(0.005),
        max_position_size: dec!(100),
        max_concurrent_positions: 2,
        ..DutchBookConfig::default()
    };
    let mut strategy = DutchBookStrategy::new(config);

    let market = make_market_info("m1", "tok_yes", "tok_no");
    strategy.analyzer.add_market(&market);

    let ctx = StrategyContext::new();

    // Set sufficient balance for trading
    {
        let mut bal = ctx.balance.write().await;
        bal.available_usdc = dec!(10000);
    }

    // Populate orderbooks in the shared context
    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks.insert(
            "tok_yes".to_string(),
            make_orderbook("tok_yes", yes_ask, yes_size),
        );
        md.orderbooks.insert(
            "tok_no".to_string(),
            make_orderbook("tok_no", no_ask, no_size),
        );
    }

    (strategy, ctx, market)
}

/// Simulate Placed events for a batch order, returning order IDs assigned.
fn simulate_placed_events(
    strategy: &mut DutchBookStrategy,
    yes_token: &str,
    no_token: &str,
) -> (String, String) {
    let yes_oid = "order_yes_1".to_string();
    let no_oid = "order_no_1".to_string();

    let yes_result = OrderResult {
        success: true,
        order_id: Some(yes_oid.clone()),
        token_id: yes_token.to_string(),
        price: dec!(0.48),
        size: dec!(100),
        side: OrderSide::Buy,
        status: Some("Placed".to_string()),
        message: "OK".to_string(),
    };
    let no_result = OrderResult {
        success: true,
        order_id: Some(no_oid.clone()),
        token_id: no_token.to_string(),
        price: dec!(0.49),
        size: dec!(100),
        side: OrderSide::Buy,
        status: Some("Placed".to_string()),
        message: "OK".to_string(),
    };

    strategy.handle_order_placed(&yes_result);
    strategy.handle_order_placed(&no_result);

    (yes_oid, no_oid)
}

// ---------------------------------------------------------------------------
// Strategy tests — construction and trait basics
// ---------------------------------------------------------------------------

#[test]
fn strategy_construction() {
    let config = DutchBookConfig::default();
    let strategy = DutchBookStrategy::new(config);
    assert_eq!(strategy.name(), "dutch-book");
    assert_eq!(strategy.tracked_market_count(), 0);
    assert_eq!(strategy.open_position_count(), 0);
    assert_eq!(strategy.active_execution_count(), 0);
}

// ---------------------------------------------------------------------------
// Strategy tests — orderbook update → PlaceBatchOrder
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_emits_batch_order_on_opportunity() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    // Trigger via orderbook update
    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    let actions = strategy.on_event(&event, &ctx).await.unwrap();

    // Should emit exactly one PlaceBatchOrder with 2 orders
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        Action::PlaceBatchOrder(orders) => {
            assert_eq!(orders.len(), 2);

            // YES order
            assert_eq!(orders[0].token_id, "tok_yes");
            assert_eq!(orders[0].price, dec!(0.48));
            assert_eq!(orders[0].side, OrderSide::Buy);
            assert_eq!(orders[0].order_type, OrderType::Fok);

            // NO order
            assert_eq!(orders[1].token_id, "tok_no");
            assert_eq!(orders[1].price, dec!(0.49));
            assert_eq!(orders[1].side, OrderSide::Buy);
            assert_eq!(orders[1].order_type, OrderType::Fok);

            // Size = min(200, 150, 100) = 100 (config limit)
            assert_eq!(orders[0].size, dec!(100));
            assert_eq!(orders[1].size, dec!(100));
        }
        other => panic!("Expected PlaceBatchOrder, got {:?}", other),
    }

    // Active execution should be tracked
    assert_eq!(strategy.active_execution_count(), 1);
}

#[tokio::test]
async fn strategy_no_action_when_no_opportunity() {
    // combined = 0.52 + 0.49 = 1.01 — no opportunity
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.52), dec!(200), dec!(0.49), dec!(150)).await;

    let snapshot = make_orderbook("tok_yes", dec!(0.52), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    let actions = strategy.on_event(&event, &ctx).await.unwrap();

    assert!(actions.is_empty());
    assert_eq!(strategy.active_execution_count(), 0);
}

#[tokio::test]
async fn strategy_no_action_for_unknown_token() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    let snapshot = make_orderbook("unknown_token", dec!(0.10), dec!(500));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    let actions = strategy.on_event(&event, &ctx).await.unwrap();

    assert!(actions.is_empty());
}

// ---------------------------------------------------------------------------
// Strategy tests — position limit enforcement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_enforces_position_limit() {
    let config = DutchBookConfig {
        max_combined_cost: dec!(0.99),
        min_profit_threshold: dec!(0.005),
        max_position_size: dec!(100),
        max_concurrent_positions: 1, // Only allow 1
        ..DutchBookConfig::default()
    };
    let mut strategy = DutchBookStrategy::new(config);

    let market1 = make_market_info("m1", "t1a", "t1b");
    let market2 = make_market_info("m2", "t2a", "t2b");
    strategy.analyzer.add_market(&market1);
    strategy.analyzer.add_market(&market2);

    let ctx = StrategyContext::new();
    {
        let mut bal = ctx.balance.write().await;
        bal.available_usdc = dec!(10000);
    }
    {
        let mut md = ctx.market_data.write().await;
        // Market 1: opportunity exists
        md.orderbooks
            .insert("t1a".to_string(), make_orderbook("t1a", dec!(0.45), dec!(200)));
        md.orderbooks
            .insert("t1b".to_string(), make_orderbook("t1b", dec!(0.45), dec!(200)));
        // Market 2: opportunity also exists
        md.orderbooks
            .insert("t2a".to_string(), make_orderbook("t2a", dec!(0.44), dec!(200)));
        md.orderbooks
            .insert("t2b".to_string(), make_orderbook("t2b", dec!(0.44), dec!(200)));
    }

    // First opportunity triggers
    let snapshot1 = make_orderbook("t1a", dec!(0.45), dec!(200));
    let event1 = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot1));
    let actions1 = strategy.on_event(&event1, &ctx).await.unwrap();
    assert_eq!(actions1.len(), 1); // PlaceBatchOrder

    // Second opportunity should be blocked (limit = 1)
    let snapshot2 = make_orderbook("t2a", dec!(0.44), dec!(200));
    let event2 = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot2));
    let actions2 = strategy.on_event(&event2, &ctx).await.unwrap();
    assert!(actions2.is_empty(), "Position limit should block second opportunity");
}

#[tokio::test]
async fn strategy_skips_market_with_active_execution() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    // First orderbook update triggers
    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot.clone()));
    let actions1 = strategy.on_event(&event, &ctx).await.unwrap();
    assert_eq!(actions1.len(), 1);
    assert_eq!(strategy.active_execution_count(), 1);

    // Same market again — should skip (already executing)
    let actions2 = strategy.on_event(&event, &ctx).await.unwrap();
    assert!(actions2.is_empty());
}

// ---------------------------------------------------------------------------
// Strategy tests — paired order construction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_paired_orders_use_fok_type() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    let actions = strategy.on_event(&event, &ctx).await.unwrap();

    if let Action::PlaceBatchOrder(orders) = &actions[0] {
        for order in orders {
            assert_eq!(order.order_type, OrderType::Fok);
            assert_eq!(order.side, OrderSide::Buy);
        }
    }
}

#[tokio::test]
async fn strategy_paired_orders_respect_neg_risk() {
    let config = DutchBookConfig::default();
    let mut strategy = DutchBookStrategy::new(config);

    // Create a neg_risk market
    let mut market = make_market_info("m1", "tok_yes", "tok_no");
    market.neg_risk = true;
    strategy.analyzer.add_market(&market);

    let ctx = StrategyContext::new();
    {
        let mut bal = ctx.balance.write().await;
        bal.available_usdc = dec!(10000);
    }
    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks
            .insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.48), dec!(200)));
        md.orderbooks
            .insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.49), dec!(150)));
    }

    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    let actions = strategy.on_event(&event, &ctx).await.unwrap();

    if let Action::PlaceBatchOrder(orders) = &actions[0] {
        assert!(orders[0].neg_risk);
        assert!(orders[1].neg_risk);
    }
}

#[tokio::test]
async fn strategy_paired_order_sizes_match() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.45), dec!(60), dec!(0.45), dec!(80)).await;

    let snapshot = make_orderbook("tok_yes", dec!(0.45), dec!(60));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    let actions = strategy.on_event(&event, &ctx).await.unwrap();

    if let Action::PlaceBatchOrder(orders) = &actions[0] {
        // Both should have the same size: min(60, 80, 100) = 60
        assert_eq!(orders[0].size, dec!(60));
        assert_eq!(orders[1].size, dec!(60));
    }
}

// ---------------------------------------------------------------------------
// Strategy tests — order placed event routing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_tracks_placed_order_ids() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    // Trigger batch order
    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();

    // Simulate Placed events
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Verify order_to_market mappings exist
    assert!(strategy.order_to_market.contains_key(&yes_oid));
    assert!(strategy.order_to_market.contains_key(&no_oid));
}

#[tokio::test]
async fn strategy_ignores_failed_placement() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    // Trigger batch order
    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();

    // Failed placement
    let failed_result = OrderResult {
        success: false,
        order_id: Some("failed_oid".to_string()),
        token_id: "tok_yes".to_string(),
        price: dec!(0.48),
        size: dec!(100),
        side: OrderSide::Buy,
        status: Some("Failed".to_string()),
        message: "Rejected".to_string(),
    };
    let actions = strategy.handle_order_placed(&failed_result);
    assert!(actions.is_empty());
    assert!(!strategy.order_to_market.contains_key("failed_oid"));
}

// ---------------------------------------------------------------------------
// Strategy tests — fill events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_both_filled_promotes_to_position() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    // Trigger batch order
    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    assert_eq!(strategy.active_execution_count(), 1);

    // Simulate placed
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Fill YES
    let actions_yes = strategy.handle_order_filled(&yes_oid, "tok_yes", dec!(0.48), dec!(100)).await;
    assert!(actions_yes.is_empty()); // Not complete yet

    // Fill NO — should trigger promotion to PairedPosition
    let actions_no = strategy.handle_order_filled(&no_oid, "tok_no", dec!(0.49), dec!(100)).await;
    assert!(!actions_no.is_empty()); // Should have a Log action

    // Active execution removed, position created
    assert_eq!(strategy.active_execution_count(), 0);
    assert_eq!(strategy.open_position_count(), 1);

    // Verify position details
    let pos = strategy.open_positions.get("m1").unwrap();
    assert_eq!(pos.yes_entry_price, dec!(0.48));
    assert_eq!(pos.no_entry_price, dec!(0.49));
    assert_eq!(pos.size, dec!(100));
    // combined_cost = (0.48 + 0.49) * 100 = 97
    assert_eq!(pos.combined_cost, dec!(97));
    // expected_profit = 100 - 97 = 3
    assert_eq!(pos.expected_profit, dec!(3));
}

#[tokio::test]
async fn strategy_fill_unknown_order_is_noop() {
    let config = DutchBookConfig::default();
    let mut strategy = DutchBookStrategy::new(config);

    let actions = strategy.handle_order_filled("unknown_order", "tok_yes", dec!(0.50), dec!(10)).await;
    assert!(actions.is_empty());
}

// ---------------------------------------------------------------------------
// Strategy tests — cancellation events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_both_cancelled_cleans_up() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    // Trigger + place
    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Cancel YES first — goes to OneCancelled, still waiting for NO
    strategy.handle_order_cancelled(&yes_oid).await;
    assert_eq!(strategy.active_execution_count(), 1);

    // Cancel NO second — now both cancelled → Complete, cleaned up
    strategy.handle_order_cancelled(&no_oid).await;
    assert_eq!(strategy.active_execution_count(), 0);

    // Order mappings cleaned up
    assert!(!strategy.order_to_market.contains_key(&yes_oid));
    assert!(!strategy.order_to_market.contains_key(&no_oid));
}

#[tokio::test]
async fn strategy_partial_fill_triggers_unwind() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    // Trigger + place
    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Fill YES first
    strategy.handle_order_filled(&yes_oid, "tok_yes", dec!(0.48), dec!(100)).await;

    // Cancel NO — partial fill triggers emergency unwind
    let actions = strategy.handle_order_cancelled(&no_oid).await;

    // Should emit a PlaceOrder (GTC SELL) for the filled YES side
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        Action::PlaceOrder(order) => {
            assert_eq!(order.token_id, "tok_yes");
            assert_eq!(order.side, OrderSide::Sell);
            assert_eq!(order.order_type, OrderType::Gtc);
            // sell_price = 0.48 * (1 - 0.03) = 0.48 * 0.97 = 0.4656
            assert_eq!(order.price, dec!(0.48) * dec!(0.97));
            assert_eq!(order.size, dec!(100));
        }
        other => panic!("Expected PlaceOrder, got {:?}", other),
    }

    // Should still have active execution (in PartialFill state, awaiting unwind placement)
    assert_eq!(strategy.active_execution_count(), 1);
}

#[tokio::test]
async fn strategy_cancel_unknown_order_is_noop() {
    let config = DutchBookConfig::default();
    let mut strategy = DutchBookStrategy::new(config);

    let actions = strategy.handle_order_cancelled("unknown_order").await;
    assert!(actions.is_empty());
}

// ---------------------------------------------------------------------------
// Strategy tests — market expiration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_market_expired_removes_from_analyzer() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    assert_eq!(strategy.tracked_market_count(), 1);

    let event = Event::MarketData(MarketDataEvent::MarketExpired("m1".to_string()));
    strategy.on_event(&event, &ctx).await.unwrap();

    assert_eq!(strategy.tracked_market_count(), 0);
}

#[tokio::test]
async fn strategy_market_expired_with_position_emits_redeem() {
    let config = DutchBookConfig::default();
    let mut strategy = DutchBookStrategy::new(config);

    let market = make_market_info("m1", "tok_yes", "tok_no");
    strategy.analyzer.add_market(&market);

    // Manually insert an open position
    let pos = PairedPosition {
        market_id: "m1".to_string(),
        yes_token_id: "tok_yes".to_string(),
        no_token_id: "tok_no".to_string(),
        neg_risk: false,
        yes_entry_price: dec!(0.48),
        no_entry_price: dec!(0.49),
        size: dec!(100),
        combined_cost: dec!(97),
        expected_profit: dec!(3),
        opened_at: Utc::now(),
    };
    strategy.open_positions.insert("m1".to_string(), pos);
    assert_eq!(strategy.open_position_count(), 1);

    let ctx = StrategyContext::new();
    let event = Event::MarketData(MarketDataEvent::MarketExpired("m1".to_string()));
    let actions = strategy.on_event(&event, &ctx).await.unwrap();

    // Should emit RedeemPosition with correct token IDs and neg_risk
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        Action::RedeemPosition(req) => {
            assert_eq!(req.market_id, "m1");
            assert_eq!(req.token_ids, vec!["tok_yes", "tok_no"]);
            assert!(!req.neg_risk);
        }
        other => panic!("Expected RedeemPosition, got {:?}", other),
    }

    assert_eq!(strategy.open_position_count(), 0);
}

#[tokio::test]
async fn strategy_market_expired_cleans_up_active_execution() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    // Trigger execution
    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    assert_eq!(strategy.active_execution_count(), 1);

    // Market expires
    let event = Event::MarketData(MarketDataEvent::MarketExpired("m1".to_string()));
    strategy.on_event(&event, &ctx).await.unwrap();

    assert_eq!(strategy.active_execution_count(), 0);
}

// ---------------------------------------------------------------------------
// Strategy tests — pending subscriptions drain
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_drains_pending_subscriptions() {
    let config = DutchBookConfig::default();
    let mut strategy = DutchBookStrategy::new(config);

    // Add markets to pending queue
    {
        let mut pending = strategy.pending_subscriptions.lock().await;
        pending.push(make_market_info("m1", "t1a", "t1b"));
        pending.push(make_market_info("m2", "t2a", "t2b"));
    }

    let ctx = StrategyContext::new();
    // Any event should drain the queue
    let event = Event::System(SystemEvent::EngineStarted);
    let actions = strategy.on_event(&event, &ctx).await.unwrap();

    // Should emit SubscribeMarket for each pending market
    assert_eq!(actions.len(), 2);
    let subscribe_count = actions
        .iter()
        .filter(|a| matches!(a, Action::SubscribeMarket(_)))
        .count();
    assert_eq!(subscribe_count, 2);

    // Markets should be registered in analyzer
    assert_eq!(strategy.tracked_market_count(), 2);

    // Queue should be empty now
    let pending = strategy.pending_subscriptions.lock().await;
    assert!(pending.is_empty());
}

#[tokio::test]
async fn strategy_empty_pending_no_actions() {
    let config = DutchBookConfig::default();
    let mut strategy = DutchBookStrategy::new(config);

    let ctx = StrategyContext::new();
    let event = Event::System(SystemEvent::EngineStarted);
    let actions = strategy.on_event(&event, &ctx).await.unwrap();
    assert!(actions.is_empty());
}

// ---------------------------------------------------------------------------
// Strategy tests — rejected order handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_rejected_order_treated_as_cancel() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    // Trigger + place
    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, _no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Reject the YES order — goes to OneCancelled (waiting for NO side event)
    let reject_event = Event::OrderUpdate(OrderEvent::Rejected {
        order_id: Some(yes_oid.clone()),
        reason: "Insufficient funds".to_string(),
        token_id: Some("tok_yes".to_string()),
    });
    let actions = strategy.on_event(&reject_event, &ctx).await.unwrap();
    assert!(actions.is_empty());
    assert_eq!(strategy.active_execution_count(), 1); // still waiting for NO side

    // Cancel NO side too — now both cancelled → Complete → cleanup
    let cancel_no = Event::OrderUpdate(OrderEvent::Cancelled(_no_oid.clone()));
    let actions = strategy.on_event(&cancel_no, &ctx).await.unwrap();
    assert!(actions.is_empty());
    assert_eq!(strategy.active_execution_count(), 0);
}

// ---------------------------------------------------------------------------
// Strategy tests — event routing coverage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_ignores_irrelevant_events() {
    let config = DutchBookConfig::default();
    let mut strategy = DutchBookStrategy::new(config);
    let ctx = StrategyContext::new();

    // PriceChange event — should not crash
    let event = Event::MarketData(MarketDataEvent::PriceChange {
        token_id: "tok_yes".to_string(),
        price: dec!(0.50),
        side: OrderSide::Buy,
        best_bid: dec!(0.49),
        best_ask: dec!(0.51),
    });
    let actions = strategy.on_event(&event, &ctx).await.unwrap();
    assert!(actions.is_empty());

    // ExternalPrice event
    let event = Event::MarketData(MarketDataEvent::ExternalPrice {
        symbol: "BTC".to_string(),
        price: dec!(60000),
        source: "binance".to_string(),
        timestamp: Utc::now(),
    });
    let actions = strategy.on_event(&event, &ctx).await.unwrap();
    assert!(actions.is_empty());
}

// ---------------------------------------------------------------------------
// Task 5: Execution lifecycle tests — both fill → PairedPosition
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_both_filled_creates_position_with_correct_prices() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.45), dec!(300), dec!(0.50), dec!(250)).await;

    let snapshot = make_orderbook("tok_yes", dec!(0.45), dec!(300));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Fill both sides at the ask prices
    strategy.handle_order_filled(&yes_oid, "tok_yes", dec!(0.45), dec!(100)).await;
    let actions = strategy.handle_order_filled(&no_oid, "tok_no", dec!(0.50), dec!(100)).await;

    // Should have a Log action
    assert!(!actions.is_empty());

    // Position should exist
    let pos = strategy.open_positions.get("m1").unwrap();
    assert_eq!(pos.yes_entry_price, dec!(0.45));
    assert_eq!(pos.no_entry_price, dec!(0.50));
    assert_eq!(pos.size, dec!(100));
    // combined_cost = (0.45 + 0.50) * 100 = 95
    assert_eq!(pos.combined_cost, dec!(95));
    // expected_profit = 100 - 95 = 5
    assert_eq!(pos.expected_profit, dec!(5));

    // Order mappings should be cleaned up
    assert!(!strategy.order_to_market.contains_key(&yes_oid));
    assert!(!strategy.order_to_market.contains_key(&no_oid));
}

#[tokio::test]
async fn lifecycle_both_filled_no_first_then_yes() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Fill NO first, then YES
    strategy.handle_order_filled(&no_oid, "tok_no", dec!(0.49), dec!(100)).await;
    let actions = strategy.handle_order_filled(&yes_oid, "tok_yes", dec!(0.48), dec!(100)).await;

    assert!(!actions.is_empty());
    assert_eq!(strategy.active_execution_count(), 0);
    assert_eq!(strategy.open_position_count(), 1);

    let pos = strategy.open_positions.get("m1").unwrap();
    assert_eq!(pos.yes_entry_price, dec!(0.48));
    assert_eq!(pos.no_entry_price, dec!(0.49));
}

// ---------------------------------------------------------------------------
// Task 5: Partial fill → emergency unwind tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_partial_fill_no_side_triggers_unwind_sell_no() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Fill NO side
    strategy.handle_order_filled(&no_oid, "tok_no", dec!(0.49), dec!(100)).await;

    // Cancel YES — partial fill, NO side filled, should sell NO
    let actions = strategy.handle_order_cancelled(&yes_oid).await;

    assert_eq!(actions.len(), 1);
    match &actions[0] {
        Action::PlaceOrder(order) => {
            assert_eq!(order.token_id, "tok_no");
            assert_eq!(order.side, OrderSide::Sell);
            assert_eq!(order.order_type, OrderType::Gtc);
            // sell_price = 0.49 * (1 - 0.03) = 0.49 * 0.97 = 0.4753
            assert_eq!(order.price, dec!(0.49) * dec!(0.97));
            assert_eq!(order.size, dec!(100));
        }
        other => panic!("Expected PlaceOrder, got {:?}", other),
    }
}

#[tokio::test]
async fn lifecycle_unwind_discount_configurable() {
    let config = DutchBookConfig {
        max_combined_cost: dec!(0.99),
        min_profit_threshold: dec!(0.005),
        max_position_size: dec!(100),
        max_concurrent_positions: 2,
        unwind_discount: dec!(0.05), // 5% discount
        ..DutchBookConfig::default()
    };
    let mut strategy = DutchBookStrategy::new(config);

    let market = make_market_info("m1", "tok_yes", "tok_no");
    strategy.analyzer.add_market(&market);

    let ctx = StrategyContext::new();
    {
        let mut bal = ctx.balance.write().await;
        bal.available_usdc = dec!(10000);
    }
    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks
            .insert("tok_yes".to_string(), make_orderbook("tok_yes", dec!(0.45), dec!(200)));
        md.orderbooks
            .insert("tok_no".to_string(), make_orderbook("tok_no", dec!(0.45), dec!(200)));
    }

    let snapshot = make_orderbook("tok_yes", dec!(0.45), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    strategy.handle_order_filled(&yes_oid, "tok_yes", dec!(0.40), dec!(100)).await;
    let actions = strategy.handle_order_cancelled(&no_oid).await;

    match &actions[0] {
        Action::PlaceOrder(order) => {
            // sell_price = 0.40 * (1 - 0.05) = 0.40 * 0.95 = 0.38
            assert_eq!(order.price, dec!(0.40) * dec!(0.95));
        }
        other => panic!("Expected PlaceOrder, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// Task 5: Both cancelled → clean removal tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_both_cancelled_first_yes_then_no() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Cancel YES first (neither filled yet) — goes to OneCancelled, still waiting
    let actions = strategy.handle_order_cancelled(&yes_oid).await;
    assert!(actions.is_empty());
    assert_eq!(strategy.active_execution_count(), 1);

    // Cancel NO second — both cancelled → Complete, cleaned up
    let actions = strategy.handle_order_cancelled(&no_oid).await;
    assert!(actions.is_empty());
    assert_eq!(strategy.active_execution_count(), 0);
    assert!(!strategy.order_to_market.contains_key(&yes_oid));
    assert!(!strategy.order_to_market.contains_key(&no_oid));
    assert_eq!(strategy.open_position_count(), 0);
}

#[tokio::test]
async fn lifecycle_both_cancelled_first_no_then_yes() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Cancel NO first — goes to OneCancelled
    let actions = strategy.handle_order_cancelled(&no_oid).await;
    assert!(actions.is_empty());
    assert_eq!(strategy.active_execution_count(), 1);

    // Cancel YES second — both cancelled → Complete
    let actions = strategy.handle_order_cancelled(&yes_oid).await;
    assert!(actions.is_empty());
    assert_eq!(strategy.active_execution_count(), 0);
}

// ---------------------------------------------------------------------------
// Task 5: Unwind order tracking and completion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_unwind_placed_transitions_to_unwinding() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Partial fill: YES fills, NO cancels
    strategy.handle_order_filled(&yes_oid, "tok_yes", dec!(0.48), dec!(100)).await;
    let _unwind_actions = strategy.handle_order_cancelled(&no_oid).await;

    // Simulate unwind sell order placement
    let unwind_result = OrderResult {
        success: true,
        order_id: Some("unwind_sell_1".to_string()),
        token_id: "tok_yes".to_string(),
        price: dec!(0.4656),
        size: dec!(100),
        side: OrderSide::Sell,
        status: Some("Placed".to_string()),
        message: "OK".to_string(),
    };
    strategy.handle_order_placed(&unwind_result);

    // Execution should now be in Unwinding state
    let exec = strategy.active_executions.get("m1").unwrap();
    assert_eq!(
        exec.state,
        ExecutionState::Unwinding {
            sell_order_id: "unwind_sell_1".to_string()
        }
    );
    assert!(strategy.order_to_market.contains_key("unwind_sell_1"));
}

#[tokio::test]
async fn lifecycle_unwind_fill_completes_execution() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Partial fill: YES fills, NO cancels
    strategy.handle_order_filled(&yes_oid, "tok_yes", dec!(0.48), dec!(100)).await;
    strategy.handle_order_cancelled(&no_oid).await;

    // Place unwind sell order
    let unwind_result = OrderResult {
        success: true,
        order_id: Some("unwind_sell_1".to_string()),
        token_id: "tok_yes".to_string(),
        price: dec!(0.4656),
        size: dec!(100),
        side: OrderSide::Sell,
        status: Some("Placed".to_string()),
        message: "OK".to_string(),
    };
    strategy.handle_order_placed(&unwind_result);

    // Unwind sell fills
    let actions =
        strategy.handle_order_filled("unwind_sell_1", "tok_yes", dec!(0.4656), dec!(100)).await;

    // Should emit a warning log about loss
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        Action::Log { level, message } => {
            assert!(matches!(level, LogLevel::Warn));
            assert!(message.contains("unwind"));
            assert!(message.contains("m1"));
        }
        other => panic!("Expected Log action, got {:?}", other),
    }

    // Execution fully cleaned up
    assert_eq!(strategy.active_execution_count(), 0);
    assert!(!strategy.order_to_market.contains_key("unwind_sell_1"));
    assert!(!strategy.order_to_market.contains_key(&yes_oid));
    assert!(!strategy.order_to_market.contains_key(&no_oid));
    // No position created (it was unwound, not completed)
    assert_eq!(strategy.open_position_count(), 0);
}

#[tokio::test]
async fn lifecycle_unwind_cancel_keeps_tracking() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // Partial fill: YES fills, NO cancels
    strategy.handle_order_filled(&yes_oid, "tok_yes", dec!(0.48), dec!(100)).await;
    strategy.handle_order_cancelled(&no_oid).await;

    // Place unwind sell order
    let unwind_result = OrderResult {
        success: true,
        order_id: Some("unwind_sell_1".to_string()),
        token_id: "tok_yes".to_string(),
        price: dec!(0.4656),
        size: dec!(100),
        side: OrderSide::Sell,
        status: Some("Placed".to_string()),
        message: "OK".to_string(),
    };
    strategy.handle_order_placed(&unwind_result);

    // Unwind sell order gets cancelled (e.g. timeout)
    let actions = strategy.handle_order_cancelled("unwind_sell_1").await;

    // Should return empty (logged warning internally)
    assert!(actions.is_empty());

    // Execution should still be tracked (manual intervention needed)
    assert_eq!(strategy.active_execution_count(), 1);
}

// ---------------------------------------------------------------------------
// Task 5: Unwind price calculation tests
// ---------------------------------------------------------------------------

#[test]
fn unwind_price_calculation_default_3_percent() {
    let fill_price = dec!(0.48);
    let discount = dec!(0.03);
    let sell_price = fill_price * (Decimal::ONE - discount);
    assert_eq!(sell_price, dec!(0.4656));
}

#[test]
fn unwind_price_calculation_5_percent() {
    let fill_price = dec!(0.50);
    let discount = dec!(0.05);
    let sell_price = fill_price * (Decimal::ONE - discount);
    assert_eq!(sell_price, dec!(0.475));
}

#[test]
fn unwind_price_calculation_small_fill_price() {
    let fill_price = dec!(0.10);
    let discount = dec!(0.03);
    let sell_price = fill_price * (Decimal::ONE - discount);
    assert_eq!(sell_price, dec!(0.0970));
}

#[test]
fn unwind_price_calculation_high_fill_price() {
    let fill_price = dec!(0.95);
    let discount = dec!(0.03);
    let sell_price = fill_price * (Decimal::ONE - discount);
    assert_eq!(sell_price, dec!(0.9215));
}

// ---------------------------------------------------------------------------
// Task 5: Full lifecycle integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn lifecycle_full_happy_path_via_events() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    // 1. Orderbook update detects opportunity
    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    let actions = strategy.on_event(&event, &ctx).await.unwrap();
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::PlaceBatchOrder(_)));

    // 2. Placed events
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // 3. Both fill via events
    let fill_yes = Event::OrderUpdate(OrderEvent::Filled {
        order_id: yes_oid.clone(),
        market_id: "m1".to_string(),
        token_id: "tok_yes".to_string(),
        price: dec!(0.48),
        size: dec!(100),
        side: OrderSide::Buy,
        strategy_name: "dutch-book".to_string(),
        realized_pnl: None,
        fee: None,
        order_type: Some("Fok".to_string()),
        orderbook_snapshot: None,
    });
    let fill_no = Event::OrderUpdate(OrderEvent::Filled {
        order_id: no_oid.clone(),
        market_id: "m1".to_string(),
        token_id: "tok_no".to_string(),
        price: dec!(0.49),
        size: dec!(100),
        side: OrderSide::Buy,
        strategy_name: "dutch-book".to_string(),
        realized_pnl: None,
        fee: None,
        order_type: Some("Fok".to_string()),
        orderbook_snapshot: None,
    });

    let actions = strategy.on_event(&fill_yes, &ctx).await.unwrap();
    assert!(actions.is_empty()); // first fill, no action yet

    let actions = strategy.on_event(&fill_no, &ctx).await.unwrap();
    assert!(!actions.is_empty()); // promotion log

    assert_eq!(strategy.open_position_count(), 1);
    assert_eq!(strategy.active_execution_count(), 0);

    // 4. Market expires → redeem
    let expire_event = Event::MarketData(MarketDataEvent::MarketExpired("m1".to_string()));
    let actions = strategy.on_event(&expire_event, &ctx).await.unwrap();
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::RedeemPosition(_)));
    assert_eq!(strategy.open_position_count(), 0);
}

#[tokio::test]
async fn lifecycle_full_unwind_path_via_events() {
    let (mut strategy, ctx, _market) =
        setup_strategy_with_market(dec!(0.48), dec!(200), dec!(0.49), dec!(150)).await;

    // 1. Orderbook update → PlaceBatchOrder
    let snapshot = make_orderbook("tok_yes", dec!(0.48), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    strategy.on_event(&event, &ctx).await.unwrap();
    let (yes_oid, no_oid) = simulate_placed_events(&mut strategy, "tok_yes", "tok_no");

    // 2. YES fills
    let fill_yes = Event::OrderUpdate(OrderEvent::Filled {
        order_id: yes_oid.clone(),
        market_id: "m1".to_string(),
        token_id: "tok_yes".to_string(),
        price: dec!(0.48),
        size: dec!(100),
        side: OrderSide::Buy,
        strategy_name: "dutch-book".to_string(),
        realized_pnl: None,
        fee: None,
        order_type: Some("Fok".to_string()),
        orderbook_snapshot: None,
    });
    strategy.on_event(&fill_yes, &ctx).await.unwrap();

    // 3. NO cancelled → emergency unwind emits PlaceOrder(SELL)
    let cancel_no = Event::OrderUpdate(OrderEvent::Cancelled(no_oid.clone()));
    let actions = strategy.on_event(&cancel_no, &ctx).await.unwrap();
    assert_eq!(actions.len(), 1);
    assert!(matches!(&actions[0], Action::PlaceOrder(_)));

    // 4. Unwind sell placed
    let unwind_placed = OrderResult {
        success: true,
        order_id: Some("unwind_1".to_string()),
        token_id: "tok_yes".to_string(),
        price: dec!(0.4656),
        size: dec!(100),
        side: OrderSide::Sell,
        status: Some("Placed".to_string()),
        message: "OK".to_string(),
    };
    let placed_event = Event::OrderUpdate(OrderEvent::Placed(unwind_placed));
    strategy.on_event(&placed_event, &ctx).await.unwrap();

    // 5. Unwind sell fills → execution complete
    let unwind_fill = Event::OrderUpdate(OrderEvent::Filled {
        order_id: "unwind_1".to_string(),
        market_id: "m1".to_string(),
        token_id: "tok_yes".to_string(),
        price: dec!(0.4656),
        size: dec!(100),
        side: OrderSide::Sell,
        strategy_name: "dutch-book".to_string(),
        realized_pnl: None,
        fee: None,
        order_type: Some("Gtc".to_string()),
        orderbook_snapshot: None,
    });
    let actions = strategy.on_event(&unwind_fill, &ctx).await.unwrap();
    assert!(!actions.is_empty()); // loss log

    assert_eq!(strategy.active_execution_count(), 0);
    assert_eq!(strategy.open_position_count(), 0);
}

// ---------------------------------------------------------------------------
// DutchBookState tests
// ---------------------------------------------------------------------------

#[test]
fn dutch_book_state_new_is_empty() {
    let state = DutchBookState::new();
    assert_eq!(state.tracked_markets, 0);
    assert!(state.positions.is_empty());
    assert!(state.executions.is_empty());
    assert!(state.recent_opportunities.is_empty());
    assert_eq!(state.total_opportunities, 0);
    assert_eq!(state.total_realized_pnl, Decimal::ZERO);
    assert_eq!(state.total_unwind_losses, Decimal::ZERO);
}

#[test]
fn dutch_book_state_record_opportunity_ring_buffer() {
    let mut state = DutchBookState::new();

    // Add 55 opportunities — should cap at 50
    for i in 0..55 {
        state.record_opportunity(ArbitrageOpportunity {
            market_id: format!("m{i}"),
            yes_ask: dec!(0.48),
            no_ask: dec!(0.49),
            combined_cost: dec!(0.97),
            profit_pct: dec!(0.0309),
            max_size: dec!(100),
            detected_at: Utc::now(),
        });
    }

    assert_eq!(state.total_opportunities, 55);
    assert_eq!(state.recent_opportunities.len(), 50);
    // Newest first: front should be m54
    assert_eq!(state.recent_opportunities.front().unwrap().market_id, "m54");
    // Oldest kept: m5 (m0-m4 were evicted)
    assert_eq!(state.recent_opportunities.back().unwrap().market_id, "m5");
}

// ---------------------------------------------------------------------------
// Dashboard rendering tests
// ---------------------------------------------------------------------------

fn make_test_state() -> DutchBookState {
    let mut state = DutchBookState::new();
    state.tracked_markets = 42;
    state.total_opportunities = 15;
    state.total_realized_pnl = dec!(5.25);

    // Add a position
    state.positions.push(PairedPosition {
        market_id: "0xabc123def456".to_string(),
        yes_token_id: "tok_yes".to_string(),
        no_token_id: "tok_no".to_string(),
        neg_risk: false,
        yes_entry_price: dec!(0.48),
        no_entry_price: dec!(0.49),
        size: dec!(100),
        combined_cost: dec!(97),
        expected_profit: dec!(3),
        opened_at: Utc::now() - Duration::hours(2),
    });

    // Add an opportunity
    state.recent_opportunities.push_front(ArbitrageOpportunity {
        market_id: "0xdef789abc012".to_string(),
        yes_ask: dec!(0.47),
        no_ask: dec!(0.50),
        combined_cost: dec!(0.97),
        profit_pct: dec!(0.0309),
        max_size: dec!(50),
        detected_at: Utc::now() - Duration::minutes(5),
    });

    // Add an active execution
    state.executions.push(PairedOrder {
        market_id: "0xexec111222".to_string(),
        yes_order_id: "ord1".to_string(),
        no_order_id: "ord2".to_string(),
        size: dec!(80),
        submitted_at: Utc::now(),
        state: ExecutionState::AwaitingFills {
            yes_filled: true,
            no_filled: false,
        },
        yes_fill_price: Some(dec!(0.45)),
        no_fill_price: None,
    });

    state
}

#[tokio::test]
async fn dashboard_renders_non_empty_html() {
    let state = Arc::new(RwLock::new(make_test_state()));
    let dashboard = DutchBookDashboard::new(state);
    let html = dashboard.render_view().await.unwrap();
    assert!(!html.is_empty());
}

#[tokio::test]
async fn dashboard_renders_summary_section() {
    let state = Arc::new(RwLock::new(make_test_state()));
    let dashboard = DutchBookDashboard::new(state);
    let html = dashboard.render_view().await.unwrap();

    assert!(html.contains("Dutch Book Summary"));
    assert!(html.contains("Markets monitored:"));
    assert!(html.contains("42")); // tracked_markets count
    assert!(html.contains("Active positions:"));
    assert!(html.contains("Opportunities detected:"));
    assert!(html.contains("15")); // total_opportunities
    assert!(html.contains("Realized P&amp;L:"));
}

#[tokio::test]
async fn dashboard_renders_positions_section() {
    let state = Arc::new(RwLock::new(make_test_state()));
    let dashboard = DutchBookDashboard::new(state);
    let html = dashboard.render_view().await.unwrap();

    assert!(html.contains("Active Positions (1)"));
    assert!(html.contains("0xabc123...")); // truncated market ID
    assert!(html.contains("0.48")); // YES price
    assert!(html.contains("0.49")); // NO price
    assert!(html.contains("YES Price"));
    assert!(html.contains("NO Price"));
}

#[tokio::test]
async fn dashboard_renders_opportunities_section() {
    let state = Arc::new(RwLock::new(make_test_state()));
    let dashboard = DutchBookDashboard::new(state);
    let html = dashboard.render_view().await.unwrap();

    assert!(html.contains("Recent Opportunities (1)"));
    assert!(html.contains("0xdef789...")); // truncated market ID (first 8 chars)
    assert!(html.contains("0.47")); // YES ask
    assert!(html.contains("0.50")); // NO ask
    assert!(html.contains("Profit %"));
}

#[tokio::test]
async fn dashboard_renders_execution_section() {
    let state = Arc::new(RwLock::new(make_test_state()));
    let dashboard = DutchBookDashboard::new(state);
    let html = dashboard.render_view().await.unwrap();

    assert!(html.contains("Execution Status (1)"));
    assert!(html.contains("0xexec11...")); // truncated market ID (first 8 chars)
    assert!(html.contains("Awaiting [Y/-]")); // yes filled, no awaiting
}

#[tokio::test]
async fn dashboard_renders_empty_state() {
    let state = Arc::new(RwLock::new(DutchBookState::new()));
    let dashboard = DutchBookDashboard::new(state);
    let html = dashboard.render_view().await.unwrap();

    assert!(html.contains("Dutch Book Summary"));
    assert!(html.contains("No active positions"));
    assert!(html.contains("No opportunities detected yet"));
    // Execution section should not appear when empty
    assert!(!html.contains("Execution Status"));
}

#[tokio::test]
async fn dashboard_view_name_is_dutch_book() {
    let state = Arc::new(RwLock::new(DutchBookState::new()));
    let dashboard = DutchBookDashboard::new(state);
    assert_eq!(dashboard.view_name(), "dutch-book");
}

#[tokio::test]
async fn dashboard_renders_unwind_losses() {
    let mut test_state = DutchBookState::new();
    test_state.total_unwind_losses = dec!(1.50);
    let state = Arc::new(RwLock::new(test_state));
    let dashboard = DutchBookDashboard::new(state);
    let html = dashboard.render_view().await.unwrap();

    assert!(html.contains("Unwind losses:"));
    assert!(html.contains("1.5000"));
}

#[tokio::test]
async fn dashboard_hides_unwind_losses_when_zero() {
    let state = Arc::new(RwLock::new(DutchBookState::new()));
    let dashboard = DutchBookDashboard::new(state);
    let html = dashboard.render_view().await.unwrap();

    assert!(!html.contains("Unwind losses:"));
}

#[tokio::test]
async fn dashboard_renders_unwinding_execution_state() {
    let mut test_state = DutchBookState::new();
    test_state.executions.push(PairedOrder {
        market_id: "0xunwind_test".to_string(),
        yes_order_id: "o1".to_string(),
        no_order_id: "o2".to_string(),
        size: dec!(50),
        submitted_at: Utc::now(),
        state: ExecutionState::Unwinding {
            sell_order_id: "sell_1".to_string(),
        },
        yes_fill_price: Some(dec!(0.45)),
        no_fill_price: None,
    });
    let state = Arc::new(RwLock::new(test_state));
    let dashboard = DutchBookDashboard::new(state);
    let html = dashboard.render_view().await.unwrap();

    assert!(html.contains("Unwinding"));
    assert!(html.contains("bp-loss")); // unwinding styled as loss
}

// ---------------------------------------------------------------------------
// Strategy shared state sync tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strategy_syncs_state_to_dashboard() {
    let shared = Arc::new(RwLock::new(DutchBookState::new()));
    let config = DutchBookConfig {
        max_combined_cost: dec!(0.99),
        min_profit_threshold: dec!(0.005),
        max_concurrent_positions: 10,
        ..DutchBookConfig::default()
    };
    let mut strategy = DutchBookStrategy::with_shared_state(config, Arc::clone(&shared));

    // Add a market to the analyzer
    let market = make_market_info("m1", "tok_yes", "tok_no");
    strategy.analyzer.add_market(&market);

    // Manually add a position
    strategy.open_positions.insert(
        "m1".to_string(),
        PairedPosition {
            market_id: "m1".to_string(),
            yes_token_id: "tok_yes".to_string(),
            no_token_id: "tok_no".to_string(),
            neg_risk: false,
            yes_entry_price: dec!(0.48),
            no_entry_price: dec!(0.49),
            size: dec!(100),
            combined_cost: dec!(97),
            expected_profit: dec!(3),
            opened_at: Utc::now(),
        },
    );

    // Trigger a sync via on_event with a no-op event
    let ctx = StrategyContext::new();
    let _actions = strategy
        .on_event(&Event::System(SystemEvent::EngineStarted), &ctx)
        .await
        .unwrap();

    // Check that the shared state was updated
    let state = shared.read().await;
    assert_eq!(state.tracked_markets, 1);
    assert_eq!(state.positions.len(), 1);
    assert_eq!(state.positions[0].market_id, "m1");
}

// ---------------------------------------------------------------------------
// Task 7: Integration tests (config wrapper, end-to-end with default config)
// ---------------------------------------------------------------------------

/// Verify that DutchBookConfig is correctly parsed from a full TOML config
/// (matching the ConfigWrapper pattern used in main.rs).
#[test]
fn config_wrapper_parses_dutch_book_section() {
    #[derive(Debug, serde::Deserialize, Default)]
    struct TestConfigWrapper {
        #[serde(default)]
        dutch_book: DutchBookConfig,
    }

    let toml_str = r#"
        [dutch_book]
        enabled = true
        max_combined_cost = 0.98
        min_profit_threshold = 0.01
        max_position_size = 50
        max_concurrent_positions = 5
    "#;

    let wrapper: TestConfigWrapper = toml::from_str(toml_str).unwrap();
    assert!(wrapper.dutch_book.enabled);
    assert_eq!(wrapper.dutch_book.max_combined_cost, dec!(0.98));
    assert_eq!(wrapper.dutch_book.min_profit_threshold, dec!(0.01));
    assert_eq!(wrapper.dutch_book.max_position_size, dec!(50));
    assert_eq!(wrapper.dutch_book.max_concurrent_positions, 5);
    // Non-specified fields should be defaults
    assert_eq!(wrapper.dutch_book.min_liquidity_usd, dec!(10000));
    assert_eq!(wrapper.dutch_book.scan_interval_secs, 600);
    assert!(wrapper.dutch_book.validate().is_ok());
}

/// Verify that DutchBookConfig defaults when [dutch_book] section is absent.
#[test]
fn config_wrapper_defaults_when_section_absent() {
    #[derive(Debug, serde::Deserialize, Default)]
    struct TestConfigWrapper {
        #[serde(default)]
        dutch_book: DutchBookConfig,
    }

    let toml_str = r#"
        [some_other_section]
        key = "value"
    "#;

    let wrapper: TestConfigWrapper = toml::from_str(toml_str).unwrap();
    assert!(!wrapper.dutch_book.enabled);
    assert_eq!(wrapper.dutch_book.max_combined_cost, dec!(0.99));
    assert!(wrapper.dutch_book.validate().is_ok());
}

/// End-to-end integration: create strategy with default config, send mock
/// events through on_event, verify actions.
#[tokio::test]
async fn integration_default_config_event_flow() {
    let config = DutchBookConfig {
        max_combined_cost: dec!(0.99),
        min_profit_threshold: dec!(0.005),
        max_position_size: dec!(100),
        max_concurrent_positions: 5,
        ..DutchBookConfig::default()
    };
    let shared_state = Arc::new(RwLock::new(DutchBookState::new()));
    let mut strategy =
        DutchBookStrategy::with_shared_state(config, Arc::clone(&shared_state));

    let ctx = StrategyContext::new();
    {
        let mut bal = ctx.balance.write().await;
        bal.available_usdc = dec!(10000);
    }

    // No-op event produces no actions (just drains empty pending queue)
    let actions = strategy
        .on_event(&Event::System(SystemEvent::EngineStarted), &ctx)
        .await
        .unwrap();
    assert!(actions.is_empty());

    // Add a market and populate orderbooks with an arbitrage opportunity
    let market = make_market_info("int_market", "int_yes", "int_no");
    strategy.analyzer.add_market(&market);
    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks.insert(
            "int_yes".to_string(),
            make_orderbook("int_yes", dec!(0.47), dec!(200)),
        );
        md.orderbooks.insert(
            "int_no".to_string(),
            make_orderbook("int_no", dec!(0.48), dec!(200)),
        );
    }

    // Orderbook update triggers opportunity detection
    let snapshot = make_orderbook("int_yes", dec!(0.47), dec!(200));
    let event = Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot));
    let actions = strategy.on_event(&event, &ctx).await.unwrap();
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        Action::PlaceBatchOrder(orders) => {
            assert_eq!(orders.len(), 2);
            assert_eq!(orders[0].side, OrderSide::Buy);
            assert_eq!(orders[1].side, OrderSide::Buy);
            assert_eq!(orders[0].order_type, OrderType::Fok);
            assert_eq!(orders[1].order_type, OrderType::Fok);
        }
        other => panic!("Expected PlaceBatchOrder, got {:?}", other),
    }

    // Dashboard state should have recorded the opportunity
    let state = shared_state.read().await;
    assert_eq!(state.total_opportunities, 1);
    assert_eq!(state.recent_opportunities.len(), 1);
    // Tracked markets synced
    assert_eq!(state.tracked_markets, 1);
}

/// Strategy name matches what main.rs expects for registration and backtest matching.
#[test]
fn integration_strategy_name_matches_expected() {
    let config = DutchBookConfig::default();
    let strategy = DutchBookStrategy::new(config);
    assert_eq!(strategy.name(), "dutch-book");
}
