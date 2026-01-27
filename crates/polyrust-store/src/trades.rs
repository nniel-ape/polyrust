use chrono::DateTime;
use libsql::params;
use polyrust_core::prelude::*;
use rust_decimal::Decimal;
use std::str::FromStr;
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::Store;

impl Store {
    /// Insert a trade record.
    pub async fn insert_trade(&self, trade: &Trade) -> StoreResult<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO trades (id, order_id, market_id, token_id, side, price, size, realized_pnl, strategy_name, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                trade.id.to_string(),
                trade.order_id.clone(),
                trade.market_id.clone(),
                trade.token_id.clone(),
                format!("{:?}", trade.side),
                trade.price.to_string(),
                trade.size.to_string(),
                trade.realized_pnl.map(|d| d.to_string()),
                trade.strategy_name.clone(),
                trade.timestamp.to_rfc3339(),
            ],
        )
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(())
    }

    /// Retrieve a trade by ID.
    pub async fn get_trade(&self, id: &str) -> StoreResult<Option<Trade>> {
        let conn = self.conn();
        let mut rows = conn
            .query(
                "SELECT id, order_id, market_id, token_id, side, price, size, realized_pnl, strategy_name, timestamp
                 FROM trades WHERE id = ?1",
                params![id],
            )
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;

        match rows.next().await.map_err(|e| StoreError::Query(e.to_string()))? {
            Some(row) => Ok(Some(parse_trade_row(&row)?)),
            None => Ok(None),
        }
    }

    /// List trades with optional strategy filter and limit.
    pub async fn list_trades(
        &self,
        strategy: Option<&str>,
        limit: usize,
    ) -> StoreResult<Vec<Trade>> {
        let conn = self.conn();

        let mut trades = Vec::new();

        let mut rows = match strategy {
            Some(s) => {
                conn.query(
                    "SELECT id, order_id, market_id, token_id, side, price, size, realized_pnl, strategy_name, timestamp
                     FROM trades WHERE strategy_name = ?1 ORDER BY timestamp DESC LIMIT ?2",
                    params![s, limit as i64],
                )
                .await
            }
            None => {
                conn.query(
                    "SELECT id, order_id, market_id, token_id, side, price, size, realized_pnl, strategy_name, timestamp
                     FROM trades ORDER BY timestamp DESC LIMIT ?1",
                    params![limit as i64],
                )
                .await
            }
        }
        .map_err(|e| StoreError::Query(e.to_string()))?;

        while let Some(row) = rows.next().await.map_err(|e| StoreError::Query(e.to_string()))? {
            trades.push(parse_trade_row(&row)?);
        }
        Ok(trades)
    }
}

fn parse_trade_row(row: &libsql::Row) -> StoreResult<Trade> {
    let id_str: String = row
        .get(0)
        .map_err(|e| StoreError::Query(e.to_string()))?;
    let order_id: String = row
        .get(1)
        .map_err(|e| StoreError::Query(e.to_string()))?;
    let market_id: String = row
        .get(2)
        .map_err(|e| StoreError::Query(e.to_string()))?;
    let token_id: String = row
        .get(3)
        .map_err(|e| StoreError::Query(e.to_string()))?;
    let side_str: String = row
        .get(4)
        .map_err(|e| StoreError::Query(e.to_string()))?;
    let price_str: String = row
        .get(5)
        .map_err(|e| StoreError::Query(e.to_string()))?;
    let size_str: String = row
        .get(6)
        .map_err(|e| StoreError::Query(e.to_string()))?;
    let pnl_str: Option<String> = row
        .get(7)
        .map_err(|e| StoreError::Query(e.to_string()))?;
    let strategy_name: String = row
        .get(8)
        .map_err(|e| StoreError::Query(e.to_string()))?;
    let ts_str: String = row
        .get(9)
        .map_err(|e| StoreError::Query(e.to_string()))?;

    Ok(Trade {
        id: Uuid::parse_str(&id_str).map_err(|e| StoreError::Query(e.to_string()))?,
        order_id,
        market_id,
        token_id,
        side: parse_order_side(&side_str)?,
        price: Decimal::from_str(&price_str).map_err(|e| StoreError::Query(e.to_string()))?,
        size: Decimal::from_str(&size_str).map_err(|e| StoreError::Query(e.to_string()))?,
        realized_pnl: pnl_str
            .map(|s| Decimal::from_str(&s))
            .transpose()
            .map_err(|e| StoreError::Query(e.to_string()))?,
        strategy_name,
        timestamp: DateTime::parse_from_rfc3339(&ts_str)
            .map_err(|e| StoreError::Query(e.to_string()))?
            .to_utc(),
    })
}

fn parse_order_side(s: &str) -> StoreResult<OrderSide> {
    match s {
        "Buy" => Ok(OrderSide::Buy),
        "Sell" => Ok(OrderSide::Sell),
        other => Err(StoreError::Query(format!("unknown OrderSide: {other}"))),
    }
}
