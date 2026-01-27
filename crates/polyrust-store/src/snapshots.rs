use chrono::{DateTime, Utc};
use libsql::params;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::Store;
use crate::error::{StoreError, StoreResult};

/// Point-in-time PnL snapshot for the dashboard / analytics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnlSnapshot {
    pub id: Option<i64>,
    pub total_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub realized_pnl: Decimal,
    pub open_positions: i64,
    pub open_orders: i64,
    pub available_balance: Decimal,
    pub timestamp: DateTime<Utc>,
}

impl Store {
    /// Insert a PnL snapshot.
    pub async fn insert_snapshot(&self, snap: &PnlSnapshot) -> StoreResult<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO pnl_snapshots (total_pnl, unrealized_pnl, realized_pnl, open_positions, open_orders, available_balance, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                snap.total_pnl.to_string(),
                snap.unrealized_pnl.to_string(),
                snap.realized_pnl.to_string(),
                snap.open_positions,
                snap.open_orders,
                snap.available_balance.to_string(),
                snap.timestamp.to_rfc3339(),
            ],
        )
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(())
    }

    /// List recent PnL snapshots, newest first.
    pub async fn list_snapshots(&self, limit: usize) -> StoreResult<Vec<PnlSnapshot>> {
        let conn = self.conn();
        let mut snaps = Vec::new();

        let mut rows = conn
            .query(
                "SELECT id, total_pnl, unrealized_pnl, realized_pnl, open_positions, open_orders, available_balance, timestamp
                 FROM pnl_snapshots ORDER BY id DESC LIMIT ?1",
                params![limit as i64],
            )
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?;

        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| StoreError::Query(e.to_string()))?
        {
            snaps.push(parse_snapshot_row(&row)?);
        }
        Ok(snaps)
    }

    /// Return the most recent PnL snapshot, if any.
    pub async fn latest_snapshot(&self) -> StoreResult<Option<PnlSnapshot>> {
        let mut snaps = self.list_snapshots(1).await?;
        Ok(snaps.pop())
    }
}

fn parse_snapshot_row(row: &libsql::Row) -> StoreResult<PnlSnapshot> {
    let id: i64 = row.get(0).map_err(|e| StoreError::Query(e.to_string()))?;
    let total_pnl_str: String = row.get(1).map_err(|e| StoreError::Query(e.to_string()))?;
    let unrealized_str: String = row.get(2).map_err(|e| StoreError::Query(e.to_string()))?;
    let realized_str: String = row.get(3).map_err(|e| StoreError::Query(e.to_string()))?;
    let open_positions: i64 = row.get(4).map_err(|e| StoreError::Query(e.to_string()))?;
    let open_orders: i64 = row.get(5).map_err(|e| StoreError::Query(e.to_string()))?;
    let balance_str: String = row.get(6).map_err(|e| StoreError::Query(e.to_string()))?;
    let ts_str: String = row.get(7).map_err(|e| StoreError::Query(e.to_string()))?;

    Ok(PnlSnapshot {
        id: Some(id),
        total_pnl: Decimal::from_str(&total_pnl_str)
            .map_err(|e| StoreError::Query(e.to_string()))?,
        unrealized_pnl: Decimal::from_str(&unrealized_str)
            .map_err(|e| StoreError::Query(e.to_string()))?,
        realized_pnl: Decimal::from_str(&realized_str)
            .map_err(|e| StoreError::Query(e.to_string()))?,
        open_positions,
        open_orders,
        available_balance: Decimal::from_str(&balance_str)
            .map_err(|e| StoreError::Query(e.to_string()))?,
        timestamp: DateTime::parse_from_rfc3339(&ts_str)
            .map_err(|e| StoreError::Query(e.to_string()))?
            .to_utc(),
    })
}
