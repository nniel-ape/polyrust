use chrono::DateTime;
use libsql::params;
use polyrust_core::prelude::*;
use rust_decimal::Decimal;
use std::str::FromStr;
use uuid::Uuid;

use crate::Store;
use crate::error::{StoreError, StoreResult};

impl Store {
    /// Insert a trade record.
    pub async fn insert_trade(&self, trade: &Trade) -> StoreResult<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO trades (id, order_id, market_id, token_id, side, price, size, realized_pnl, strategy_name, timestamp, fee, order_type, entry_price, close_reason, orderbook_snapshot)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                trade.fee.map(|d| d.to_string()),
                trade.order_type.clone(),
                trade.entry_price.map(|d| d.to_string()),
                trade.close_reason.clone(),
                trade.orderbook_snapshot.clone(),
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
                "SELECT id, order_id, market_id, token_id, side, price, size, realized_pnl, strategy_name, timestamp, fee, order_type, entry_price, close_reason, orderbook_snapshot
                 FROM trades WHERE id = ?1",
                params![id],
            )
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;

        match rows
            .next()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?
        {
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
                    "SELECT id, order_id, market_id, token_id, side, price, size, realized_pnl, strategy_name, timestamp, fee, order_type, entry_price, close_reason, orderbook_snapshot
                     FROM trades WHERE strategy_name = ?1 ORDER BY timestamp DESC LIMIT ?2",
                    params![s, limit as i64],
                )
                .await
            }
            None => {
                conn.query(
                    "SELECT id, order_id, market_id, token_id, side, price, size, realized_pnl, strategy_name, timestamp, fee, order_type, entry_price, close_reason, orderbook_snapshot
                     FROM trades ORDER BY timestamp DESC LIMIT ?1",
                    params![limit as i64],
                )
                .await
            }
        }
        .map_err(|e| StoreError::Query(e.to_string()))?;

        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?
        {
            trades.push(parse_trade_row(&row)?);
        }
        Ok(trades)
    }

    /// Count total trades, optionally filtered by strategy.
    pub async fn count_trades(&self, strategy: Option<&str>) -> StoreResult<i64> {
        let conn = self.conn();
        let mut rows = match strategy {
            Some(s) => {
                conn.query(
                    "SELECT COUNT(*) FROM trades WHERE strategy_name = ?1",
                    params![s],
                )
                .await
            }
            None => conn.query("SELECT COUNT(*) FROM trades", ()).await,
        }
        .map_err(|e| StoreError::Query(e.to_string()))?;

        match rows
            .next()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?
        {
            Some(row) => {
                let count: i64 = row.get(0).map_err(|e| StoreError::Query(e.to_string()))?;
                Ok(count)
            }
            None => Ok(0),
        }
    }

    /// Sum realized P&L across all trades with non-null realized_pnl.
    pub async fn sum_realized_pnl(&self, strategy: Option<&str>) -> StoreResult<Decimal> {
        let conn = self.conn();
        let query = match strategy {
            Some(_) => {
                "SELECT realized_pnl FROM trades WHERE realized_pnl IS NOT NULL AND strategy_name = ?1"
            }
            None => "SELECT realized_pnl FROM trades WHERE realized_pnl IS NOT NULL",
        };

        let mut rows = match strategy {
            Some(s) => conn.query(query, params![s]).await,
            None => conn.query(query, ()).await,
        }
        .map_err(|e| StoreError::Query(e.to_string()))?;

        let mut total = Decimal::ZERO;
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?
        {
            let pnl_str: String = row.get(0).map_err(|e| StoreError::Query(e.to_string()))?;
            let pnl = Decimal::from_str(&pnl_str).map_err(|e| StoreError::Query(e.to_string()))?;
            total += pnl;
        }
        Ok(total)
    }

    /// Sum fees across all trades with non-null fee.
    pub async fn sum_fees(&self, strategy: Option<&str>) -> StoreResult<Decimal> {
        let conn = self.conn();
        let query = match strategy {
            Some(_) => "SELECT fee FROM trades WHERE fee IS NOT NULL AND strategy_name = ?1",
            None => "SELECT fee FROM trades WHERE fee IS NOT NULL",
        };

        let mut rows = match strategy {
            Some(s) => conn.query(query, params![s]).await,
            None => conn.query(query, ()).await,
        }
        .map_err(|e| StoreError::Query(e.to_string()))?;

        let mut total = Decimal::ZERO;
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?
        {
            let fee_str: String = row.get(0).map_err(|e| StoreError::Query(e.to_string()))?;
            let fee = Decimal::from_str(&fee_str).map_err(|e| StoreError::Query(e.to_string()))?;
            total += fee;
        }
        Ok(total)
    }
}

fn parse_trade_row(row: &libsql::Row) -> StoreResult<Trade> {
    let id_str: String = row.get(0).map_err(|e| StoreError::Query(e.to_string()))?;
    let order_id: String = row.get(1).map_err(|e| StoreError::Query(e.to_string()))?;
    let market_id: String = row.get(2).map_err(|e| StoreError::Query(e.to_string()))?;
    let token_id: String = row.get(3).map_err(|e| StoreError::Query(e.to_string()))?;
    let side_str: String = row.get(4).map_err(|e| StoreError::Query(e.to_string()))?;
    let price_str: String = row.get(5).map_err(|e| StoreError::Query(e.to_string()))?;
    let size_str: String = row.get(6).map_err(|e| StoreError::Query(e.to_string()))?;
    let pnl_str: Option<String> = row.get(7).map_err(|e| StoreError::Query(e.to_string()))?;
    let strategy_name: String = row.get(8).map_err(|e| StoreError::Query(e.to_string()))?;
    let ts_str: String = row.get(9).map_err(|e| StoreError::Query(e.to_string()))?;

    // New optional columns (indices 10-14) — use .ok().flatten() for backward compat
    let fee_str: Option<String> = row.get(10).ok().flatten();
    let order_type: Option<String> = row.get(11).ok().flatten();
    let entry_price_str: Option<String> = row.get(12).ok().flatten();
    let close_reason: Option<String> = row.get(13).ok().flatten();
    let orderbook_snapshot: Option<String> = row.get(14).ok().flatten();

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
        fee: fee_str
            .map(|s| Decimal::from_str(&s))
            .transpose()
            .map_err(|e| StoreError::Query(e.to_string()))?,
        order_type,
        entry_price: entry_price_str
            .map(|s| Decimal::from_str(&s))
            .transpose()
            .map_err(|e| StoreError::Query(e.to_string()))?,
        close_reason,
        orderbook_snapshot,
    })
}

use crate::parsing::parse_order_side;
