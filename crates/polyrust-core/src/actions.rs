use crate::types::*;

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
