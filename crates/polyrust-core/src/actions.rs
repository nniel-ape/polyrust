use crate::types::*;
use rust_decimal::Decimal;

/// Actions a strategy can request the engine to execute
#[derive(Debug, Clone)]
pub enum Action {
    PlaceOrder(OrderRequest),
    PlaceBatchOrder(Vec<OrderRequest>),
    CancelOrder(OrderId),
    CancelAllOrders,
    Log {
        level: LogLevel,
        message: String,
    },
    EmitSignal {
        signal_type: String,
        payload: serde_json::Value,
    },
    /// Record a fill detected by the strategy (e.g. reconciled GTC fills).
    /// Engine converts this to `OrderEvent::Filled` for trade persistence.
    RecordFill {
        order_id: OrderId,
        market_id: MarketId,
        token_id: TokenId,
        side: OrderSide,
        price: Decimal,
        size: Decimal,
        realized_pnl: Option<Decimal>,
    },
    SubscribeMarket(MarketInfo),
    UnsubscribeMarket(MarketId),
    RedeemPosition(RedeemRequest),
}

/// Log severity level for Action::Log
#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}
