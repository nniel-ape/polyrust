use chrono::{TimeDelta, Utc};
use polyrust_core::prelude::*;
use rust_decimal_macros::dec;

#[test]
fn orderbook_mid_price() {
    let ob = OrderbookSnapshot {
        token_id: "tok1".into(),
        bids: vec![OrderbookLevel {
            price: dec!(0.50),
            size: dec!(100),
        }],
        asks: vec![OrderbookLevel {
            price: dec!(0.52),
            size: dec!(100),
        }],
        timestamp: Utc::now(),
    };
    assert_eq!(ob.mid_price(), Some(dec!(0.51)));
}

#[test]
fn orderbook_spread() {
    let ob = OrderbookSnapshot {
        token_id: "tok1".into(),
        bids: vec![OrderbookLevel {
            price: dec!(0.50),
            size: dec!(100),
        }],
        asks: vec![OrderbookLevel {
            price: dec!(0.52),
            size: dec!(100),
        }],
        timestamp: Utc::now(),
    };
    assert_eq!(ob.spread(), Some(dec!(0.02)));
}

#[test]
fn orderbook_empty_returns_none() {
    let ob = OrderbookSnapshot {
        token_id: "tok1".into(),
        bids: vec![],
        asks: vec![],
        timestamp: Utc::now(),
    };
    assert_eq!(ob.mid_price(), None);
    assert_eq!(ob.spread(), None);
    assert_eq!(ob.best_bid(), None);
    assert_eq!(ob.best_ask(), None);
}

#[test]
fn orderbook_one_side_only() {
    let ob = OrderbookSnapshot {
        token_id: "tok1".into(),
        bids: vec![OrderbookLevel {
            price: dec!(0.50),
            size: dec!(100),
        }],
        asks: vec![],
        timestamp: Utc::now(),
    };
    assert_eq!(ob.mid_price(), Some(dec!(0.50)));
    assert_eq!(ob.spread(), None);
}

#[test]
fn position_unrealized_pnl() {
    let pos = Position {
        id: uuid::Uuid::new_v4(),
        market_id: "m1".into(),
        token_id: "tok1".into(),
        side: OutcomeSide::Up,
        entry_price: dec!(0.50),
        size: dec!(10),
        current_price: dec!(0.60),
        entry_time: Utc::now(),
        strategy_name: "test".into(),
    };
    assert_eq!(pos.unrealized_pnl(), dec!(1.0));
}

#[test]
fn position_negative_pnl() {
    let pos = Position {
        id: uuid::Uuid::new_v4(),
        market_id: "m1".into(),
        token_id: "tok1".into(),
        side: OutcomeSide::Down,
        entry_price: dec!(0.60),
        size: dec!(10),
        current_price: dec!(0.50),
        entry_time: Utc::now(),
        strategy_name: "test".into(),
    };
    assert_eq!(pos.unrealized_pnl(), dec!(-1.0));
}

#[test]
fn order_request_serde_roundtrip() {
    let req = OrderRequest::new(
        "tok1".into(),
        dec!(0.55),
        dec!(5),
        OrderSide::Buy,
        OrderType::Gtc,
        false,
    );
    let json = serde_json::to_string(&req).unwrap();
    let deserialized: OrderRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.price, dec!(0.55));
    assert_eq!(deserialized.size, dec!(5));
    assert!(!deserialized.neg_risk);
}

#[test]
fn order_side_serde() {
    let json = serde_json::to_string(&OrderSide::Buy).unwrap();
    assert_eq!(json, "\"BUY\"");
    let json = serde_json::to_string(&OrderSide::Sell).unwrap();
    assert_eq!(json, "\"SELL\"");
}

#[test]
fn outcome_side_serde() {
    let json = serde_json::to_string(&OutcomeSide::Up).unwrap();
    assert_eq!(json, "\"up\"");
    let json = serde_json::to_string(&OutcomeSide::Down).unwrap();
    assert_eq!(json, "\"down\"");
}

#[test]
fn order_status_serde() {
    let json = serde_json::to_string(&OrderStatus::PartiallyFilled).unwrap();
    assert_eq!(json, "\"PARTIALLY_FILLED\"");
}

#[test]
fn orderbook_best_ask_depth() {
    let ob = OrderbookSnapshot {
        token_id: "tok1".into(),
        bids: vec![],
        asks: vec![
            OrderbookLevel {
                price: dec!(0.95),
                size: dec!(20),
            },
            OrderbookLevel {
                price: dec!(0.96),
                size: dec!(50),
            },
        ],
        timestamp: Utc::now(),
    };
    assert_eq!(ob.best_ask_depth(), Some(dec!(20)));
}

#[test]
fn orderbook_best_ask_depth_empty() {
    let ob = OrderbookSnapshot {
        token_id: "tok1".into(),
        bids: vec![],
        asks: vec![],
        timestamp: Utc::now(),
    };
    assert_eq!(ob.best_ask_depth(), None);
}

#[test]
fn orderbook_ask_depth_up_to() {
    let ob = OrderbookSnapshot {
        token_id: "tok1".into(),
        bids: vec![],
        asks: vec![
            OrderbookLevel {
                price: dec!(0.95),
                size: dec!(20),
            },
            OrderbookLevel {
                price: dec!(0.96),
                size: dec!(30),
            },
            OrderbookLevel {
                price: dec!(0.98),
                size: dec!(100),
            },
        ],
        timestamp: Utc::now(),
    };
    // Up to 0.96 should include first two levels
    assert_eq!(ob.ask_depth_up_to(dec!(0.96)), dec!(50));
    // Up to 0.95 should include only first level
    assert_eq!(ob.ask_depth_up_to(dec!(0.95)), dec!(20));
    // Up to 1.00 should include all levels
    assert_eq!(ob.ask_depth_up_to(dec!(1.00)), dec!(150));
    // Up to 0.90 should include nothing
    assert_eq!(ob.ask_depth_up_to(dec!(0.90)), dec!(0));
}

#[test]
fn market_info_seconds_remaining() {
    let future = Utc::now() + TimeDelta::seconds(300);
    let market = MarketInfo {
        id: "m1".into(),
        slug: "test-market".into(),
        question: "Will BTC be above 100k?".into(),
        start_date: None,
        end_date: future,
        token_ids: TokenIds {
            outcome_a: "tok_up".into(),
            outcome_b: "tok_down".into(),
        },
        accepting_orders: true,
        neg_risk: false,
        min_order_size: Decimal::new(50, 1), // 5.0 shares default
        tick_size: Decimal::new(1, 2), // 0.01 default
        fee_rate_bps: 0,
    };
    let remaining = market.seconds_remaining();
    assert!(remaining >= 299 && remaining <= 301);
    assert!(!market.has_ended());
}

#[test]
fn market_info_has_ended() {
    let past = Utc::now() - TimeDelta::seconds(60);
    let market = MarketInfo {
        id: "m1".into(),
        slug: "test-market".into(),
        question: "Will BTC be above 100k?".into(),
        start_date: None,
        end_date: past,
        token_ids: TokenIds {
            outcome_a: "tok_up".into(),
            outcome_b: "tok_down".into(),
        },
        accepting_orders: false,
        neg_risk: false,
        min_order_size: Decimal::new(50, 1), // 5.0 shares default
        tick_size: Decimal::new(1, 2), // 0.01 default
        fee_rate_bps: 0,
    };
    assert!(market.has_ended());
    assert_eq!(market.seconds_remaining(), 0);
}
