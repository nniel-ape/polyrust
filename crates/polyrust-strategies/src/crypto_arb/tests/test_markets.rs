use std::sync::Arc;

use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use polyrust_core::prelude::*;

use super::*;
use crate::crypto_arb::config::ArbitrageConfig;
use crate::crypto_arb::domain::{MarketWithReference, PendingOrder, ReferenceQuality};
use crate::crypto_arb::runtime::CryptoArbRuntime;

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
                peak_price: dec!(0.60),

                estimated_fee: Decimal::ZERO,
                entry_market_price: dec!(0.60),
                tick_size: dec!(0.01),
                fee_rate_bps: 0,
                entry_order_type: OrderType::Gtc,
                entry_fee_per_share: Decimal::ZERO,
                recovery_cost: Decimal::ZERO,
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
async fn reservation_blocks_concurrent_access() {
    let base = make_base_no_chainlink();

    // First reservation succeeds
    assert!(base.try_reserve_market(&"market1".to_string(), 1).await);

    // Second reservation for same market fails
    assert!(!base.try_reserve_market(&"market1".to_string(), 2).await);
}

#[tokio::test]
async fn reservation_counted_in_has_market_exposure() {
    let base = make_base_no_chainlink();

    // No exposure initially
    assert!(!base.has_market_exposure(&"market1".to_string()).await);

    // Reserve the market
    assert!(base.try_reserve_market(&"market1".to_string(), 1).await);

    // Now has exposure
    assert!(base.has_market_exposure(&"market1".to_string()).await);
}

#[tokio::test]
async fn reservation_counted_in_can_open_position() {
    let mut config = ArbitrageConfig::default();
    config.use_chainlink = false;
    config.max_positions = 2;
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    assert!(base.can_open_position().await);

    // Reserve 2 slots
    assert!(base.try_reserve_market(&"market1".to_string(), 2).await);

    // Now at capacity (1 reservation counts as 1 in the map, but total=1 + slot_count check)
    // Actually the reservation uses 1 map entry. Let's reserve another.
    assert!(!base.try_reserve_market(&"market2".to_string(), 1).await);
}

#[tokio::test]
async fn release_reservation_makes_market_available() {
    let base = make_base_no_chainlink();

    // Reserve and then release
    assert!(base.try_reserve_market(&"market1".to_string(), 1).await);
    assert!(base.has_market_exposure(&"market1".to_string()).await);

    base.release_reservation(&"market1".to_string()).await;

    // Market is now available again
    assert!(!base.has_market_exposure(&"market1".to_string()).await);
    assert!(base.try_reserve_market(&"market1".to_string(), 2).await);
}

#[tokio::test]
async fn release_reservation_then_pending_preserves_exposure() {
    let base = make_base_no_chainlink();

    // Reserve market
    assert!(base.try_reserve_market(&"market1".to_string(), 1).await);

    // Consume reservation and insert pending order
    base.release_reservation(&"market1".to_string()).await;
    {
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
    let base = Arc::new(CryptoArbRuntime::new(config, vec![]));

    // Initially not disabled
    assert!(!base.is_auto_disabled().await);

    // Record losing trades
    base.record_trade_pnl(dec!(-1.0)).await;
    base.record_trade_pnl(dec!(-1.0)).await;
    base.record_trade_pnl(dec!(-1.0)).await;

    // Now should be disabled (0% win rate after 3 trades)
    assert!(base.is_auto_disabled().await);
}

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
