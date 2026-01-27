use chrono::DateTime;
use libsql::params;
use polyrust_core::prelude::*;
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::error::{StoreError, StoreResult};
use crate::Store;

impl Store {
    /// Insert an order record.
    pub async fn insert_order(&self, order: &Order, strategy_name: &str) -> StoreResult<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO orders (id, token_id, side, price, size, filled_size, status, strategy_name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                order.id.clone(),
                order.token_id.clone(),
                format!("{:?}", order.side),
                order.price.to_string(),
                order.size.to_string(),
                order.filled_size.to_string(),
                format!("{:?}", order.status),
                strategy_name.to_string(),
                order.created_at.to_rfc3339(),
            ],
        )
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(())
    }

    /// Retrieve an order by ID.
    pub async fn get_order(&self, id: &str) -> StoreResult<Option<Order>> {
        let conn = self.conn();
        let mut rows = conn
            .query(
                "SELECT id, token_id, side, price, size, filled_size, status, created_at
                 FROM orders WHERE id = ?1",
                params![id],
            )
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;

        match rows.next().await.map_err(|e| StoreError::Query(e.to_string()))? {
            Some(row) => Ok(Some(parse_order_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Update the status of an order.
    pub async fn update_order_status(&self, id: &str, status: OrderStatus) -> StoreResult<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE orders SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            params![format!("{:?}", status), id],
        )
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(())
    }

    /// List orders with optional status filter and limit.
    pub async fn list_orders(
        &self,
        status: Option<OrderStatus>,
        limit: usize,
    ) -> StoreResult<Vec<Order>> {
        let conn = self.conn();
        let mut orders = Vec::new();

        let mut rows = match status {
            Some(s) => {
                conn.query(
                    "SELECT id, token_id, side, price, size, filled_size, status, created_at
                     FROM orders WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2",
                    params![format!("{:?}", s), limit as i64],
                )
                .await
            }
            None => {
                conn.query(
                    "SELECT id, token_id, side, price, size, filled_size, status, created_at
                     FROM orders ORDER BY created_at DESC LIMIT ?1",
                    params![limit as i64],
                )
                .await
            }
        }
        .map_err(|e| StoreError::Query(e.to_string()))?;

        while let Some(row) = rows.next().await.map_err(|e| StoreError::Query(e.to_string()))? {
            orders.push(parse_order_row(&row)?);
        }
        Ok(orders)
    }
}

fn parse_order_row(row: &libsql::Row) -> StoreResult<Order> {
    let id: String = row.get(0).map_err(|e| StoreError::Query(e.to_string()))?;
    let token_id: String = row.get(1).map_err(|e| StoreError::Query(e.to_string()))?;
    let side_str: String = row.get(2).map_err(|e| StoreError::Query(e.to_string()))?;
    let price_str: String = row.get(3).map_err(|e| StoreError::Query(e.to_string()))?;
    let size_str: String = row.get(4).map_err(|e| StoreError::Query(e.to_string()))?;
    let filled_str: String = row.get(5).map_err(|e| StoreError::Query(e.to_string()))?;
    let status_str: String = row.get(6).map_err(|e| StoreError::Query(e.to_string()))?;
    let created_str: String = row.get(7).map_err(|e| StoreError::Query(e.to_string()))?;

    Ok(Order {
        id,
        token_id,
        side: parse_order_side(&side_str)?,
        price: Decimal::from_str(&price_str).map_err(|e| StoreError::Query(e.to_string()))?,
        size: Decimal::from_str(&size_str).map_err(|e| StoreError::Query(e.to_string()))?,
        filled_size: Decimal::from_str(&filled_str)
            .map_err(|e| StoreError::Query(e.to_string()))?,
        status: parse_order_status(&status_str)?,
        created_at: DateTime::parse_from_rfc3339(&created_str)
            .map_err(|e| StoreError::Query(e.to_string()))?
            .to_utc(),
    })
}

use crate::parsing::parse_order_side;

fn parse_order_status(s: &str) -> StoreResult<OrderStatus> {
    match s {
        "Open" => Ok(OrderStatus::Open),
        "Filled" => Ok(OrderStatus::Filled),
        "PartiallyFilled" => Ok(OrderStatus::PartiallyFilled),
        "Cancelled" => Ok(OrderStatus::Cancelled),
        "Expired" => Ok(OrderStatus::Expired),
        other => Err(StoreError::Query(format!("unknown OrderStatus: {other}"))),
    }
}
