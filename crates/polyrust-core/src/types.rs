use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a market (Polymarket condition_id)
pub type MarketId = String;

/// ERC-1155 token identifier for a market outcome
pub type TokenId = String;

/// Unique identifier for an order
pub type OrderId = String;

/// Side of a market outcome (Up/Down, Yes/No)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeSide {
    Up,
    Down,
    Yes,
    No,
}

/// Order side
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

/// Order type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderType {
    /// Good Till Cancelled
    Gtc,
    /// Good Till Date
    Gtd,
    /// Fill or Kill
    Fok,
}

/// A request to place an order (strategy -> execution backend)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub token_id: TokenId,
    /// Price in 0-1 range (probability)
    pub price: Decimal,
    /// Number of shares
    pub size: Decimal,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub neg_risk: bool,
}

/// Result of an order placement
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResult {
    pub success: bool,
    pub order_id: Option<OrderId>,
    pub token_id: TokenId,
    pub price: Decimal,
    pub size: Decimal,
    pub side: OrderSide,
    pub status: Option<String>,
    pub message: String,
}

/// An open order
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: OrderId,
    pub token_id: TokenId,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub filled_size: Decimal,
    pub status: OrderStatus,
    pub created_at: DateTime<Utc>,
}

/// Order lifecycle status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStatus {
    Open,
    Filled,
    PartiallyFilled,
    Cancelled,
    Expired,
}

/// A position in a market outcome
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub id: Uuid,
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub side: OutcomeSide,
    pub entry_price: Decimal,
    pub size: Decimal,
    pub current_price: Decimal,
    pub entry_time: DateTime<Utc>,
    pub strategy_name: String,
}

impl Position {
    pub fn unrealized_pnl(&self) -> Decimal {
        (self.current_price - self.entry_price) * self.size
    }
}

/// A completed trade
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub id: Uuid,
    pub order_id: OrderId,
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub realized_pnl: Option<Decimal>,
    pub strategy_name: String,
    pub timestamp: DateTime<Utc>,
}

/// A single level in an orderbook (price + size)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderbookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

/// Orderbook snapshot for a single token
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderbookSnapshot {
    pub token_id: TokenId,
    pub bids: Vec<OrderbookLevel>,
    pub asks: Vec<OrderbookLevel>,
    pub timestamp: DateTime<Utc>,
}

impl OrderbookSnapshot {
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.first().map(|l| l.price)
    }

    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.first().map(|l| l.price)
    }

    pub fn mid_price(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some((bid + ask) / Decimal::TWO),
            (Some(p), None) | (None, Some(p)) => Some(p),
            _ => None,
        }
    }

    pub fn spread(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some(ask - bid),
            _ => None,
        }
    }
}

/// Market information (from Gamma API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketInfo {
    /// condition_id
    pub id: MarketId,
    pub slug: String,
    pub question: String,
    /// When the market's window starts (e.g. 15-min boundary).
    /// Used for accurate reference price lookup.
    pub start_date: Option<DateTime<Utc>>,
    pub end_date: DateTime<Utc>,
    pub token_ids: TokenIds,
    pub accepting_orders: bool,
    pub neg_risk: bool,
}

/// Token IDs for the two outcomes of a market
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenIds {
    /// "Up" or "Yes" outcome
    pub outcome_a: TokenId,
    /// "Down" or "No" outcome
    pub outcome_b: TokenId,
}

impl MarketInfo {
    pub fn has_ended(&self) -> bool {
        Utc::now() >= self.end_date
    }

    pub fn seconds_remaining(&self) -> i64 {
        (self.end_date - Utc::now()).num_seconds().max(0)
    }
}
