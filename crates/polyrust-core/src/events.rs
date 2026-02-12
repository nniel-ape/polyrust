use crate::types::*;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// All events that flow through the EventBus
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    MarketData(MarketDataEvent),
    OrderUpdate(OrderEvent),
    PositionChange(PositionEvent),
    Signal(SignalEvent),
    System(SystemEvent),
}

impl Event {
    /// Topic string for event bus routing
    pub fn topic(&self) -> &'static str {
        match self {
            Event::MarketData(_) => "market_data",
            Event::OrderUpdate(_) => "order_update",
            Event::PositionChange(_) => "position_change",
            Event::Signal(_) => "signal",
            Event::System(_) => "system",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MarketDataEvent {
    OrderbookUpdate(OrderbookSnapshot),
    PriceChange {
        token_id: TokenId,
        price: Decimal,
        side: OrderSide,
        best_bid: Decimal,
        best_ask: Decimal,
    },
    Trade {
        token_id: TokenId,
        price: Decimal,
        size: Decimal,
        timestamp: DateTime<Utc>,
        /// Trade side from the data source (e.g. subgraph). `None` for live feeds
        /// where side is not available at the event level.
        #[serde(default)]
        side: Option<OrderSide>,
    },
    ExternalPrice {
        symbol: String,
        price: Decimal,
        source: String,
        timestamp: DateTime<Utc>,
    },
    MarketDiscovered(MarketInfo),
    MarketExpired(MarketId),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderEvent {
    Placed(OrderResult),
    Filled {
        order_id: OrderId,
        market_id: MarketId,
        token_id: TokenId,
        side: OrderSide,
        price: Decimal,
        size: Decimal,
        strategy_name: String,
        /// Strategy-provided realized P&L. When `Some`, persistence uses this
        /// value directly instead of computing from position state.
        realized_pnl: Option<Decimal>,
    },
    PartiallyFilled {
        order_id: OrderId,
        filled_size: Decimal,
        remaining_size: Decimal,
    },
    Cancelled(OrderId),
    CancelFailed {
        order_id: OrderId,
        reason: String,
    },
    Rejected {
        order_id: Option<OrderId>,
        reason: String,
        /// Token ID of the rejected order (for pending order cleanup)
        token_id: Option<TokenId>,
    },
    Redeemed {
        market_id: MarketId,
        tx_hash: String,
        strategy_name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PositionEvent {
    Opened(Position),
    Closed {
        position_id: uuid::Uuid,
        realized_pnl: Decimal,
    },
    Updated(Position),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalEvent {
    pub strategy_name: String,
    pub signal_type: String,
    pub payload: serde_json::Value,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SystemEvent {
    EngineStarted,
    EngineStopping,
    StrategyStarted(String),
    StrategyStopped(String),
    Error {
        source: String,
        message: String,
    },
    HealthCheck {
        strategies_active: usize,
        positions_open: usize,
        uptime_seconds: u64,
    },
    /// Periodic snapshot of open order IDs from the execution backend.
    /// Strategies compare this against their tracked orders to detect fills.
    OpenOrderSnapshot(Vec<String>),
}
