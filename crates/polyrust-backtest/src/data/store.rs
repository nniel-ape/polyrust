use chrono::{DateTime, Utc};
use libsql::{params, Builder, Connection, Database};
use rust_decimal::Decimal;
use tracing::info;

use crate::error::{BacktestError, BacktestResult};

/// Historical market data record types
#[derive(Debug, Clone)]
pub struct HistoricalPrice {
    pub token_id: String,
    pub timestamp: DateTime<Utc>,
    pub price: Decimal,
    pub source: String, // "clob" | "subgraph"
}

#[derive(Debug, Clone)]
pub struct HistoricalTrade {
    pub id: String, // tx_hash or synthetic ID
    pub token_id: String,
    pub timestamp: DateTime<Utc>,
    pub price: Decimal,
    pub size: Decimal,
    pub side: String, // "buy" | "sell"
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct HistoricalMarket {
    pub market_id: String,
    pub slug: String,
    pub question: String,
    pub start_date: DateTime<Utc>,
    pub end_date: DateTime<Utc>,
    pub token_a: String,
    pub token_b: String,
    pub neg_risk: bool,
}

#[derive(Debug, Clone)]
pub struct DataFetchLog {
    pub id: Option<i64>,
    pub source: String,
    pub token_id: String,
    pub start_ts: DateTime<Utc>,
    pub end_ts: DateTime<Utc>,
    pub fetched_at: DateTime<Utc>,
    pub row_count: i64,
}

/// Persistent historical data cache using libsql/Turso.
/// Separate from live Store; reused across backtest runs.
pub struct HistoricalDataStore {
    _db: Database,
    conn: Connection,
}

impl HistoricalDataStore {
    /// Open (or create) a historical data database at the given path.
    /// Use `":memory:"` for ephemeral in-memory database (tests).
    pub async fn new(path: &str) -> BacktestResult<Self> {
        let db = Builder::new_local(path)
            .build()
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?;

        let conn = db
            .connect()
            .map_err(|e| BacktestError::Database(e.to_string()))?;

        let store = Self { _db: db, conn };
        store.run_migrations().await?;
        info!(path, "HistoricalDataStore initialized");
        Ok(store)
    }

    /// Return a reference to the shared connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Run all schema migrations (idempotent).
    async fn run_migrations(&self) -> BacktestResult<()> {
        let conn = self.conn();
        let stmts = [
            // Price timeseries table
            "CREATE TABLE IF NOT EXISTS historical_prices (
                token_id TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                price TEXT NOT NULL,
                source TEXT NOT NULL,
                PRIMARY KEY (token_id, timestamp, source)
            )",
            "CREATE INDEX IF NOT EXISTS idx_hist_prices_token_ts ON historical_prices(token_id, timestamp)",
            // Trade events table
            "CREATE TABLE IF NOT EXISTS historical_trades (
                id TEXT PRIMARY KEY,
                token_id TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                price TEXT NOT NULL,
                size TEXT NOT NULL,
                side TEXT NOT NULL,
                source TEXT NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS idx_hist_trades_token_ts ON historical_trades(token_id, timestamp)",
            // Market metadata table
            "CREATE TABLE IF NOT EXISTS historical_markets (
                market_id TEXT PRIMARY KEY,
                slug TEXT NOT NULL,
                question TEXT NOT NULL,
                start_date TEXT NOT NULL,
                end_date TEXT NOT NULL,
                token_a TEXT NOT NULL,
                token_b TEXT NOT NULL,
                neg_risk INTEGER NOT NULL
            )",
            // Data fetch tracking table
            "CREATE TABLE IF NOT EXISTS data_fetch_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source TEXT NOT NULL,
                token_id TEXT NOT NULL,
                start_ts INTEGER NOT NULL,
                end_ts INTEGER NOT NULL,
                fetched_at TEXT NOT NULL,
                row_count INTEGER NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS idx_fetch_log_source_token ON data_fetch_log(source, token_id)",
        ];

        for stmt in stmts {
            conn.execute(stmt, ())
                .await
                .map_err(|e| BacktestError::Database(e.to_string()))?;
        }

        Ok(())
    }

    // Insert methods

    /// Insert multiple historical prices (batch operation).
    pub async fn insert_historical_prices(&self, prices: Vec<HistoricalPrice>) -> BacktestResult<()> {
        if prices.is_empty() {
            return Ok(());
        }

        let conn = self.conn();
        for price in prices {
            conn.execute(
                "INSERT OR REPLACE INTO historical_prices (token_id, timestamp, price, source) VALUES (?1, ?2, ?3, ?4)",
                params![
                    price.token_id,
                    price.timestamp.timestamp(),
                    price.price.to_string(),
                    price.source,
                ],
            )
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?;
        }

        Ok(())
    }

    /// Insert multiple historical trades (batch operation).
    pub async fn insert_historical_trades(&self, trades: Vec<HistoricalTrade>) -> BacktestResult<()> {
        if trades.is_empty() {
            return Ok(());
        }

        let conn = self.conn();
        for trade in trades {
            conn.execute(
                "INSERT OR REPLACE INTO historical_trades (id, token_id, timestamp, price, size, side, source) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    trade.id,
                    trade.token_id,
                    trade.timestamp.timestamp(),
                    trade.price.to_string(),
                    trade.size.to_string(),
                    trade.side,
                    trade.source,
                ],
            )
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?;
        }

        Ok(())
    }

    /// Insert a single historical market.
    pub async fn insert_historical_market(&self, market: HistoricalMarket) -> BacktestResult<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR REPLACE INTO historical_markets (market_id, slug, question, start_date, end_date, token_a, token_b, neg_risk) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                market.market_id,
                market.slug,
                market.question,
                market.start_date.to_rfc3339(),
                market.end_date.to_rfc3339(),
                market.token_a,
                market.token_b,
                if market.neg_risk { 1 } else { 0 },
            ],
        )
        .await
        .map_err(|e| BacktestError::Database(e.to_string()))?;

        Ok(())
    }

    /// Log a data fetch operation.
    pub async fn insert_fetch_log(&self, log: DataFetchLog) -> BacktestResult<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO data_fetch_log (source, token_id, start_ts, end_ts, fetched_at, row_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                log.source,
                log.token_id,
                log.start_ts.timestamp(),
                log.end_ts.timestamp(),
                log.fetched_at.to_rfc3339(),
                log.row_count,
            ],
        )
        .await
        .map_err(|e| BacktestError::Database(e.to_string()))?;

        Ok(())
    }

    // Query methods

    /// Get historical prices for a token within a time range.
    pub async fn get_historical_prices(
        &self,
        token_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<Vec<HistoricalPrice>> {
        let conn = self.conn();
        let mut rows = conn
            .query(
                "SELECT token_id, timestamp, price, source FROM historical_prices WHERE token_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3 ORDER BY timestamp ASC",
                params![token_id, start.timestamp(), end.timestamp()],
            )
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?;

        let mut prices = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| BacktestError::Database(e.to_string()))? {
            let token_id: String = row.get(0).map_err(|e| BacktestError::Database(e.to_string()))?;
            let timestamp: i64 = row.get(1).map_err(|e| BacktestError::Database(e.to_string()))?;
            let price_str: String = row.get(2).map_err(|e| BacktestError::Database(e.to_string()))?;
            let source: String = row.get(3).map_err(|e| BacktestError::Database(e.to_string()))?;

            let price = price_str.parse::<Decimal>()
                .map_err(|e| BacktestError::Database(format!("Failed to parse price: {}", e)))?;

            prices.push(HistoricalPrice {
                token_id,
                timestamp: DateTime::from_timestamp(timestamp, 0)
                    .ok_or_else(|| BacktestError::Database("Invalid timestamp".to_string()))?,
                price,
                source,
            });
        }

        Ok(prices)
    }

    /// Get historical trades for a token within a time range.
    pub async fn get_historical_trades(
        &self,
        token_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<Vec<HistoricalTrade>> {
        let conn = self.conn();
        let mut rows = conn
            .query(
                "SELECT id, token_id, timestamp, price, size, side, source FROM historical_trades WHERE token_id = ?1 AND timestamp >= ?2 AND timestamp <= ?3 ORDER BY timestamp ASC",
                params![token_id, start.timestamp(), end.timestamp()],
            )
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?;

        let mut trades = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| BacktestError::Database(e.to_string()))? {
            let id: String = row.get(0).map_err(|e| BacktestError::Database(e.to_string()))?;
            let token_id: String = row.get(1).map_err(|e| BacktestError::Database(e.to_string()))?;
            let timestamp: i64 = row.get(2).map_err(|e| BacktestError::Database(e.to_string()))?;
            let price_str: String = row.get(3).map_err(|e| BacktestError::Database(e.to_string()))?;
            let size_str: String = row.get(4).map_err(|e| BacktestError::Database(e.to_string()))?;
            let side: String = row.get(5).map_err(|e| BacktestError::Database(e.to_string()))?;
            let source: String = row.get(6).map_err(|e| BacktestError::Database(e.to_string()))?;

            let price = price_str.parse::<Decimal>()
                .map_err(|e| BacktestError::Database(format!("Failed to parse price: {}", e)))?;
            let size = size_str.parse::<Decimal>()
                .map_err(|e| BacktestError::Database(format!("Failed to parse size: {}", e)))?;

            trades.push(HistoricalTrade {
                id,
                token_id,
                timestamp: DateTime::from_timestamp(timestamp, 0)
                    .ok_or_else(|| BacktestError::Database("Invalid timestamp".to_string()))?,
                price,
                size,
                side,
                source,
            });
        }

        Ok(trades)
    }

    /// Get fetch log entries for a specific source and token.
    pub async fn get_fetch_log(&self, source: &str, token_id: &str) -> BacktestResult<Vec<DataFetchLog>> {
        let conn = self.conn();
        let mut rows = conn
            .query(
                "SELECT id, source, token_id, start_ts, end_ts, fetched_at, row_count FROM data_fetch_log WHERE source = ?1 AND token_id = ?2 ORDER BY fetched_at DESC",
                params![source, token_id],
            )
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?;

        let mut logs = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| BacktestError::Database(e.to_string()))? {
            let id: i64 = row.get(0).map_err(|e| BacktestError::Database(e.to_string()))?;
            let source: String = row.get(1).map_err(|e| BacktestError::Database(e.to_string()))?;
            let token_id: String = row.get(2).map_err(|e| BacktestError::Database(e.to_string()))?;
            let start_ts: i64 = row.get(3).map_err(|e| BacktestError::Database(e.to_string()))?;
            let end_ts: i64 = row.get(4).map_err(|e| BacktestError::Database(e.to_string()))?;
            let fetched_at_str: String = row.get(5).map_err(|e| BacktestError::Database(e.to_string()))?;
            let row_count: i64 = row.get(6).map_err(|e| BacktestError::Database(e.to_string()))?;

            let fetched_at = DateTime::parse_from_rfc3339(&fetched_at_str)
                .map_err(|e| BacktestError::Database(format!("Failed to parse fetched_at: {}", e)))?
                .with_timezone(&Utc);

            logs.push(DataFetchLog {
                id: Some(id),
                source,
                token_id,
                start_ts: DateTime::from_timestamp(start_ts, 0)
                    .ok_or_else(|| BacktestError::Database("Invalid start_ts".to_string()))?,
                end_ts: DateTime::from_timestamp(end_ts, 0)
                    .ok_or_else(|| BacktestError::Database("Invalid end_ts".to_string()))?,
                fetched_at,
                row_count,
            });
        }

        Ok(logs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn test_historical_data_store_creation() {
        let store = HistoricalDataStore::new(":memory:").await.unwrap();
        assert!(store.conn().is_autocommit());
    }

    #[tokio::test]
    async fn test_insert_and_query_prices() {
        let store = HistoricalDataStore::new(":memory:").await.unwrap();

        let now = Utc::now();
        let prices = vec![
            HistoricalPrice {
                token_id: "token1".to_string(),
                timestamp: now,
                price: dec!(0.50),
                source: "clob".to_string(),
            },
            HistoricalPrice {
                token_id: "token1".to_string(),
                timestamp: now + chrono::Duration::minutes(1),
                price: dec!(0.51),
                source: "clob".to_string(),
            },
        ];

        store.insert_historical_prices(prices.clone()).await.unwrap();

        let retrieved = store
            .get_historical_prices("token1", now - chrono::Duration::minutes(1), now + chrono::Duration::minutes(2))
            .await
            .unwrap();

        assert_eq!(retrieved.len(), 2);
        assert_eq!(retrieved[0].price, dec!(0.50));
        assert_eq!(retrieved[1].price, dec!(0.51));
    }

    #[tokio::test]
    async fn test_insert_and_query_trades() {
        let store = HistoricalDataStore::new(":memory:").await.unwrap();

        let now = Utc::now();
        let trades = vec![
            HistoricalTrade {
                id: "trade1".to_string(),
                token_id: "token1".to_string(),
                timestamp: now,
                price: dec!(0.50),
                size: dec!(100.0),
                side: "buy".to_string(),
                source: "clob".to_string(),
            },
            HistoricalTrade {
                id: "trade2".to_string(),
                token_id: "token1".to_string(),
                timestamp: now + chrono::Duration::minutes(1),
                price: dec!(0.51),
                size: dec!(50.0),
                side: "sell".to_string(),
                source: "subgraph".to_string(),
            },
        ];

        store.insert_historical_trades(trades.clone()).await.unwrap();

        let retrieved = store
            .get_historical_trades("token1", now - chrono::Duration::minutes(1), now + chrono::Duration::minutes(2))
            .await
            .unwrap();

        assert_eq!(retrieved.len(), 2);
        assert_eq!(retrieved[0].price, dec!(0.50));
        assert_eq!(retrieved[0].size, dec!(100.0));
        assert_eq!(retrieved[1].side, "sell");
    }

    #[tokio::test]
    async fn test_insert_market() {
        let store = HistoricalDataStore::new(":memory:").await.unwrap();

        let now = Utc::now();
        let market = HistoricalMarket {
            market_id: "market1".to_string(),
            slug: "btc-up-15min".to_string(),
            question: "Will BTC go up?".to_string(),
            start_date: now,
            end_date: now + chrono::Duration::minutes(15),
            token_a: "token_a".to_string(),
            token_b: "token_b".to_string(),
            neg_risk: false,
        };

        store.insert_historical_market(market).await.unwrap();

        // Query it back
        let conn = store.conn();
        let mut rows = conn
            .query(
                "SELECT market_id, slug, question FROM historical_markets WHERE market_id = ?1",
                params!["market1"],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let market_id: String = row.get(0).unwrap();
        let slug: String = row.get(1).unwrap();
        let question: String = row.get(2).unwrap();

        assert_eq!(market_id, "market1");
        assert_eq!(slug, "btc-up-15min");
        assert_eq!(question, "Will BTC go up?");
    }

    #[tokio::test]
    async fn test_fetch_log() {
        let store = HistoricalDataStore::new(":memory:").await.unwrap();

        let now = Utc::now();
        let log = DataFetchLog {
            id: None,
            source: "clob".to_string(),
            token_id: "token1".to_string(),
            start_ts: now - chrono::Duration::hours(1),
            end_ts: now,
            fetched_at: now,
            row_count: 100,
        };

        store.insert_fetch_log(log).await.unwrap();

        let logs = store.get_fetch_log("clob", "token1").await.unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].source, "clob");
        assert_eq!(logs[0].token_id, "token1");
        assert_eq!(logs[0].row_count, 100);
    }

    #[tokio::test]
    async fn test_empty_inserts() {
        let store = HistoricalDataStore::new(":memory:").await.unwrap();

        // Should not error on empty batch
        store.insert_historical_prices(vec![]).await.unwrap();
        store.insert_historical_trades(vec![]).await.unwrap();
    }

    #[tokio::test]
    async fn test_price_deduplication() {
        let store = HistoricalDataStore::new(":memory:").await.unwrap();

        let now = Utc::now();
        let price1 = HistoricalPrice {
            token_id: "token1".to_string(),
            timestamp: now,
            price: dec!(0.50),
            source: "clob".to_string(),
        };
        let price2 = HistoricalPrice {
            token_id: "token1".to_string(),
            timestamp: now,
            price: dec!(0.52), // Different price, same key
            source: "clob".to_string(),
        };

        store.insert_historical_prices(vec![price1]).await.unwrap();
        store.insert_historical_prices(vec![price2]).await.unwrap();

        let retrieved = store
            .get_historical_prices("token1", now - chrono::Duration::seconds(1), now + chrono::Duration::seconds(1))
            .await
            .unwrap();

        // Should only have one entry (replaced)
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].price, dec!(0.52));
    }
}
