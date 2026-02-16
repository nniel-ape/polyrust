use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use polyrust_core::prelude::*;

use super::*;
use crate::crypto_arb::config::{StopLossConfig, TailEndConfig};
use crate::crypto_arb::domain::{
    ArbitragePosition, ExitOrderMeta, OpenLimitOrder, PositionLifecycle, PositionLifecycleState,
    StopLossTriggerKind, TriggerEvalContext, compute_exit_clip,
};
use crate::crypto_arb::services::taker_fee;

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

    // Healthy -> ExitExecuting (FOK)
    let mut lc = PositionLifecycle::new();
    let result = lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "order-1".to_string(),
            order_type: OrderType::Fok,
            exit_price: dec!(0.90),
            submitted_at: t,
            hedge_order_id: None,
            hedge_price: None,
        },
        "trigger fired",
        t,
    );
    assert!(result.is_ok());

    // ExitExecuting -> ExitExecuting (GTC residual after partial fill)
    let result = lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "order-2".to_string(),
            order_type: OrderType::Gtc,
            exit_price: dec!(0.89),
            submitted_at: t,
            hedge_order_id: None,
            hedge_price: None,
        },
        "GTC residual chase",
        t,
    );
    assert!(result.is_ok());

    // ExitExecuting -> Healthy (cancelled)
    let result = lc.transition(PositionLifecycleState::Healthy, "GTC cancelled", t);
    assert!(result.is_ok());

    // Healthy -> ExitExecuting (second trigger)
    let result = lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "order-3".to_string(),
            order_type: OrderType::Fok,
            exit_price: dec!(0.88),
            submitted_at: t,
            hedge_order_id: None,
            hedge_price: None,
        },
        "second trigger",
        t,
    );
    assert!(result.is_ok());

    // ExitExecuting -> Healthy (rejected)
    let result = lc.transition(PositionLifecycleState::Healthy, "order rejected", t);
    assert!(result.is_ok());

    // ExitExecuting -> Hedged (hedge fill)
    let result = lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "order-4".to_string(),
            order_type: OrderType::Fok,
            exit_price: dec!(0.87),
            submitted_at: t,
            hedge_order_id: None,
            hedge_price: None,
        },
        "third trigger",
        t,
    );
    assert!(result.is_ok());
    let result = lc.transition(
        PositionLifecycleState::Hedged {
            hedge_cost: dec!(5.0),
            hedged_at: t,
        },
        "hedge filled",
        t,
    );
    assert!(result.is_ok());

    // Verify transitions are logged (7 transitions in lc)
    assert_eq!(lc.transition_log.len(), 7);
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
            hedge_order_id: None,
            hedge_price: None,
        },
        "hard crash trigger",
        t,
    );
    assert!(result.is_ok());
    assert!(matches!(
        lc.state,
        PositionLifecycleState::ExitExecuting { .. }
    ));
}

#[test]
fn lifecycle_exit_executing_to_hedged_is_valid() {
    let t = now();
    let mut lc = PositionLifecycle::new();

    // First transition to ExitExecuting
    lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "o1".into(),
            order_type: OrderType::Fok,
            exit_price: dec!(0.92),
            submitted_at: t,
            hedge_order_id: None,
            hedge_price: None,
        },
        "exit trigger",
        t,
    )
    .unwrap();

    // ExitExecuting -> Hedged is a valid transition
    let result = lc.transition(
        PositionLifecycleState::Hedged {
            hedge_cost: dec!(5.0),
            hedged_at: t,
        },
        "hedge filled",
        t,
    );
    assert!(result.is_ok());
    assert!(matches!(lc.state, PositionLifecycleState::Hedged { .. }));
}

#[test]
fn lifecycle_invalid_transitions_return_error() {
    let t = now();

    // Healthy -> Healthy (invalid: self-loop on Healthy)
    let mut lc = PositionLifecycle::new();
    let result = lc.transition(PositionLifecycleState::Healthy, "invalid self-loop", t);
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("Healthy"));

    // Healthy -> Hedged (invalid: cannot skip ExitExecuting)
    let mut lc = PositionLifecycle::new();
    let result = lc.transition(
        PositionLifecycleState::Hedged {
            hedge_cost: dec!(5.0),
            hedged_at: t,
        },
        "skip to hedged",
        t,
    );
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("Healthy") && msg.contains("Hedged"));
}

#[test]
fn lifecycle_transition_log_caps_at_50() {
    let t = now();
    let mut lc = PositionLifecycle::new();

    // Generate 60 transitions by cycling Healthy -> ExitExecuting -> Healthy -> ExitExecuting...
    for i in 0..60 {
        if matches!(lc.state, PositionLifecycleState::Healthy) {
            lc.transition(
                PositionLifecycleState::ExitExecuting {
                    order_id: format!("o-{i}"),
                    order_type: OrderType::Fok,
                    exit_price: dec!(0.90),
                    submitted_at: t,
                    hedge_order_id: None,
                    hedge_price: None,
                },
                &format!("transition {i}"),
                t,
            )
            .unwrap();
        } else if matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }) {
            lc.transition(
                PositionLifecycleState::Healthy,
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
        PositionLifecycleState::Hedged {
            hedge_cost: dec!(5.0),
            hedged_at: t,
        },
        "should fail",
        t,
    );
    assert_eq!(lc.state, PositionLifecycleState::Healthy);
    assert!(
        lc.transition_log.is_empty(),
        "Failed transition should not log"
    );
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
        hedge_order_id: None,
        hedge_price: None,
    };
    let s = format!("{state}");
    assert!(s.contains("ExitExecuting"));
    assert!(s.contains("Fok"));
    assert!(s.contains("0.92"));
}

// ---------------------------------------------------------------------------
// ArbitragePosition entry fee/order metadata tests
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
        recovery_cost: Decimal::ZERO,
    };

    assert_eq!(pos.entry_order_type, OrderType::Fok);
    assert_eq!(pos.entry_fee_per_share, expected_fee);
    assert!(pos.entry_fee_per_share > Decimal::ZERO);
    // At p=0.92: fee = 2 * 0.92 * 0.08 * 0.0315 = 0.0046368
    assert_eq!(pos.entry_fee_per_share, dec!(0.0046368));
}

// ---------------------------------------------------------------------------
// Lifecycle store tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn position_creation_creates_lifecycle_in_healthy_state() {
    let base = make_base_with_market("m1", 300).await;

    // Create and record a position
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
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
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
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
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
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
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
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
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );
    base.record_position(pos).await;

    // Simulate adding an exit order for this token
    {
        let mut exit_orders = base.exit_orders_by_id.write().await;
        exit_orders.insert(
            "exit-order-1".to_string(),
            ExitOrderMeta {
                token_id: "token_up".to_string(),
                order_token_id: "token_up".to_string(),
                order_type: OrderType::Fok,
                source_state: "ExitExecuting".to_string(),

                exit_price: dec!(0.85),
                clip_size: dec!(10),
            },
        );
        // Add an unrelated exit order too
        exit_orders.insert(
            "exit-order-2".to_string(),
            ExitOrderMeta {
                token_id: "other_token".to_string(),
                order_token_id: "other_token".to_string(),
                order_type: OrderType::Gtc,
                source_state: "ExitExecuting".to_string(),

                exit_price: dec!(0.80),
                clip_size: dec!(5),
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
// evaluate_triggers tests
// ---------------------------------------------------------------------------

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
        dec!(0.95),        // entry
        dec!(0.95),        // peak (no profit yet)
        dec!(0.86),        // current bid (dropped 0.09)
        dec!(90000),       // reference BTC price
        Some(dec!(90000)), // no external reversal
        300,               // time remaining
        15,                // seconds since entry (past sell delay)
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
        dec!(0.95),        // entry
        dec!(0.95),        // peak
        dec!(0.94),        // bid only dropped 0.01 (not enough for bid-based hard crash)
        dec!(90000),       // reference
        Some(dec!(89400)), // external dropped 0.667%
        300,
        15,
    );

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(
        trigger.is_some(),
        "Hard crash should fire on 0.667% reversal"
    );
    match trigger.unwrap() {
        StopLossTriggerKind::HardCrash { reversal_pct, .. } => {
            // reversal = (90000-89400)/90000 = 600/90000 ≈ 0.00667
            assert!(
                reversal_pct >= dec!(0.006),
                "Reversal {reversal_pct} should be >= 0.006"
            );
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
    assert!(
        trigger.is_some(),
        "Hard crash should work with single fresh source (no composite)"
    );
    assert!(matches!(
        trigger.unwrap(),
        StopLossTriggerKind::HardCrash { .. }
    ));
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
    // seconds_since_entry=25 to be beyond post_entry_window_secs (20) so Level 4
    // doesn't fire first.
    let ctx = make_trigger_ctx(
        dec!(0.95),        // entry
        dec!(0.95),        // peak
        dec!(0.90), // bid dropped 0.05 (= min_drop, satisfies market_dropped but not hard crash)
        dec!(90000), // reference
        Some(dec!(89700)), // reversal = 300/90000 = 0.333% >= reversal_pct (0.003)
        300,
        25,
    );

    // First tick: counter goes to 1, not yet at threshold (2)
    let trigger1 = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(
        trigger1.is_none(),
        "First tick should not trigger (need 2 consecutive)"
    );
    assert_eq!(lifecycle.dual_trigger_ticks, 1);

    // Second tick: counter goes to 2, threshold met
    let trigger2 = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(
        trigger2.is_some(),
        "Second tick should trigger (2 consecutive)"
    );
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
    // seconds_since_entry=25 to be beyond post_entry_window_secs (20) so Level 4
    // doesn't fire first.
    let ctx1 = make_trigger_ctx(
        dec!(0.95),
        dec!(0.95),
        dec!(0.90), // market dropped 0.05
        dec!(90000),
        Some(dec!(89700)), // crypto reversed
        300,
        25,
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
        26,
    );
    lifecycle.evaluate_triggers(&ctx2, &sl, &te);
    assert_eq!(
        lifecycle.dual_trigger_ticks, 0,
        "Counter should reset when market condition clears"
    );
}

#[test]
fn evaluate_triggers_trailing_unarmable_at_high_entry() {
    // Entry at 0.99 with tick_size 0.01: price_cap = 0.99, headroom = 0,
    // effective_arm_distance = min(0.015, 0) = 0 < 0.01 => trailing_unarmable
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    let ctx = make_trigger_ctx(
        dec!(0.99), // entry (very high)
        dec!(0.99), // peak
        dec!(0.97), // bid dropped 0.02
        dec!(90000),
        Some(dec!(90000)), // no reversal
        300,
        15,
    );

    let _trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);

    // Hard crash doesn't fire (bid_drop=0.02 < 0.08, no external reversal)
    // Dual trigger doesn't fire (market_dropped but no crypto reversal)
    // Trailing should be marked unarmable
    assert!(
        lifecycle.trailing_unarmable,
        "Trailing should be unarmable at entry 0.99"
    );

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
    assert!(
        trigger2.is_some(),
        "Hard crash should work even when trailing is unarmable"
    );
    assert!(matches!(
        trigger2.unwrap(),
        StopLossTriggerKind::HardCrash { .. }
    ));
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
        dec!(0.90), // entry
        dec!(0.96), // peak (armed: 0.96 >= 0.915)
        dec!(0.93), // current bid
        dec!(90000),
        Some(dec!(90000)),
        450, // time remaining
        15,
    );

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(
        trigger.is_some(),
        "Trailing should trigger on 0.03 drop from peak at 450s"
    );
    match trigger.unwrap() {
        StopLossTriggerKind::TrailingStop {
            peak_bid,
            current_bid,
            effective_distance,
        } => {
            assert_eq!(peak_bid, dec!(0.96));
            assert_eq!(current_bid, dec!(0.93));
            assert_eq!(effective_distance, dec!(0.025));
        }
        other => panic!("Expected TrailingStop, got {other}"),
    }
    assert!(
        !lifecycle.trailing_unarmable,
        "Should NOT be unarmable at entry 0.90"
    );
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
    assert!(
        trigger.is_some(),
        "Trailing should trigger with floor distance at 90s"
    );
    match trigger.unwrap() {
        StopLossTriggerKind::TrailingStop {
            effective_distance, ..
        } => {
            assert_eq!(
                effective_distance,
                dec!(0.015),
                "Floor should prevent decay below 0.015"
            );
        }
        other => panic!("Expected TrailingStop, got {other}"),
    }
}

#[test]
fn evaluate_triggers_post_entry_exit_during_sell_delay() {
    // Post-entry deferred triggers within sell delay window when bid drops
    let sl = StopLossConfig::default();
    let te = TailEndConfig::default();
    let mut lifecycle = PositionLifecycle::new();

    // Within sell delay (5s < min_sell_delay_secs=10) and post_entry_window (5s <= 20s)
    // Bid dropped 0.06 from entry (>= post_entry_exit_drop=0.05)
    let ctx = make_trigger_ctx(
        dec!(0.95), // entry
        dec!(0.95), // peak
        dec!(0.89), // bid dropped 0.06
        dec!(90000),
        Some(dec!(90000)),
        300,
        5, // 5s since entry (within sell delay of 10s)
    );

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(
        trigger.is_some(),
        "Post-entry deferred should fire within sell delay"
    );
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
        book_age_ms: 5000,                 // STALE book (> 1200ms)
        external_price: Some(dec!(89700)), // crypto reversed
        external_age_ms: Some(500),
        composite_sources: Some(3),
        time_remaining: 300,
        now,
    };

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    // Book is stale, so: hard crash (needs fresh book) = no,
    // dual trigger = no, trailing = no, post-entry = no
    assert!(
        trigger.is_none(),
        "All triggers should be suppressed with stale orderbook"
    );
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
        dec!(0.96), // peak (armed)
        dec!(0.90), // drop = 0.06 >= 0.05
        dec!(90000),
        None, // NO external price
        900,
        15,
    );

    let trigger = lifecycle.evaluate_triggers(&ctx, &sl, &te);
    assert!(
        trigger.is_some(),
        "Trailing should work without external price"
    );
    assert!(matches!(
        trigger.unwrap(),
        StopLossTriggerKind::TrailingStop { .. }
    ));
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
    assert_eq!(
        clip,
        dec!(4),
        "Depth * cap_factor should be the limiting factor"
    );
}

#[test]
fn exit_clip_below_min_size_returns_zero() {
    // remaining=10, bid_depth=0.5, cap=0.8 → depth_capped=0.4, below min_size=1 → 0
    let clip = compute_exit_clip(dec!(10), dec!(0.5), dec!(0.8), dec!(1));
    assert_eq!(
        clip,
        Decimal::ZERO,
        "Below min_size should return zero (dust)"
    );
}

#[test]
fn exit_clip_dust_remaining_returns_zero() {
    // remaining=0.001, bid_depth=100, cap=0.8 → capped=0.001, below min_size=1 → 0
    let clip = compute_exit_clip(dec!(0.001), dec!(100), dec!(0.8), dec!(1));
    assert_eq!(clip, Decimal::ZERO, "Dust remaining should return zero");
}

// ---------------------------------------------------------------------------
// Hedge fill lifecycle tests
// ---------------------------------------------------------------------------

#[test]
fn hedge_fill_transitions_to_hedged_state() {
    let t = now();
    let mut lc = PositionLifecycle::new();

    // First transition to ExitExecuting with a hedge
    lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "exit-1".to_string(),
            order_type: OrderType::Fak,
            exit_price: dec!(0.90),
            submitted_at: t,
            hedge_order_id: Some("hedge-1".to_string()),
            hedge_price: Some(dec!(0.08)),
        },
        "trigger fired",
        t,
    )
    .unwrap();

    // Then transition to Hedged
    lc.transition(
        PositionLifecycleState::Hedged {
            hedge_cost: dec!(0.082),
            hedged_at: t,
        },
        "hedge filled",
        t,
    )
    .unwrap();

    assert!(matches!(lc.state, PositionLifecycleState::Hedged { .. }));
    if let PositionLifecycleState::Hedged { hedge_cost, .. } = &lc.state {
        assert_eq!(*hedge_cost, dec!(0.082));
    }
}

/// Test sell fills before hedge: position resolved, hedge should be cancelled.
/// This verifies the lifecycle allows sell-first resolution.
#[test]
fn sell_fills_before_hedge_position_resolved() {
    let t = now();
    let mut lc = PositionLifecycle::new();

    // Transition to ExitExecuting with hedge pending
    lc.transition(
        PositionLifecycleState::ExitExecuting {
            order_id: "exit-1".to_string(),
            order_type: OrderType::Fak,
            exit_price: dec!(0.90),
            submitted_at: t,
            hedge_order_id: Some("hedge-1".to_string()),
            hedge_price: Some(dec!(0.08)),
        },
        "trigger fired",
        t,
    )
    .unwrap();

    // Verify ExitExecuting has hedge tracking
    if let PositionLifecycleState::ExitExecuting {
        hedge_order_id,
        hedge_price,
        ..
    } = &lc.state
    {
        assert_eq!(hedge_order_id.as_deref(), Some("hedge-1"));
        assert_eq!(*hedge_price, Some(dec!(0.08)));
    } else {
        panic!("Expected ExitExecuting state");
    }

    // When sell fills first, position is removed (lifecycle is cleaned up externally).
    // The ExitExecuting -> Healthy transition is used for partial fills that
    // cancel the hedge, while full fills just remove the lifecycle entirely.
    // Test that ExitExecuting can transition to Healthy (for cancel-and-retry path):
    let result = lc.transition(
        PositionLifecycleState::Healthy,
        "sell cancelled for retry",
        t,
    );
    assert!(result.is_ok());
}
