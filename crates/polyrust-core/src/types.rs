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
    /// Market tick size for price rounding (default: 0.01)
    pub tick_size: Decimal,
    /// Fee rate in basis points (default: 0)
    pub fee_rate_bps: u32,
    /// Post-only flag: if true, the order is rejected if it would match immediately.
    /// Enforces maker behavior (0% fee). Default: false.
    pub post_only: bool,
}

impl OrderRequest {
    /// Create a new OrderRequest with default tick_size (0.01) and fee_rate_bps (0)
    pub fn new(
        token_id: TokenId,
        price: Decimal,
        size: Decimal,
        side: OrderSide,
        order_type: OrderType,
        neg_risk: bool,
    ) -> Self {
        Self {
            token_id,
            price,
            size,
            side,
            order_type,
            neg_risk,
            tick_size: Decimal::new(1, 2), // 0.01 default
            fee_rate_bps: 0,
            post_only: false,
        }
    }

    /// Set the tick size for this order
    pub fn with_tick_size(mut self, tick_size: Decimal) -> Self {
        self.tick_size = tick_size;
        self
    }

    /// Set the fee rate in basis points for this order
    pub fn with_fee_rate_bps(mut self, fee_rate_bps: u32) -> Self {
        self.fee_rate_bps = fee_rate_bps;
        self
    }

    /// Set the post-only flag for this order
    pub fn with_post_only(mut self, post_only: bool) -> Self {
        self.post_only = post_only;
        self
    }
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
    /// Taker fee paid (None = unknown/maker)
    pub fee: Option<Decimal>,
    /// Order type: "Gtc", "Gtd", "Fok"
    pub order_type: Option<String>,
    /// Average entry price on closing (sell) trades
    pub entry_price: Option<Decimal>,
    /// How the trade was closed: "Strategy", "Settlement", "ForceClose"
    pub close_reason: Option<String>,
    /// JSON blob of orderbook state at fill time (buys only)
    pub orderbook_snapshot: Option<String>,
}

/// Request to redeem winning positions after market resolution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedeemRequest {
    pub market_id: MarketId,
    /// Hex bytes32 condition_id for CTF contract
    pub condition_id: String,
    pub token_ids: Vec<TokenId>,
    pub neg_risk: bool,
}

/// Result of a position redemption
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedeemResult {
    pub market_id: MarketId,
    pub tx_hash: String,
    pub success: bool,
    pub message: String,
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

    /// Returns the size available at the best ask level.
    pub fn best_ask_depth(&self) -> Option<Decimal> {
        self.asks.first().map(|l| l.size)
    }

    /// Returns the total ask size available at price levels up to (inclusive) `max_price`.
    /// Useful for estimating how much a FOK order can sweep.
    pub fn ask_depth_up_to(&self, max_price: Decimal) -> Decimal {
        self.asks
            .iter()
            .take_while(|l| l.price <= max_price)
            .map(|l| l.size)
            .sum()
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
    /// Minimum order size in shares (from Gamma API orderMinSize).
    /// Defaults to 5.0 if not provided.
    pub min_order_size: Decimal,
    /// Market tick size for price rounding (from Gamma API).
    /// Defaults to 0.01 if not provided.
    #[serde(default = "default_tick_size")]
    pub tick_size: Decimal,
    /// Fee rate in basis points (from Gamma API).
    /// Defaults to 0 if not provided.
    #[serde(default)]
    pub fee_rate_bps: u32,
}

fn default_tick_size() -> Decimal {
    Decimal::new(1, 2) // 0.01
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
    pub fn has_ended_at(&self, now: DateTime<Utc>) -> bool {
        now >= self.end_date
    }

    pub fn seconds_remaining_at(&self, now: DateTime<Utc>) -> i64 {
        (self.end_date - now).num_seconds().max(0)
    }

    /// Convenience: uses Utc::now(). Prefer `_at` variants in backtest-aware code.
    pub fn has_ended(&self) -> bool {
        self.has_ended_at(Utc::now())
    }

    pub fn seconds_remaining(&self) -> i64 {
        self.seconds_remaining_at(Utc::now())
    }
}
