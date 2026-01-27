use libsql::{Builder, Connection, Database};
use tracing::info;

use crate::error::{StoreError, StoreResult};

/// Persistence layer wrapping a libsql (Turso) embedded SQLite database.
///
/// Uses a single shared connection to ensure in-memory databases work
/// correctly (each `Database::connect()` call on `:memory:` returns an
/// independent database, so we keep one connection for the Store's lifetime).
pub struct Store {
    _db: Database,
    conn: Connection,
}

impl Store {
    /// Open (or create) a database at the given path.
    /// Use `":memory:"` for an ephemeral in-memory database (tests).
    pub async fn new(path: &str) -> StoreResult<Self> {
        let db = Builder::new_local(path)
            .build()
            .await
            .map_err(|e| StoreError::Connection(e.to_string()))?;

        let conn = db
            .connect()
            .map_err(|e| StoreError::Connection(e.to_string()))?;

        let store = Self { _db: db, conn };
        store.run_migrations().await?;
        info!(path, "Store initialised");
        Ok(store)
    }

    /// Return a reference to the shared connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Run all schema migrations (idempotent).
    async fn run_migrations(&self) -> StoreResult<()> {
        let conn = self.conn();
        let stmts = [
            "CREATE TABLE IF NOT EXISTS trades (
                id TEXT PRIMARY KEY,
                order_id TEXT NOT NULL,
                market_id TEXT NOT NULL,
                token_id TEXT NOT NULL,
                side TEXT NOT NULL,
                price TEXT NOT NULL,
                size TEXT NOT NULL,
                realized_pnl TEXT,
                strategy_name TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
            "CREATE INDEX IF NOT EXISTS idx_trades_strategy ON trades(strategy_name)",
            "CREATE INDEX IF NOT EXISTS idx_trades_timestamp ON trades(timestamp)",
            "CREATE TABLE IF NOT EXISTS orders (
                id TEXT PRIMARY KEY,
                token_id TEXT NOT NULL,
                side TEXT NOT NULL,
                price TEXT NOT NULL,
                size TEXT NOT NULL,
                filled_size TEXT NOT NULL DEFAULT '0',
                status TEXT NOT NULL,
                strategy_name TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
            "CREATE INDEX IF NOT EXISTS idx_orders_status ON orders(status)",
            "CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type TEXT NOT NULL,
                topic TEXT NOT NULL,
                payload TEXT NOT NULL,
                timestamp TEXT NOT NULL DEFAULT (datetime('now'))
            )",
            "CREATE INDEX IF NOT EXISTS idx_events_topic ON events(topic)",
            "CREATE TABLE IF NOT EXISTS pnl_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                total_pnl TEXT NOT NULL,
                unrealized_pnl TEXT NOT NULL,
                realized_pnl TEXT NOT NULL,
                open_positions INTEGER NOT NULL,
                open_orders INTEGER NOT NULL,
                available_balance TEXT NOT NULL,
                timestamp TEXT NOT NULL DEFAULT (datetime('now'))
            )",
            "CREATE INDEX IF NOT EXISTS idx_pnl_timestamp ON pnl_snapshots(timestamp)",
        ];

        for stmt in stmts {
            conn.execute(stmt, ())
                .await
                .map_err(|e| StoreError::Migration(e.to_string()))?;
        }

        Ok(())
    }
}
