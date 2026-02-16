use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use polyrust_core::prelude::*;

use super::*;
use crate::crypto_arb::config::ArbitrageConfig;
use crate::crypto_arb::domain::{
    ArbitragePosition, CompositePriceResult, ExitOrderMeta, MarketWithReference, ModeStats,
    OpenLimitOrder, PendingOrder, PositionLifecycleState, ReferenceQuality,
};
use crate::crypto_arb::runtime::CryptoArbRuntime;
use crate::crypto_arb::strategy::tailend::TailEndStrategy;

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
    let mut config = ArbitrageConfig::default();
    config.performance.auto_disable = true;
    config.performance.min_trades = 20;
    config.performance.min_win_rate = dec!(0.40);
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    // Record exactly 20 trades: 8 wins (40%), 12 losses
    for _ in 0..8 {
        base.record_trade_pnl(dec!(1.0)).await;
    }
    for _ in 0..12 {
        base.record_trade_pnl(dec!(-1.0)).await;
    }

    // 40% win rate = exactly at threshold → NOT disabled (need to be strictly below)
    assert!(
        !base.is_auto_disabled().await,
        "At exactly min_win_rate should NOT be disabled"
    );
}

#[tokio::test]
async fn auto_disable_below_threshold() {
    let mut config = ArbitrageConfig::default();
    config.performance.auto_disable = true;
    config.performance.min_trades = 20;
    config.performance.min_win_rate = dec!(0.40);
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    // Record 20 trades: 7 wins (35%), 13 losses
    for _ in 0..7 {
        base.record_trade_pnl(dec!(1.0)).await;
    }
    for _ in 0..13 {
        base.record_trade_pnl(dec!(-1.0)).await;
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
            PendingOrder {
                market_id: market_id.clone(),
                token_id: "token2".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.90),
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
    let mut config = ArbitrageConfig::default();
    config.max_positions = 3;
    config.use_chainlink = false;
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

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
            PendingOrder {
                market_id: "m2".to_string(),
                token_id: "token2".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.90),
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
    assert!(base.can_open_position().await, "2/3 should allow opening");

    // Add 1 limit order → total = 3
    {
        let mut limits = base.open_limit_orders.write().await;
        limits.insert(
            "order3".to_string(),
            OpenLimitOrder {
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
// Strike proximity filter tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn strike_proximity_rejects_within_threshold() {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.min_sustained_secs = 0;
    config.tailend.min_sustained_ticks = 0;
    config.tailend.max_recent_volatility = dec!(1.0);
    config.tailend.min_strike_distance_pct = dec!(0.0008); // 0.08%
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

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
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.min_sustained_secs = 0;
    config.tailend.min_sustained_ticks = 0;
    config.tailend.max_recent_volatility = dec!(1.0);
    config.tailend.min_strike_distance_pct = dec!(0.0008); // 0.08%
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

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
        entries.push_back((
            now - Duration::seconds(3),
            dec!(1996),
            "test".to_string(),
            now - Duration::seconds(3),
        ));
        entries.push_back((
            now - Duration::seconds(1),
            dec!(1996),
            "test".to_string(),
            now - Duration::seconds(1),
        ));
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
// Position cleanup tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_position_clears_lifecycle() {
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

    // Remove position
    let removed = base.remove_position_by_token("token_cleanup").await;
    assert!(removed.is_some());

    // Lifecycle should be cleared
    let lifecycles = base.position_lifecycle.read().await;
    assert!(!lifecycles.contains_key("token_cleanup"));
}

// ---------------------------------------------------------------------------
// Reduce/remove position tests (Fix 3 partial fill)
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

    // Lifecycle should be cleaned up
    drop(positions);
    let lifecycles = base.position_lifecycle.read().await;
    assert!(!lifecycles.contains_key("t1"));
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

    // Lifecycle should be preserved (position still open)
    drop(positions);
    let lifecycles = base.position_lifecycle.read().await;
    assert!(
        lifecycles.contains_key("t1"),
        "Lifecycle preserved on partial close"
    );
}

// ---------------------------------------------------------------------------
// Recovery logic tests (Task 15)
// ---------------------------------------------------------------------------

/// Test set completion viability: entry 0.93 + other_ask 0.07 = 1.00 <= 1.01 → viable.
#[tokio::test]
async fn recovery_set_completion_viable_combined_cost_within_budget() {
    let base = make_base_with_market("m1", 300).await;

    // Create position with entry at 0.93
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.93),
        dec!(10),
        dec!(50000),
        dec!(0.93),
    );
    {
        let mut positions = base.positions.write().await;
        positions
            .entry("m1".to_string())
            .or_default()
            .push(pos.clone());
    }
    base.ensure_lifecycle("token_up").await;

    // Seed the opposite token's cached ask at 0.07
    {
        let mut asks = base.cached_asks.write().await;
        asks.insert("token_down".to_string(), dec!(0.07));
    }

    // Check: combined cost = 0.93 + 0.07 = 1.00 <= recovery_max_set_cost (1.01)
    let combined = pos.entry_price + dec!(0.07);
    assert!(
        combined <= base.config.stop_loss.recovery_max_set_cost,
        "Set completion should be viable: {} <= {}",
        combined,
        base.config.stop_loss.recovery_max_set_cost
    );

    // Verify opposite token lookup works
    let opposite = base.get_opposite_token("m1", "token_up").await;
    assert_eq!(opposite, Some("token_down".to_string()));
}

/// Test set completion not viable: entry 0.93 + other_ask 0.10 = 1.03 > 1.01 → skip.
#[tokio::test]
async fn recovery_set_completion_not_viable_combined_cost_exceeds_budget() {
    let base = make_base_with_market("m1", 300).await;

    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.93),
        dec!(10),
        dec!(50000),
        dec!(0.93),
    );

    // Set opposite token ask to 0.10 (combined 1.03 > 1.01)
    {
        let mut asks = base.cached_asks.write().await;
        asks.insert("token_down".to_string(), dec!(0.10));
    }

    let combined = pos.entry_price + dec!(0.10);
    assert!(
        combined > base.config.stop_loss.recovery_max_set_cost,
        "Set completion should NOT be viable: {} > {}",
        combined,
        base.config.stop_loss.recovery_max_set_cost
    );
}

/// Test opposite-side alpha recovery: momentum confirmed for 2 ticks → recovery is viable.
#[tokio::test]
async fn recovery_opposite_alpha_momentum_confirmed() {
    let base = make_base_with_market("m1", 300).await;
    let sl_config = &base.config.stop_loss;

    // Position: bought Up at 0.90, reference was 50000
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.90),
        dec!(10),
        dec!(50000),
        dec!(0.90),
    );

    // Seed price history with reversal: BTC dropped below reference for N ticks
    // For Up position, reversal means price < reference
    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        let now = Utc::now();
        let confirm_ticks = 2usize;
        for i in 0..confirm_ticks {
            let ts = now - Duration::seconds((confirm_ticks - i) as i64);
            entries.push_back((ts, dec!(49500), "test".to_string(), ts)); // Below 50000 reference = reversal for Up position
        }
        history.insert("BTC".to_string(), entries);
    }

    // Seed composite cache
    {
        let mut cache = base.sl_composite_cache.write().await;
        cache.insert(
            "BTC".to_string(),
            (
                CompositePriceResult {
                    price: dec!(49500),
                    sources_used: 2,
                    max_lag_ms: 100,
                    dispersion_bps: dec!(10),
                },
                Utc::now(),
            ),
        );
    }

    // Verify: all recent ticks show reversal (price < reference for Up)
    {
        let history = base.price_history.read().await;
        let entries = history.get("BTC").unwrap();
        let all_reversed = entries.iter().rev().take(2).all(|(_, price, _, _)| {
            // For Up position, reversal = price dropped below reference
            *price < pos.reference_price
        });
        assert!(all_reversed, "All recent ticks should show reversal");
    }

    // Verify the other side ask is within set-completion budget
    {
        let mut asks = base.cached_asks.write().await;
        asks.insert("token_down".to_string(), dec!(0.10));
    }
    let set_cost = pos.entry_price + dec!(0.10);
    assert!(
        set_cost <= sl_config.recovery_max_set_cost,
        "Set completion cost {} should be within budget {}",
        set_cost,
        sl_config.recovery_max_set_cost
    );
}

/// Test same-side re-entry blocked: recovery exit cooldown not elapsed → blocks re-entry.
#[tokio::test]
async fn recovery_same_side_reentry_blocked_during_cooldown() {
    let base = make_base_with_market("m1", 300).await;

    // Record a recovery exit cooldown for market m1
    base.record_recovery_exit_cooldown(&"m1".to_string()).await;

    // Verify cooldown is active
    let cooled = base.is_recovery_exit_cooled_down(&"m1".to_string()).await;
    assert!(
        cooled,
        "Recovery exit cooldown should be active immediately after recording"
    );
}

/// Test same-side re-entry allowed: cooldown elapsed → allows re-entry.
#[tokio::test]
async fn recovery_same_side_reentry_allowed_after_cooldown() {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    // Set very short cooldown for testing (uses stale_market_cooldown_secs)
    config.stop_loss.stale_market_cooldown_secs = 1;
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    // Set event time to 10 seconds ago
    let past = Utc::now() - Duration::seconds(10);
    *base.last_event_time.write().await = past;

    // Record cooldown (will expire at past + 1s, which is 9 seconds ago)
    {
        let expires_at = past + Duration::seconds(1);
        let mut cooldowns = base.recovery_exit_cooldowns.write().await;
        cooldowns.insert("m1".to_string(), expires_at);
    }

    // Now set event time to current time (well past the cooldown)
    *base.last_event_time.write().await = Utc::now();

    let cooled = base.is_recovery_exit_cooled_down(&"m1".to_string()).await;
    assert!(
        !cooled,
        "Recovery exit cooldown should have expired (1s cooldown, checked 10s later)"
    );
}

/// Test that recovery exit cooldown is recorded when position is resolved.
#[tokio::test]
async fn recovery_exit_records_cooldown_for_market() {
    let base = make_base_with_market("m1", 300).await;
    let m1 = "m1".to_string();
    let m2 = "m2".to_string();

    // No cooldown initially
    assert!(
        !base.is_recovery_exit_cooled_down(&m1).await,
        "No cooldown should exist initially"
    );

    // Record cooldown
    base.record_recovery_exit_cooldown(&m1).await;

    // Cooldown should be active
    assert!(
        base.is_recovery_exit_cooled_down(&m1).await,
        "Cooldown should be active after recording"
    );

    // Different market should not be affected
    assert!(
        !base.is_recovery_exit_cooled_down(&m2).await,
        "Different market should not be affected"
    );
}

// ── Proactive Hedge Tests ──────────────────────────────────────────────

/// Test hedge buy placed when set completion cost <= recovery_max_set_cost.
/// Combined: entry_price(0.93) + opposite_ask(0.07) = 1.00 <= 1.01
#[tokio::test]
async fn hedge_placed_when_set_completion_cost_within_budget() {
    let base = make_base_with_market("m1", 300).await;

    // Create position
    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.93),
        dec!(10),
        dec!(50000),
        dec!(0.93),
    );
    {
        let mut positions = base.positions.write().await;
        positions
            .entry("m1".to_string())
            .or_default()
            .push(pos.clone());
    }
    base.ensure_lifecycle("token_up").await;

    // Seed opposite ask at 0.07 (combined 1.00 <= 1.01)
    {
        let mut asks = base.cached_asks.write().await;
        asks.insert("token_down".to_string(), dec!(0.07));
    }

    let strategy = TailEndStrategy::new(base.clone());
    let now = Utc::now();
    let result = strategy.evaluate_hedge(&pos, pos.size, false, now).await;

    assert!(
        result.is_some(),
        "Hedge should be placed when combined cost {} <= {}",
        pos.entry_price + dec!(0.07),
        base.config.stop_loss.recovery_max_set_cost
    );

    let (action, hedge_oid, hedge_price) = result.unwrap();
    assert!(matches!(action, Action::PlaceOrder(_)));
    assert!(hedge_oid.starts_with("hedge-"));
    assert_eq!(hedge_price, dec!(0.07));

    // Verify exit order meta was stored for fill routing
    let exit_orders = base.exit_orders_by_id.read().await;
    let hedge_meta = exit_orders.get(&hedge_oid).unwrap();
    assert!(hedge_meta.source_state.starts_with("Hedge"));
    assert_eq!(hedge_meta.order_token_id, "token_down");
    assert_eq!(hedge_meta.token_id, "token_up");
}

/// Test hedge skipped when combined cost > recovery_max_set_cost.
/// Combined: entry_price(0.93) + opposite_ask(0.10) = 1.03 > 1.01
#[tokio::test]
async fn hedge_skipped_when_cost_exceeds_threshold() {
    let base = make_base_with_market("m1", 300).await;

    let pos = make_position(
        "m1",
        "token_up",
        OutcomeSide::Up,
        dec!(0.93),
        dec!(10),
        dec!(50000),
        dec!(0.93),
    );
    {
        let mut positions = base.positions.write().await;
        positions
            .entry("m1".to_string())
            .or_default()
            .push(pos.clone());
    }

    // Seed opposite ask at 0.10 (combined 1.03 > 1.01)
    {
        let mut asks = base.cached_asks.write().await;
        asks.insert("token_down".to_string(), dec!(0.10));
    }

    let strategy = TailEndStrategy::new(base.clone());
    let now = Utc::now();
    let result = strategy.evaluate_hedge(&pos, pos.size, false, now).await;

    assert!(
        result.is_none(),
        "Hedge should be skipped when combined cost {} > {}",
        pos.entry_price + dec!(0.10),
        base.config.stop_loss.recovery_max_set_cost
    );
}

// ===========================================================================
// Inline tests from tailend/mod.rs
// ===========================================================================

// ---------------------------------------------------------------------------
// TailEnd-specific test helpers
// ---------------------------------------------------------------------------

fn make_tailend_market_info(id: &str, end_date: chrono::DateTime<Utc>) -> MarketInfo {
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
        min_order_size: dec!(5.0),
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
    }
}

async fn make_tailend_strategy(time_remaining: i64) -> (TailEndStrategy, StrategyContext) {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.min_sustained_secs = 5; // Small window to keep test simple
    config.tailend.max_recent_volatility = dec!(1.0); // Disable volatility filter
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    let market = MarketWithReference {
        market: make_tailend_market_info("market1", Utc::now() + Duration::seconds(time_remaining)),
        reference_price: dec!(50000),
        reference_quality: ReferenceQuality::Exact,
        discovery_time: Utc::now(),
        coin: "BTC".to_string(),
        window_ts: 0,
    };
    base.active_markets
        .write()
        .await
        .insert("market1".to_string(), market);

    // Populate price history so sustained direction check passes.
    // Use timestamps spread over last 5s to establish direction.
    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        let now = Utc::now();
        // BTC above reference (51000 > 50000) — favors Up direction
        entries.push_back((
            now - Duration::seconds(3),
            dec!(51000),
            "test".to_string(),
            now - Duration::seconds(3),
        ));
        entries.push_back((
            now - Duration::seconds(1),
            dec!(51000),
            "test".to_string(),
            now - Duration::seconds(1),
        ));
        history.insert("BTC".to_string(), entries);
    }

    let ctx = StrategyContext::new();
    let strategy = TailEndStrategy::new(base);
    (strategy, ctx)
}

#[tokio::test]
async fn tailend_generates_order_within_window() {
    let (strategy, ctx) = make_tailend_strategy(60).await;

    // Set up orderbook with ask >= threshold (0.93 at 60s), tight spread
    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks.insert(
            "token_up".to_string(),
            OrderbookSnapshot {
                token_id: "token_up".to_string(),
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

    // BTC price above reference → predicts Up → token_up
    let opp = strategy
        .evaluate_opportunity(&"market1".to_string(), dec!(51000), &ctx)
        .await;
    assert!(opp.is_some());
    let opp = opp.unwrap();
    assert_eq!(opp.token_id, "token_up");
    assert_eq!(opp.buy_price, dec!(0.94));
}

#[tokio::test]
async fn tailend_skips_outside_window() {
    let (strategy, ctx) = make_tailend_strategy(200).await; // > 120s threshold

    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks.insert(
            "token_up".to_string(),
            OrderbookSnapshot {
                token_id: "token_up".to_string(),
                bids: vec![OrderbookLevel {
                    price: dec!(0.92),
                    size: dec!(100),
                }],
                asks: vec![OrderbookLevel {
                    price: dec!(0.95),
                    size: dec!(100),
                }],
                timestamp: Utc::now(),
            },
        );
    }

    let opp = strategy
        .evaluate_opportunity(&"market1".to_string(), dec!(51000), &ctx)
        .await;
    assert!(opp.is_none());
}

#[tokio::test]
async fn tailend_skips_below_threshold() {
    let (strategy, ctx) = make_tailend_strategy(60).await;

    // At 60s, dynamic threshold is 0.93. Set ask to 0.89 (below threshold).
    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks.insert(
            "token_up".to_string(),
            OrderbookSnapshot {
                token_id: "token_up".to_string(),
                bids: vec![OrderbookLevel {
                    price: dec!(0.87),
                    size: dec!(100),
                }],
                asks: vec![OrderbookLevel {
                    price: dec!(0.89),
                    size: dec!(100),
                }],
                timestamp: Utc::now(),
            },
        );
    }

    let opp = strategy
        .evaluate_opportunity(&"market1".to_string(), dec!(51000), &ctx)
        .await;
    assert!(opp.is_none());
}

#[tokio::test]
async fn tailend_dynamic_threshold_tightens() {
    let strategy_constructor = |time: i64| async move {
        let (s, _) = make_tailend_strategy(time).await;
        s
    };

    let s120 = strategy_constructor(120).await;
    let s30 = strategy_constructor(30).await;

    let t120 = s120.get_ask_threshold(120);
    let t30 = s30.get_ask_threshold(30);

    // At 120s → 0.90, at 30s → 0.95
    assert_eq!(t120, dec!(0.90));
    assert_eq!(t30, dec!(0.95));
    assert!(t30 > t120);
}

#[tokio::test]
async fn tailend_respects_max_spread() {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.min_sustained_secs = 0;
    config.tailend.max_recent_volatility = dec!(1.0);
    config.tailend.max_spread_bps = dec!(50); // 50 bps = 0.5%
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    let market = MarketWithReference {
        market: make_tailend_market_info("market1", Utc::now() + Duration::seconds(60)),
        reference_price: dec!(50000),
        reference_quality: ReferenceQuality::Exact,
        discovery_time: Utc::now(),
        coin: "BTC".to_string(),
        window_ts: 0,
    };
    base.active_markets
        .write()
        .await
        .insert("market1".to_string(), market);

    let ctx = StrategyContext::new();
    let strategy = TailEndStrategy::new(base);

    // Wide spread: bid=0.90, ask=0.95 → spread=5.4% >> 0.5%
    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks.insert(
            "token_up".to_string(),
            OrderbookSnapshot {
                token_id: "token_up".to_string(),
                bids: vec![OrderbookLevel {
                    price: dec!(0.90),
                    size: dec!(100),
                }],
                asks: vec![OrderbookLevel {
                    price: dec!(0.95),
                    size: dec!(100),
                }],
                timestamp: Utc::now(),
            },
        );
    }

    let opp = strategy
        .evaluate_opportunity(&"market1".to_string(), dec!(51000), &ctx)
        .await;
    assert!(opp.is_none());
}

#[tokio::test]
async fn tailend_pending_order_stores_aggressive_price() {
    let (strategy, ctx) = make_tailend_strategy(60).await;

    // Set up orderbook: ask=0.94, bid=0.935, depth=100
    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks.insert(
            "token_up".to_string(),
            OrderbookSnapshot {
                token_id: "token_up".to_string(),
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

    // Trigger entry via external price
    let actions = strategy
        .handle_external_price("BTC", dec!(51000), "test", ctx.now().await, &ctx)
        .await;

    // Should have produced a PlaceOrder action
    assert!(
        actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
        "Expected PlaceOrder action"
    );

    // Verify pending order stores aggressive_price (ask + 1 tick = 0.95), not buy_price (0.94)
    let pending = strategy.base.pending_orders.read().await;
    let po = pending.get("token_up").expect("pending order for token_up");
    let expected_aggressive = dec!(0.95); // 0.94 + 0.01 * 1 tick step
    assert_eq!(
        po.price, expected_aggressive,
        "PendingOrder.price should be aggressive_price ({expected_aggressive}), got {}",
        po.price
    );
}

#[tokio::test]
async fn tailend_partially_filled_updates_limit_order_size() {
    let (strategy, ctx) = make_tailend_strategy(60).await;

    // Seed an open limit order as if a GTC was placed
    {
        let mut limits = strategy.base.open_limit_orders.write().await;
        limits.insert(
            "order123".to_string(),
            OpenLimitOrder {
                order_id: "order123".to_string(),
                market_id: "market1".to_string(),
                token_id: "token_up".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.95),
                size: dec!(10),
                reference_price: dec!(50000),
                coin: "BTC".to_string(),
                placed_at: Utc::now(),
                kelly_fraction: None,
                estimated_fee: dec!(0.001),
                tick_size: dec!(0.01),
                fee_rate_bps: 0,
                cancel_pending: false,
                reconcile_miss_count: 0,
            },
        );
    }

    // Simulate a PartiallyFilled event
    let event = Event::OrderUpdate(polyrust_core::events::OrderEvent::PartiallyFilled {
        order_id: "order123".to_string(),
        filled_size: dec!(4),
        remaining_size: dec!(6),
    });

    let mut strategy_mut = strategy;
    let actions = strategy_mut.on_event(&event, &ctx).await.unwrap();
    assert!(actions.iter().all(|a| !matches!(a, Action::PlaceOrder(_))));

    // Verify size updated to remaining
    let limits = strategy_mut.base.open_limit_orders.read().await;
    let lo = limits.get("order123").expect("limit order still present");
    assert_eq!(lo.size, dec!(6), "size should be updated to remaining_size");
}

// --- PnL entry fee bug fix tests ---

/// GTC entry (maker, 0% fee) + FOK exit (taker fee on exit only).
/// Entry fee must be 0, only exit taker fee is deducted.
#[test]
fn pnl_gtc_entry_fok_exit_entry_fee_is_zero() {
    use crate::crypto_arb::services::taker_fee;

    let entry_price = dec!(0.92);
    let exit_price = dec!(0.85);
    let size = dec!(100);
    let fee_rate = dec!(0.0315);

    // GTC entry → entry_fee_per_share = 0
    let entry_fee_per_share = Decimal::ZERO;
    let exit_fee = taker_fee(exit_price, fee_rate);

    let pnl = (exit_price - entry_price) * size - (entry_fee_per_share * size) - (exit_fee * size);

    let expected_exit_fee = taker_fee(dec!(0.85), fee_rate);
    let expected = (dec!(0.85) - dec!(0.92)) * dec!(100) - expected_exit_fee * dec!(100);
    assert_eq!(pnl, expected);
    assert_eq!(entry_fee_per_share * size, Decimal::ZERO);
}

/// GTC entry + GTC exit → both fees = 0.
#[test]
fn pnl_gtc_entry_gtc_exit_both_fees_zero() {
    let entry_price = dec!(0.93);
    let exit_price = dec!(0.88);
    let size = dec!(50);

    let entry_fee_per_share = Decimal::ZERO;
    let pnl = (exit_price - entry_price) * size - (entry_fee_per_share * size);

    let expected = (dec!(0.88) - dec!(0.93)) * dec!(50);
    assert_eq!(pnl, expected);
    assert_eq!(pnl, dec!(-2.5));
}

/// FOK entry (taker fee) + FOK exit (taker fee) → both fees deducted.
#[test]
fn pnl_fok_entry_fok_exit_both_fees_deducted() {
    use crate::crypto_arb::services::taker_fee;

    let entry_price = dec!(0.94);
    let exit_price = dec!(0.90);
    let size = dec!(100);
    let fee_rate = dec!(0.0315);

    let entry_fee_per_share = taker_fee(entry_price, fee_rate);
    let exit_fee = taker_fee(exit_price, fee_rate);

    let pnl = (exit_price - entry_price) * size - (entry_fee_per_share * size) - (exit_fee * size);

    let expected_entry = taker_fee(dec!(0.94), fee_rate);
    let expected_exit = taker_fee(dec!(0.90), fee_rate);
    let expected = (dec!(0.90) - dec!(0.94)) * dec!(100)
        - expected_entry * dec!(100)
        - expected_exit * dec!(100);
    assert_eq!(pnl, expected);
    assert!(entry_fee_per_share > Decimal::ZERO);
    assert!(exit_fee > Decimal::ZERO);
}

/// Market expiry with GTC entry: winning outcome → entry fee = 0.
#[test]
fn pnl_market_expiry_gtc_entry_win() {
    let entry_price = dec!(0.90);
    let size = dec!(100);
    let entry_fee_per_share = Decimal::ZERO;

    let pnl = (Decimal::ONE - entry_price) * size - (entry_fee_per_share * size);
    assert_eq!(pnl, dec!(10));
}

/// Market expiry with GTC entry: losing outcome → entry fee = 0.
#[test]
fn pnl_market_expiry_gtc_entry_loss() {
    let entry_price = dec!(0.90);
    let size = dec!(100);
    let entry_fee_per_share = Decimal::ZERO;

    let pnl = -(entry_price * size) - (entry_fee_per_share * size);
    assert_eq!(pnl, dec!(-90));
}

/// Market expiry with FOK entry: taker fee deducted from outcome.
#[test]
fn pnl_market_expiry_fok_entry_win() {
    use crate::crypto_arb::services::taker_fee;

    let entry_price = dec!(0.92);
    let size = dec!(100);
    let fee_rate = dec!(0.0315);
    let entry_fee_per_share = taker_fee(entry_price, fee_rate);

    let pnl = (Decimal::ONE - entry_price) * size - (entry_fee_per_share * size);

    let expected = dec!(8) - taker_fee(dec!(0.92), fee_rate) * dec!(100);
    assert_eq!(pnl, expected);
    assert!(pnl > Decimal::ZERO);
    assert!(pnl < dec!(8));
}

// --- PnL exit price bug fix tests ---

#[test]
fn pnl_fok_exit_uses_actual_fill_price_not_trigger_bid() {
    use crate::crypto_arb::services::taker_fee;

    let entry_price = dec!(0.95);
    let _trigger_bid = dec!(0.92);
    let actual_fill_price = dec!(0.93);
    let size = dec!(100);
    let fee_rate = dec!(0.0315);
    let entry_fee_per_share = Decimal::ZERO;

    let exit_fee = taker_fee(actual_fill_price, fee_rate);
    let correct_pnl =
        (actual_fill_price - entry_price) * size - (entry_fee_per_share * size) - (exit_fee * size);

    let wrong_exit_fee = taker_fee(dec!(0.92), fee_rate);
    let wrong_pnl =
        (dec!(0.92) - entry_price) * size - (entry_fee_per_share * size) - (wrong_exit_fee * size);

    assert!(correct_pnl > wrong_pnl);
    assert!(correct_pnl - wrong_pnl > dec!(0.5));
    assert!(correct_pnl < Decimal::ZERO);
    assert!(wrong_pnl < Decimal::ZERO);
}

#[test]
fn pnl_fok_exit_same_trigger_and_fill_price() {
    use crate::crypto_arb::services::taker_fee;

    let entry_price = dec!(0.95);
    let fill_price = dec!(0.90);
    let size = dec!(50);
    let fee_rate = dec!(0.0315);
    let entry_fee_per_share = Decimal::ZERO;

    let exit_fee = taker_fee(fill_price, fee_rate);
    let pnl = (fill_price - entry_price) * size - (entry_fee_per_share * size) - (exit_fee * size);

    let expected =
        (dec!(0.90) - dec!(0.95)) * dec!(50) - taker_fee(dec!(0.90), fee_rate) * dec!(50);
    assert_eq!(pnl, expected);
    assert!(pnl < Decimal::ZERO);
}

// -----------------------------------------------------------------------
// Lifecycle-driven stop-loss evaluation tests
// -----------------------------------------------------------------------

async fn make_lifecycle_test_setup(
    entry_time_offset_secs: i64,
    time_remaining_secs: i64,
) -> (TailEndStrategy, OrderbookSnapshot) {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.min_sustained_secs = 0;
    config.tailend.max_recent_volatility = dec!(1.0);
    config.stop_loss.hard_drop_abs = dec!(0.08);
    config.stop_loss.hard_reversal_pct = dec!(0.006);
    config.stop_loss.dual_trigger_consecutive_ticks = 2;
    config.stop_loss.reversal_pct = dec!(0.003);
    config.stop_loss.min_drop = dec!(0.05);
    config.stop_loss.sl_max_book_age_ms = 5000;
    config.stop_loss.sl_max_external_age_ms = 5000;
    config.stop_loss.sl_min_sources = 1;
    config.stop_loss.sl_max_dispersion_bps = dec!(100);
    config.tailend.min_sell_delay_secs = 10;
    config.tailend.post_entry_window_secs = 20;
    config.tailend.post_entry_exit_drop = dec!(0.04);
    config.stop_loss.min_remaining_secs = 0;
    config.stop_loss.exit_depth_cap_factor = dec!(0.80);

    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    let now = Utc::now();
    let end_date = now + Duration::seconds(time_remaining_secs);

    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "market1".to_string(),
            MarketWithReference {
                market: make_tailend_market_info("market1", end_date),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Exact,
                discovery_time: now,
                coin: "BTC".to_string(),
                window_ts: 0,
            },
        );
    }

    {
        let mut history = base.price_history.write().await;
        let mut entries = std::collections::VecDeque::new();
        entries.push_back((now, dec!(49700), "test".to_string(), now));
        history.insert("BTC".to_string(), entries);
    }

    {
        let mut cache = base.sl_composite_cache.write().await;
        cache.insert(
            "BTC".to_string(),
            (
                CompositePriceResult {
                    price: dec!(49700),
                    sources_used: 2,
                    max_lag_ms: 100,
                    dispersion_bps: dec!(5),
                },
                now,
            ),
        );
    }

    let entry_time = now - Duration::seconds(entry_time_offset_secs);
    let pos = ArbitragePosition {
        market_id: "market1".to_string(),
        token_id: "token_up".to_string(),
        side: OutcomeSide::Up,
        entry_price: dec!(0.90),
        size: dec!(10),
        reference_price: dec!(50000),
        coin: "BTC".to_string(),
        order_id: None,
        entry_time,
        kelly_fraction: None,
        peak_price: dec!(0.90),
        estimated_fee: Decimal::ZERO,
        entry_market_price: dec!(0.90),
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
        entry_order_type: OrderType::Gtc,
        entry_fee_per_share: Decimal::ZERO,
        recovery_cost: Decimal::ZERO,
    };
    base.record_position(pos).await;

    let strategy = TailEndStrategy::new(base);

    let snapshot = OrderbookSnapshot {
        token_id: "token_up".to_string(),
        bids: vec![OrderbookLevel {
            price: dec!(0.82),
            size: dec!(100),
        }],
        asks: vec![OrderbookLevel {
            price: dec!(0.85),
            size: dec!(100),
        }],
        timestamp: now,
    };

    (strategy, snapshot)
}

#[tokio::test]
async fn lifecycle_trigger_transitions_to_exit_executing() {
    let (strategy, snapshot) = make_lifecycle_test_setup(20, 300).await;

    let actions = strategy.handle_orderbook_update(&snapshot).await;

    assert!(
        actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
        "Expected PlaceOrder action for stop-loss exit, got: {actions:?}"
    );

    let lifecycles = strategy.base.position_lifecycle.read().await;
    let lc = lifecycles
        .get("token_up")
        .expect("lifecycle for token_up should exist");
    assert!(
        matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }),
        "Expected ExitExecuting, got: {:?}",
        lc.state
    );

    let exit_orders = strategy.base.exit_orders_by_id.read().await;
    assert!(
        !exit_orders.is_empty(),
        "exit_orders_by_id should have the exit order meta"
    );
}

#[tokio::test]
async fn lifecycle_non_hard_trigger_during_sell_delay_skips() {
    let (strategy, _snapshot) = make_lifecycle_test_setup(5, 300).await;

    {
        let mut cache = strategy.base.sl_composite_cache.write().await;
        cache.clear();
    }
    {
        let mut history = strategy.base.price_history.write().await;
        history.clear();
    }

    let snapshot = OrderbookSnapshot {
        token_id: "token_up".to_string(),
        bids: vec![OrderbookLevel {
            price: dec!(0.85),
            size: dec!(100),
        }],
        asks: vec![OrderbookLevel {
            price: dec!(0.87),
            size: dec!(100),
        }],
        timestamp: Utc::now(),
    };

    let actions = strategy.handle_orderbook_update(&snapshot).await;

    assert!(
        !actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
        "Should not sell during sell delay for non-hard trigger, got: {actions:?}"
    );

    let lifecycles = strategy.base.position_lifecycle.read().await;
    let lc = lifecycles.get("token_up").expect("lifecycle should exist");
    assert!(
        matches!(lc.state, PositionLifecycleState::Healthy),
        "Expected Healthy during sell delay (non-hard trigger skips), got: {:?}",
        lc.state
    );
}

#[tokio::test]
async fn lifecycle_hard_crash_bypasses_sell_delay() {
    let (strategy, snapshot) = make_lifecycle_test_setup(5, 300).await;

    let actions = strategy.handle_orderbook_update(&snapshot).await;

    assert!(
        actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
        "Hard crash should bypass sell delay and produce exit, got: {actions:?}"
    );

    let lifecycles = strategy.base.position_lifecycle.read().await;
    let lc = lifecycles.get("token_up").expect("lifecycle should exist");
    assert!(
        matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }),
        "Expected ExitExecuting after hard crash bypass, got: {:?}",
        lc.state
    );
}

#[tokio::test]
async fn lifecycle_post_entry_trigger_fires_when_sellable() {
    let (strategy, _snapshot) = make_lifecycle_test_setup(20, 300).await;

    let snapshot = OrderbookSnapshot {
        token_id: "token_up".to_string(),
        bids: vec![OrderbookLevel {
            price: dec!(0.77),
            size: dec!(100),
        }],
        asks: vec![OrderbookLevel {
            price: dec!(0.79),
            size: dec!(100),
        }],
        timestamp: Utc::now(),
    };

    let actions = strategy.handle_orderbook_update(&snapshot).await;

    assert!(
        actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
        "Should sell when delay elapsed and trigger fires, got: {actions:?}"
    );

    let lifecycles = strategy.base.position_lifecycle.read().await;
    let lc = lifecycles.get("token_up").unwrap();
    assert!(
        matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }),
        "Expected ExitExecuting, got: {:?}",
        lc.state
    );
}

async fn make_exit_executing_setup() -> TailEndStrategy {
    let (strategy, snapshot) = make_lifecycle_test_setup(20, 300).await;
    let _actions = strategy.handle_orderbook_update(&snapshot).await;
    {
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").unwrap();
        assert!(matches!(
            lc.state,
            PositionLifecycleState::ExitExecuting { .. }
        ));
    }
    strategy
}

#[tokio::test]
async fn lifecycle_fak_rejected_transitions_to_healthy() {
    let strategy = make_exit_executing_setup().await;

    let exit_oid = {
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").unwrap();
        match &lc.state {
            PositionLifecycleState::ExitExecuting { order_id, .. } => order_id.clone(),
            other => panic!("Expected ExitExecuting, got: {other:?}"),
        }
    };

    let ctx = StrategyContext::new();
    let event = Event::OrderUpdate(polyrust_core::events::OrderEvent::Rejected {
        order_id: Some(exit_oid.clone()),
        token_id: Some("token_up".to_string()),
        reason: "couldn't be fully filled".to_string(),
    });
    let mut strategy_mut = strategy;
    let _actions = strategy_mut.on_event(&event, &ctx).await.unwrap();

    let lifecycles = strategy_mut.base.position_lifecycle.read().await;
    let lc = lifecycles.get("token_up").unwrap();
    assert!(
        matches!(lc.state, PositionLifecycleState::Healthy),
        "Expected Healthy after FAK rejection, got: {:?}",
        lc.state
    );

    let exit_orders = strategy_mut.base.exit_orders_by_id.read().await;
    let has_token = exit_orders.values().any(|m| m.token_id == "token_up");
    assert!(
        !has_token,
        "exit_orders_by_id should be cleaned up after rejection"
    );
}

#[tokio::test]
async fn lifecycle_gtc_refresh_cancels_stale_order() {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.min_sustained_secs = 0;
    config.tailend.max_recent_volatility = dec!(1.0);
    config.stop_loss.min_remaining_secs = 0;
    config.stop_loss.exit_depth_cap_factor = dec!(0.80);

    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));
    let now = Utc::now();

    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "market1".to_string(),
            MarketWithReference {
                market: make_tailend_market_info("market1", now + Duration::seconds(300)),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Exact,
                discovery_time: now,
                coin: "BTC".to_string(),
                window_ts: 0,
            },
        );
    }

    let pos = ArbitragePosition {
        market_id: "market1".to_string(),
        token_id: "token_up".to_string(),
        side: OutcomeSide::Up,
        entry_price: dec!(0.90),
        size: dec!(10),
        reference_price: dec!(50000),
        coin: "BTC".to_string(),
        order_id: None,
        entry_time: now - Duration::seconds(20),
        kelly_fraction: None,
        peak_price: dec!(0.90),
        estimated_fee: Decimal::ZERO,
        entry_market_price: dec!(0.90),
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
        entry_order_type: OrderType::Gtc,
        entry_fee_per_share: Decimal::ZERO,
        recovery_cost: Decimal::ZERO,
    };
    base.record_position(pos).await;

    let exit_oid = "exit-gtc-token_up-12345".to_string();
    {
        let mut lifecycle = base.ensure_lifecycle("token_up").await;
        lifecycle
            .transition(
                PositionLifecycleState::ExitExecuting {
                    order_id: exit_oid.clone(),
                    order_type: OrderType::Gtc,
                    exit_price: dec!(0.81),
                    submitted_at: now - Duration::seconds(3),
                    hedge_order_id: None,
                    hedge_price: None,
                },
                "test setup",
                now,
            )
            .unwrap();
        lifecycle.pending_exit_order_id = Some(exit_oid.clone());
        let mut lifecycles = base.position_lifecycle.write().await;
        lifecycles.insert("token_up".to_string(), lifecycle);
    }

    {
        let mut exit_orders = base.exit_orders_by_id.write().await;
        exit_orders.insert(
            exit_oid.clone(),
            ExitOrderMeta {
                token_id: "token_up".to_string(),
                order_token_id: "token_up".to_string(),
                order_type: OrderType::Gtc,
                source_state: "ExitActive(GTC residual)".to_string(),
                exit_price: dec!(0.81),
                clip_size: dec!(10),
            },
        );
    }

    let strategy = TailEndStrategy::new(base);

    let snapshot = OrderbookSnapshot {
        token_id: "token_up".to_string(),
        bids: vec![OrderbookLevel {
            price: dec!(0.82),
            size: dec!(100),
        }],
        asks: vec![OrderbookLevel {
            price: dec!(0.85),
            size: dec!(100),
        }],
        timestamp: now,
    };

    let actions = strategy.handle_orderbook_update(&snapshot).await;

    let has_cancel = actions
        .iter()
        .any(|a| matches!(a, Action::CancelOrder(oid) if oid == &exit_oid));
    assert!(
        has_cancel,
        "Expected CancelOrder for stale GTC exit, got: {actions:?}"
    );
}

#[tokio::test]
async fn lifecycle_partial_fill_places_gtc_residual() {
    let strategy = make_exit_executing_setup().await;

    let exit_oid = {
        let lifecycles = strategy.base.position_lifecycle.read().await;
        let lc = lifecycles.get("token_up").unwrap();
        match &lc.state {
            PositionLifecycleState::ExitExecuting { order_id, .. } => order_id.clone(),
            other => panic!("Expected ExitExecuting, got: {other:?}"),
        }
    };

    let actions = strategy
        .on_order_filled(&exit_oid, "token_up", dec!(0.82), dec!(5))
        .await;

    let positions = strategy.base.positions.read().await;
    let pos = positions
        .values()
        .flat_map(|v| v.iter())
        .find(|p| p.token_id == "token_up");
    assert!(
        pos.is_some(),
        "Position should still exist after partial fill"
    );
    assert_eq!(pos.unwrap().size, dec!(5), "Size should be reduced to 5");
    drop(positions);

    let lifecycles = strategy.base.position_lifecycle.read().await;
    let lc = lifecycles.get("token_up").unwrap();
    match &lc.state {
        PositionLifecycleState::ExitExecuting { order_type, .. } => {
            assert_eq!(
                *order_type,
                OrderType::Gtc,
                "Residual should use GTC order type"
            );
        }
        other => panic!("Expected ExitExecuting(GTC) for residual, got: {other:?}"),
    }

    let has_place = actions.iter().any(|a| matches!(a, Action::PlaceOrder(_)));
    assert!(
        has_place,
        "Expected PlaceOrder for GTC residual after partial fill, got: {actions:?}"
    );
}

// -----------------------------------------------------------------------
// Task 16: Order event routing through lifecycle transitions
// -----------------------------------------------------------------------

async fn make_exit_fill_test_setup(exit_order_type: OrderType) -> (TailEndStrategy, String) {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.min_sustained_secs = 0;
    config.tailend.max_recent_volatility = dec!(1.0);
    config.stop_loss.min_remaining_secs = 0;
    config.stop_loss.exit_depth_cap_factor = dec!(0.80);

    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));
    let now = Utc::now();

    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "market1".to_string(),
            MarketWithReference {
                market: make_tailend_market_info("market1", now + Duration::seconds(300)),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Exact,
                discovery_time: now,
                coin: "BTC".to_string(),
                window_ts: 0,
            },
        );
    }

    let pos = ArbitragePosition {
        market_id: "market1".to_string(),
        token_id: "token_up".to_string(),
        side: OutcomeSide::Up,
        entry_price: dec!(0.92),
        size: dec!(10),
        reference_price: dec!(50000),
        coin: "BTC".to_string(),
        order_id: None,
        entry_time: now - Duration::seconds(20),
        kelly_fraction: None,
        peak_price: dec!(0.92),
        estimated_fee: Decimal::ZERO,
        entry_market_price: dec!(0.92),
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
        entry_order_type: OrderType::Gtc,
        entry_fee_per_share: Decimal::ZERO,
        recovery_cost: Decimal::ZERO,
    };
    base.record_position(pos).await;

    let exit_oid = format!(
        "exit-{}-token_up-{}",
        if exit_order_type == OrderType::Gtc {
            "gtc"
        } else {
            "fak"
        },
        now.timestamp_millis()
    );

    {
        let mut lifecycle = base.ensure_lifecycle("token_up").await;
        lifecycle
            .transition(
                PositionLifecycleState::ExitExecuting {
                    order_id: exit_oid.clone(),
                    order_type: exit_order_type,
                    exit_price: dec!(0.85),
                    submitted_at: now,
                    hedge_order_id: None,
                    hedge_price: None,
                },
                "test setup: trigger fired",
                now,
            )
            .unwrap();
        lifecycle.pending_exit_order_id = Some(exit_oid.clone());
        let mut lifecycles = base.position_lifecycle.write().await;
        lifecycles.insert("token_up".to_string(), lifecycle);
    }

    {
        let mut exit_orders = base.exit_orders_by_id.write().await;
        exit_orders.insert(
            exit_oid.clone(),
            ExitOrderMeta {
                token_id: "token_up".to_string(),
                order_token_id: "token_up".to_string(),
                order_type: exit_order_type,
                source_state: "HardCrash(bid_drop=0.08, reversal=0.006)".to_string(),
                exit_price: dec!(0.85),
                clip_size: dec!(10),
            },
        );
    }

    let strategy = TailEndStrategy::new(base);
    (strategy, exit_oid)
}

#[tokio::test]
async fn lifecycle_exit_fill_routes_through_lifecycle_fak() {
    let (strategy, exit_oid) = make_exit_fill_test_setup(OrderType::Fak).await;

    let actions = strategy
        .on_order_filled(&exit_oid, "token_up", dec!(0.85), dec!(10))
        .await;

    assert!(
        actions.is_empty() || !actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
        "Should not produce further orders after full exit fill"
    );

    let positions = strategy.base.positions.read().await;
    let has_position = positions
        .values()
        .flat_map(|v| v.iter())
        .any(|p| p.token_id == "token_up");
    assert!(
        !has_position,
        "Position should be removed after full exit fill"
    );

    let lifecycles = strategy.base.position_lifecycle.read().await;
    assert!(
        !lifecycles.contains_key("token_up"),
        "Lifecycle should be removed after full exit fill"
    );

    let exit_orders = strategy.base.exit_orders_by_id.read().await;
    let has_token = exit_orders.values().any(|m| m.token_id == "token_up");
    assert!(
        !has_token,
        "exit_orders_by_id should be cleaned up after full fill"
    );
}

#[tokio::test]
async fn lifecycle_exit_fill_routes_through_lifecycle_gtc() {
    let (strategy, exit_oid) = make_exit_fill_test_setup(OrderType::Gtc).await;

    let actions = strategy
        .on_order_filled(&exit_oid, "token_up", dec!(0.88), dec!(10))
        .await;

    assert!(actions.is_empty(), "Should not produce further orders");

    let positions = strategy.base.positions.read().await;
    let has_position = positions
        .values()
        .flat_map(|v| v.iter())
        .any(|p| p.token_id == "token_up");
    assert!(
        !has_position,
        "Position should be removed after GTC exit fill"
    );

    let lifecycles = strategy.base.position_lifecycle.read().await;
    assert!(
        !lifecycles.contains_key("token_up"),
        "Lifecycle should be removed after GTC full fill"
    );
}

#[tokio::test]
async fn lifecycle_partial_exit_fill_dust_removed() {
    let (strategy, exit_oid) = make_exit_fill_test_setup(OrderType::Fak).await;

    let _actions = strategy
        .on_order_filled(&exit_oid, "token_up", dec!(0.85), dec!(6))
        .await;

    let positions = strategy.base.positions.read().await;
    let has_position = positions
        .values()
        .flat_map(|v| v.iter())
        .any(|p| p.token_id == "token_up");
    assert!(
        !has_position,
        "Dust position (4 < min_order_size 5) should be removed"
    );
}

#[tokio::test]
async fn lifecycle_partial_exit_fill_above_min_places_gtc_residual() {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.min_sustained_secs = 0;
    config.tailend.max_recent_volatility = dec!(1.0);
    config.stop_loss.min_remaining_secs = 0;
    config.stop_loss.exit_depth_cap_factor = dec!(0.80);

    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));
    let now = Utc::now();

    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "market1".to_string(),
            MarketWithReference {
                market: make_tailend_market_info("market1", now + Duration::seconds(300)),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Exact,
                discovery_time: now,
                coin: "BTC".to_string(),
                window_ts: 0,
            },
        );
    }

    let pos = ArbitragePosition {
        market_id: "market1".to_string(),
        token_id: "token_up".to_string(),
        side: OutcomeSide::Up,
        entry_price: dec!(0.92),
        size: dec!(20),
        reference_price: dec!(50000),
        coin: "BTC".to_string(),
        order_id: None,
        entry_time: now - Duration::seconds(20),
        kelly_fraction: None,
        peak_price: dec!(0.92),
        estimated_fee: Decimal::ZERO,
        entry_market_price: dec!(0.92),
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
        entry_order_type: OrderType::Gtc,
        entry_fee_per_share: Decimal::ZERO,
        recovery_cost: Decimal::ZERO,
    };
    base.record_position(pos).await;

    {
        let mut lifecycle = base.ensure_lifecycle("token_up").await;
        lifecycle
            .transition(
                PositionLifecycleState::ExitExecuting {
                    order_id: "exit-fak-1".to_string(),
                    order_type: OrderType::Fak,
                    exit_price: dec!(0.85),
                    submitted_at: now,
                    hedge_order_id: None,
                    hedge_price: None,
                },
                "test trigger",
                now,
            )
            .unwrap();
        lifecycle.pending_exit_order_id = Some("exit-fak-1".to_string());
        let mut lifecycles = base.position_lifecycle.write().await;
        lifecycles.insert("token_up".to_string(), lifecycle);
    }
    {
        let mut exit_orders = base.exit_orders_by_id.write().await;
        exit_orders.insert(
            "exit-fak-1".to_string(),
            ExitOrderMeta {
                token_id: "token_up".to_string(),
                order_token_id: "token_up".to_string(),
                order_type: OrderType::Fak,
                source_state: "test".to_string(),
                exit_price: dec!(0.85),
                clip_size: dec!(10),
            },
        );
    }

    let strategy = TailEndStrategy::new(base);

    let actions = strategy
        .on_order_filled("exit-fak-1", "token_up", dec!(0.85), dec!(12))
        .await;

    let lifecycles = strategy.base.position_lifecycle.read().await;
    let lc = lifecycles.get("token_up");
    assert!(
        lc.is_some(),
        "Lifecycle should exist for remaining position"
    );
    match &lc.unwrap().state {
        PositionLifecycleState::ExitExecuting { order_type, .. } => {
            assert_eq!(
                *order_type,
                OrderType::Gtc,
                "Residual should use GTC order type"
            );
        }
        other => panic!("Expected ExitExecuting(GTC) for residual, got: {other:?}"),
    }

    let has_place = actions.iter().any(|a| matches!(a, Action::PlaceOrder(_)));
    assert!(
        has_place,
        "Expected PlaceOrder for GTC residual after partial FAK fill, got: {actions:?}"
    );

    let positions = strategy.base.positions.read().await;
    let pos = positions
        .values()
        .flat_map(|v| v.iter())
        .find(|p| p.token_id == "token_up");
    assert!(pos.is_some(), "Position should still exist");
    assert_eq!(pos.unwrap().size, dec!(8), "Size should be reduced to 8");
}

#[tokio::test]
async fn lifecycle_rejection_transitions_to_healthy() {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.min_sustained_secs = 0;
    config.tailend.max_recent_volatility = dec!(1.0);
    config.stop_loss.min_remaining_secs = 0;

    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));
    let now = Utc::now();

    {
        let mut markets = base.active_markets.write().await;
        markets.insert(
            "market1".to_string(),
            MarketWithReference {
                market: make_tailend_market_info("market1", now + Duration::seconds(300)),
                reference_price: dec!(50000),
                reference_quality: ReferenceQuality::Exact,
                discovery_time: now,
                coin: "BTC".to_string(),
                window_ts: 0,
            },
        );
    }

    let pos = ArbitragePosition {
        market_id: "market1".to_string(),
        token_id: "token_up".to_string(),
        side: OutcomeSide::Up,
        entry_price: dec!(0.92),
        size: dec!(10),
        reference_price: dec!(50000),
        coin: "BTC".to_string(),
        order_id: None,
        entry_time: now - Duration::seconds(20),
        kelly_fraction: None,
        peak_price: dec!(0.92),
        estimated_fee: Decimal::ZERO,
        entry_market_price: dec!(0.92),
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
        entry_order_type: OrderType::Gtc,
        entry_fee_per_share: Decimal::ZERO,
        recovery_cost: Decimal::ZERO,
    };
    base.record_position(pos).await;

    let exit_oid = "exit-fak-token_up-999".to_string();
    {
        let mut lifecycle = base.ensure_lifecycle("token_up").await;
        lifecycle
            .transition(
                PositionLifecycleState::ExitExecuting {
                    order_id: exit_oid.clone(),
                    order_type: OrderType::Fak,
                    exit_price: dec!(0.85),
                    submitted_at: now,
                    hedge_order_id: None,
                    hedge_price: None,
                },
                "test trigger",
                now,
            )
            .unwrap();
        lifecycle.pending_exit_order_id = Some(exit_oid.clone());
        let mut lifecycles = base.position_lifecycle.write().await;
        lifecycles.insert("token_up".to_string(), lifecycle);
    }
    {
        let mut exit_orders = base.exit_orders_by_id.write().await;
        exit_orders.insert(
            exit_oid.clone(),
            ExitOrderMeta {
                token_id: "token_up".to_string(),
                order_token_id: "token_up".to_string(),
                order_type: OrderType::Fak,
                source_state: "HardCrash".to_string(),
                exit_price: dec!(0.85),
                clip_size: dec!(10),
            },
        );
    }
    let mut strategy = TailEndStrategy::new(base);
    let ctx = StrategyContext::new();

    let event = Event::OrderUpdate(polyrust_core::events::OrderEvent::Rejected {
        order_id: Some(exit_oid.clone()),
        token_id: Some("token_up".to_string()),
        reason: "couldn't be fully filled".to_string(),
    });
    let actions = strategy.on_event(&event, &ctx).await.unwrap();
    assert!(
        !actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
        "Rejection should not immediately place a new order"
    );

    let lifecycles = strategy.base.position_lifecycle.read().await;
    let lc = lifecycles.get("token_up");
    assert!(lc.is_some(), "Lifecycle should still exist after rejection");
    assert!(
        matches!(lc.unwrap().state, PositionLifecycleState::Healthy),
        "Expected Healthy after rejection, got: {:?}",
        lc.unwrap().state
    );

    let exit_orders = strategy.base.exit_orders_by_id.read().await;
    let has_token = exit_orders.values().any(|m| m.token_id == "token_up");
    assert!(
        !has_token,
        "exit_orders_by_id should be cleaned up after rejection"
    );
}

// ── Fast-path exit tests ─────────────────────────────────────────────

async fn make_fast_path_test_setup(
    fast_path_enabled: bool,
    book_age_secs: i64,
) -> (TailEndStrategy, StrategyContext) {
    let now = Utc::now();
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.enabled = true;
    config.tailend.fast_path_enabled = fast_path_enabled;
    config.tailend.fast_path_max_book_age_ms = 2000;
    config.tailend.min_sell_delay_secs = 10;
    config.stop_loss.hard_drop_abs = dec!(0.08);
    config.stop_loss.hard_reversal_pct = dec!(0.006);
    config.stop_loss.sl_max_book_age_ms = 3000;
    config.stop_loss.sl_max_external_age_ms = 5000;
    config.stop_loss.min_remaining_secs = 10;

    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    let market = MarketWithReference {
        market: make_tailend_market_info("market1", now + Duration::seconds(60)),
        reference_price: dec!(50000),
        reference_quality: ReferenceQuality::Exact,
        discovery_time: now,
        coin: "BTC".to_string(),
        window_ts: 0,
    };
    base.active_markets
        .write()
        .await
        .insert("market1".to_string(), market);

    let pos = ArbitragePosition {
        market_id: "market1".to_string(),
        token_id: "token_up".to_string(),
        side: OutcomeSide::Up,
        entry_price: dec!(0.92),
        size: dec!(10),
        reference_price: dec!(50000),
        coin: "BTC".to_string(),
        order_id: None,
        entry_time: now - Duration::seconds(20),
        kelly_fraction: None,
        peak_price: dec!(0.92),
        estimated_fee: Decimal::ZERO,
        entry_market_price: dec!(0.92),
        tick_size: dec!(0.01),
        fee_rate_bps: 0,
        entry_order_type: OrderType::Gtc,
        entry_fee_per_share: Decimal::ZERO,
        recovery_cost: Decimal::ZERO,
    };
    base.record_position(pos).await;

    {
        let composite = CompositePriceResult {
            price: dec!(49500),
            sources_used: 2,
            max_lag_ms: 100,
            dispersion_bps: dec!(10),
        };
        let mut cache = base.sl_composite_cache.write().await;
        cache.insert("BTC".to_string(), (composite, now));
    }

    {
        let mut history = base.price_history.write().await;
        let mut entries = VecDeque::new();
        entries.push_back((
            now - Duration::seconds(1),
            dec!(49500),
            "test".to_string(),
            now - Duration::seconds(1),
        ));
        history.insert("BTC".to_string(), entries);
    }

    let ctx = StrategyContext::new();

    {
        let mut md = ctx.market_data.write().await;
        md.orderbooks.insert(
            "token_up".to_string(),
            OrderbookSnapshot {
                token_id: "token_up".to_string(),
                bids: vec![OrderbookLevel {
                    price: dec!(0.83),
                    size: dec!(50),
                }],
                asks: vec![OrderbookLevel {
                    price: dec!(0.85),
                    size: dec!(50),
                }],
                timestamp: now - Duration::seconds(book_age_secs),
            },
        );
    }

    let strategy = TailEndStrategy::new(base);
    (strategy, ctx)
}

#[tokio::test]
async fn fast_path_triggers_exit_with_fresh_book() {
    let (strategy, ctx) = make_fast_path_test_setup(true, 1).await;

    let actions = strategy.evaluate_exits_on_price_change("BTC", &ctx).await;

    assert!(
        actions.iter().any(|a| matches!(a, Action::PlaceOrder(_))),
        "Fast-path should trigger exit order with fresh book and hard crash conditions"
    );

    let lifecycles = strategy.base.position_lifecycle.read().await;
    let lc = lifecycles.get("token_up").expect("Lifecycle should exist");
    assert!(
        matches!(lc.state, PositionLifecycleState::ExitExecuting { .. }),
        "Lifecycle should be ExitExecuting after fast-path exit, got: {:?}",
        lc.state
    );
}

#[tokio::test]
async fn fast_path_skips_stale_book() {
    let (strategy, ctx) = make_fast_path_test_setup(true, 5).await;

    let actions = strategy.evaluate_exits_on_price_change("BTC", &ctx).await;

    assert!(
        actions.is_empty(),
        "Fast-path should skip exit when book snapshot is stale"
    );

    let lifecycles = strategy.base.position_lifecycle.read().await;
    let lc = lifecycles.get("token_up").expect("Lifecycle should exist");
    assert!(
        matches!(lc.state, PositionLifecycleState::Healthy),
        "Lifecycle should remain Healthy when book is stale, got: {:?}",
        lc.state
    );
}

#[tokio::test]
async fn fast_path_skips_exit_executing_positions() {
    let (strategy, ctx) = make_fast_path_test_setup(true, 1).await;

    {
        let now = Utc::now();
        let mut lifecycle = strategy.base.ensure_lifecycle("token_up").await;
        lifecycle
            .transition(
                PositionLifecycleState::ExitExecuting {
                    order_id: "existing-exit-123".to_string(),
                    order_type: OrderType::Fak,
                    exit_price: dec!(0.85),
                    submitted_at: now,
                    hedge_order_id: None,
                    hedge_price: None,
                },
                "test pre-existing exit",
                now,
            )
            .unwrap();
        let mut lifecycles = strategy.base.position_lifecycle.write().await;
        lifecycles.insert("token_up".to_string(), lifecycle);
    }

    let actions = strategy.evaluate_exits_on_price_change("BTC", &ctx).await;

    assert!(
        actions.is_empty(),
        "Fast-path should skip positions in ExitExecuting state"
    );
}

#[tokio::test]
async fn fast_path_disabled_produces_no_exits() {
    let (strategy, ctx) = make_fast_path_test_setup(false, 1).await;

    let actions = strategy.evaluate_exits_on_price_change("BTC", &ctx).await;

    assert!(
        actions.is_empty(),
        "Fast-path should produce no exits when disabled"
    );
}
