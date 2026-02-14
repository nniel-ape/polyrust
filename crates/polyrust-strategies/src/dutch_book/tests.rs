use std::collections::{HashMap, HashSet};

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use polyrust_core::prelude::*;

use super::analyzer::ArbitrageAnalyzer;
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
