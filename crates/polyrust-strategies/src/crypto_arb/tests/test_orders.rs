use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use polyrust_core::prelude::*;

use super::*;
use crate::crypto_arb::config::ArbitrageConfig;
use crate::crypto_arb::domain::{OpenLimitOrder, StopLossRejectionKind};
use crate::crypto_arb::runtime::CryptoArbRuntime;

// ---------------------------------------------------------------------------
// Rejection cooldown tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rejection_cooldown_blocks_reevaluation() {
    let config = ArbitrageConfig::default();
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

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
    let config = ArbitrageConfig::default();
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    let market_id = "market-456".to_string();

    // Record a very short cooldown (1 second)
    base.record_rejection_cooldown(&market_id, 1).await;
    assert!(base.is_rejection_cooled_down(&market_id).await);

    // Advance simulated time by 2 seconds to expire the cooldown
    *base.last_event_time.write().await = Utc::now() + chrono::Duration::seconds(2);
    assert!(!base.is_rejection_cooled_down(&market_id).await);
}

// ---------------------------------------------------------------------------
// Stale order management tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_limit_order_cancelled_after_max_age() {
    let mut config = ArbitrageConfig::default();
    config.order.max_age_secs = 1; // 1 second for quick test
    config.use_chainlink = false;
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    // Add a limit order with a past placed_at
    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert(
            "old-order".to_string(),
            OpenLimitOrder {
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
    let mut config = ArbitrageConfig::default();
    config.order.max_age_secs = 1;
    config.use_chainlink = false;
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert(
            "old-order".to_string(),
            OpenLimitOrder {
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
// Reconciliation tests
// ---------------------------------------------------------------------------

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
    let mut clob_open = HashSet::new();
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
    let clob_open = HashSet::new();
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
// StopLossRejectionKind classification tests
// ---------------------------------------------------------------------------

#[test]
fn rejection_kind_classifies_liquidity() {
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
// Reconcile miss counter grace period
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconcile_miss_counter_resets_on_reappear() {
    let base = make_base_no_chainlink();

    let lo = make_open_limit_order("order-1", "market-A", "token-A-up");
    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert("order-1".to_string(), lo);
    }

    let empty_clob = HashSet::new();

    // First miss: miss_count goes to 1
    let actions = base.reconcile_limit_orders(&empty_clob).await;
    assert!(actions.is_empty());
    {
        let limits = base.open_limit_orders.read().await;
        assert_eq!(limits["order-1"].reconcile_miss_count, 1);
    }

    // Order reappears in the next snapshot — miss_count resets to 0
    let mut clob_with_order = HashSet::new();
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
