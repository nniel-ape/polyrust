use polyrust_core::types::OrderSide;

use crate::error::{StoreError, StoreResult};

pub(crate) fn parse_order_side(s: &str) -> StoreResult<OrderSide> {
    match s {
        "Buy" => Ok(OrderSide::Buy),
        "Sell" => Ok(OrderSide::Sell),
        other => Err(StoreError::Query(format!("unknown OrderSide: {other}"))),
    }
}
