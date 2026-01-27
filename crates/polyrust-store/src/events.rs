use chrono::{DateTime, Utc};
use libsql::params;
use polyrust_core::prelude::Event;
use serde::{Deserialize, Serialize};

use crate::Store;
use crate::error::{StoreError, StoreResult};

/// A persisted event record (read back from the database).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub id: i64,
    pub event_type: String,
    pub topic: String,
    pub payload: String,
    pub timestamp: DateTime<Utc>,
}

impl Store {
    /// Persist an event as a JSON payload.
    pub async fn insert_event(&self, event: &Event) -> StoreResult<()> {
        let conn = self.conn();
        let event_type = format!("{:?}", event)
            .split('(')
            .next()
            .unwrap_or("Unknown")
            .to_string();
        let topic = event.topic().to_string();
        let payload = serde_json::to_string(event).map_err(|e| StoreError::Query(e.to_string()))?;

        conn.execute(
            "INSERT INTO events (event_type, topic, payload) VALUES (?1, ?2, ?3)",
            params![event_type, topic, payload],
        )
        .await
        .map_err(|e| StoreError::Query(e.to_string()))?;
        Ok(())
    }

    /// List events with optional topic filter and limit.
    pub async fn list_events(
        &self,
        topic: Option<&str>,
        limit: usize,
    ) -> StoreResult<Vec<StoredEvent>> {
        let conn = self.conn();
        let mut events = Vec::new();

        let mut rows = match topic {
            Some(t) => {
                conn.query(
                    "SELECT id, event_type, topic, payload, timestamp
                     FROM events WHERE topic = ?1 ORDER BY id DESC LIMIT ?2",
                    params![t, limit as i64],
                )
                .await
            }
            None => {
                conn.query(
                    "SELECT id, event_type, topic, payload, timestamp
                     FROM events ORDER BY id DESC LIMIT ?1",
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
            events.push(parse_event_row(&row)?);
        }
        Ok(events)
    }
}

fn parse_event_row(row: &libsql::Row) -> StoreResult<StoredEvent> {
    let id: i64 = row.get(0).map_err(|e| StoreError::Query(e.to_string()))?;
    let event_type: String = row.get(1).map_err(|e| StoreError::Query(e.to_string()))?;
    let topic: String = row.get(2).map_err(|e| StoreError::Query(e.to_string()))?;
    let payload: String = row.get(3).map_err(|e| StoreError::Query(e.to_string()))?;
    let ts_str: String = row.get(4).map_err(|e| StoreError::Query(e.to_string()))?;

    // SQLite datetime() returns "YYYY-MM-DD HH:MM:SS" format
    let timestamp = if let Ok(dt) = DateTime::parse_from_rfc3339(&ts_str) {
        dt.to_utc()
    } else {
        chrono::NaiveDateTime::parse_from_str(&ts_str, "%Y-%m-%d %H:%M:%S")
            .map(|ndt| ndt.and_utc())
            .map_err(|e| StoreError::Query(format!("invalid timestamp '{ts_str}': {e}")))?
    };

    Ok(StoredEvent {
        id,
        event_type,
        topic,
        payload,
        timestamp,
    })
}
