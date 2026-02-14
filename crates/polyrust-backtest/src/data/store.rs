use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use libsql::{Builder, Connection, Database, params};
use rust_decimal::Decimal;
use tokio::sync::Mutex;
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

/// Synthesized price bucket from streaming trade aggregation.
/// Produced by `HistoricalDataStore::stream_trades_into_synthesis` — one bucket per
/// (token_id, time_window). Trades are never materialized as `HistoricalTrade`;
/// they go directly from DB cursor → bucket → this output struct.
#[derive(Debug, Clone)]
pub struct SynthesizedBucket {
    pub token_id: String,
    pub bucket_end: i64, // unix timestamp
    pub last_price: Decimal,
    pub best_bid: Decimal,
    pub best_ask: Decimal,
}

/// Historical crypto price from Binance klines (OHLCV).
#[derive(Debug, Clone)]
pub struct HistoricalCryptoPrice {
    pub symbol: String, // "BTC", "ETH"
    pub timestamp: DateTime<Utc>,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub source: String, // "binance-spot", "binance-futures"
}

/// Persistent historical data cache using libsql/Turso.
/// Separate from live Store; reused across backtest runs.
/// Write operations are serialized via `write_lock` to prevent
/// transaction interleaving from concurrent async tasks.
pub struct HistoricalDataStore {
    _db: Database,
    conn: Connection,
    write_lock: Mutex<()>,
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

        let store = Self {
            _db: db,
            conn,
            write_lock: Mutex::new(()),
        };
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

        // Performance PRAGMAs — WAL enables concurrent reads during writes,
        // NORMAL sync is safe with WAL, larger cache and in-memory temp tables.
        // Use execute_batch because journal_mode returns a result row that
        // execute() rejects with "Execute returned rows".
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA cache_size=-64000;
             PRAGMA temp_store=MEMORY;",
        )
        .await
        .map_err(|e| BacktestError::Database(e.to_string()))?;

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
            // Covering index for stream_trades_into_synthesis — avoids table lookups for price/side
            "CREATE INDEX IF NOT EXISTS idx_hist_trades_synthesis ON historical_trades(token_id, timestamp, price, side)",
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
            // Historical crypto prices (Binance klines)
            "CREATE TABLE IF NOT EXISTS historical_crypto_prices (
                symbol TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                open TEXT NOT NULL,
                high TEXT NOT NULL,
                low TEXT NOT NULL,
                close TEXT NOT NULL,
                volume TEXT NOT NULL,
                source TEXT NOT NULL,
                PRIMARY KEY (symbol, timestamp, source)
            )",
            "CREATE INDEX IF NOT EXISTS idx_crypto_prices_symbol_ts ON historical_crypto_prices(symbol, timestamp)",
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
    /// Uses `execute_batch` to send all INSERTs in one call per chunk.
    pub async fn insert_historical_prices(
        &self,
        prices: Vec<HistoricalPrice>,
    ) -> BacktestResult<()> {
        if prices.is_empty() {
            return Ok(());
        }

        let _guard = self.write_lock.lock().await;
        let conn = self.conn();

        for chunk in prices.chunks(500) {
            let mut sql = String::with_capacity(chunk.len() * 120);
            for p in chunk {
                let token_id = p.token_id.replace('\'', "''");
                let ts = p.timestamp.timestamp();
                let price = p.price;
                let source = p.source.replace('\'', "''");
                sql.push_str(&format!(
                    "INSERT OR REPLACE INTO historical_prices (token_id, timestamp, price, source) VALUES ('{token_id}', {ts}, '{price}', '{source}');\n"
                ));
            }
            conn.execute_batch(&sql)
                .await
                .map_err(|e| BacktestError::Database(e.to_string()))?;
        }

        Ok(())
    }

    /// Insert multiple historical trades (batch operation).
    /// Uses `execute_batch` to send all INSERTs in one call per chunk.
    pub async fn insert_historical_trades(
        &self,
        trades: Vec<HistoricalTrade>,
    ) -> BacktestResult<()> {
        if trades.is_empty() {
            return Ok(());
        }

        let _guard = self.write_lock.lock().await;
        let conn = self.conn();

        for chunk in trades.chunks(500) {
            let mut sql = String::with_capacity(chunk.len() * 180);
            for t in chunk {
                let id = t.id.replace('\'', "''");
                let token_id = t.token_id.replace('\'', "''");
                let ts = t.timestamp.timestamp();
                let price = t.price;
                let size = t.size;
                let side = t.side.replace('\'', "''");
                let source = t.source.replace('\'', "''");
                sql.push_str(&format!(
                    "INSERT OR REPLACE INTO historical_trades (id, token_id, timestamp, price, size, side, source) VALUES ('{id}', '{token_id}', {ts}, '{price}', '{size}', '{side}', '{source}');\n"
                ));
            }
            conn.execute_batch(&sql)
                .await
                .map_err(|e| BacktestError::Database(e.to_string()))?;
        }

        Ok(())
    }

    /// Insert a single historical market.
    /// No write_lock needed — WAL mode handles concurrent single-statement writes.
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
    /// No write_lock needed — WAL mode handles concurrent single-statement writes.
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

    /// Get a historical market by market_id.
    pub async fn get_historical_market(
        &self,
        market_id: &str,
    ) -> BacktestResult<Option<HistoricalMarket>> {
        let conn = self.conn();
        let mut rows = conn
            .query(
                "SELECT market_id, slug, question, start_date, end_date, token_a, token_b, neg_risk FROM historical_markets WHERE market_id = ?1",
                params![market_id],
            )
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?;

        if let Some(row) = rows
            .next()
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?
        {
            let start_str: String = row
                .get(3)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let end_str: String = row
                .get(4)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let neg_risk_int: i64 = row
                .get(7)
                .map_err(|e| BacktestError::Database(e.to_string()))?;

            Ok(Some(HistoricalMarket {
                market_id: row
                    .get(0)
                    .map_err(|e| BacktestError::Database(e.to_string()))?,
                slug: row
                    .get(1)
                    .map_err(|e| BacktestError::Database(e.to_string()))?,
                question: row
                    .get(2)
                    .map_err(|e| BacktestError::Database(e.to_string()))?,
                start_date: DateTime::parse_from_rfc3339(&start_str)
                    .map_err(|e| BacktestError::Database(format!("Invalid start_date: {}", e)))?
                    .with_timezone(&Utc),
                end_date: DateTime::parse_from_rfc3339(&end_str)
                    .map_err(|e| BacktestError::Database(format!("Invalid end_date: {}", e)))?
                    .with_timezone(&Utc),
                token_a: row
                    .get(5)
                    .map_err(|e| BacktestError::Database(e.to_string()))?,
                token_b: row
                    .get(6)
                    .map_err(|e| BacktestError::Database(e.to_string()))?,
                neg_risk: neg_risk_int != 0,
            }))
        } else {
            Ok(None)
        }
    }

    /// List all cached markets whose time range overlaps [start, end].
    pub async fn list_cached_markets(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<Vec<HistoricalMarket>> {
        let conn = self.conn();
        let mut rows = conn
            .query(
                "SELECT market_id, slug, question, start_date, end_date, token_a, token_b, neg_risk FROM historical_markets WHERE start_date < ?2 AND end_date > ?1",
                params![start.to_rfc3339(), end.to_rfc3339()],
            )
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?;

        let mut markets = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?
        {
            let start_str: String = row
                .get(3)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let end_str: String = row
                .get(4)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let neg_risk_int: i64 = row
                .get(7)
                .map_err(|e| BacktestError::Database(e.to_string()))?;

            markets.push(HistoricalMarket {
                market_id: row
                    .get(0)
                    .map_err(|e| BacktestError::Database(e.to_string()))?,
                slug: row
                    .get(1)
                    .map_err(|e| BacktestError::Database(e.to_string()))?,
                question: row
                    .get(2)
                    .map_err(|e| BacktestError::Database(e.to_string()))?,
                start_date: DateTime::parse_from_rfc3339(&start_str)
                    .map_err(|e| BacktestError::Database(format!("Invalid start_date: {}", e)))?
                    .with_timezone(&Utc),
                end_date: DateTime::parse_from_rfc3339(&end_str)
                    .map_err(|e| BacktestError::Database(format!("Invalid end_date: {}", e)))?
                    .with_timezone(&Utc),
                token_a: row
                    .get(5)
                    .map_err(|e| BacktestError::Database(e.to_string()))?,
                token_b: row
                    .get(6)
                    .map_err(|e| BacktestError::Database(e.to_string()))?,
                neg_risk: neg_risk_int != 0,
            });
        }

        Ok(markets)
    }

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
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?
        {
            let token_id: String = row
                .get(0)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let timestamp: i64 = row
                .get(1)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let price_str: String = row
                .get(2)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let source: String = row
                .get(3)
                .map_err(|e| BacktestError::Database(e.to_string()))?;

            let price = price_str
                .parse::<Decimal>()
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
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?
        {
            let id: String = row
                .get(0)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let token_id: String = row
                .get(1)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let timestamp: i64 = row
                .get(2)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let price_str: String = row
                .get(3)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let size_str: String = row
                .get(4)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let side: String = row
                .get(5)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let source: String = row
                .get(6)
                .map_err(|e| BacktestError::Database(e.to_string()))?;

            let price = price_str
                .parse::<Decimal>()
                .map_err(|e| BacktestError::Database(format!("Failed to parse price: {}", e)))?;
            let size = size_str
                .parse::<Decimal>()
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
    pub async fn get_fetch_log(
        &self,
        source: &str,
        token_id: &str,
    ) -> BacktestResult<Vec<DataFetchLog>> {
        let conn = self.conn();
        let mut rows = conn
            .query(
                "SELECT id, source, token_id, start_ts, end_ts, fetched_at, row_count FROM data_fetch_log WHERE source = ?1 AND token_id = ?2 ORDER BY fetched_at DESC",
                params![source, token_id],
            )
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?;

        let mut logs = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?
        {
            let id: i64 = row
                .get(0)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let source: String = row
                .get(1)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let token_id: String = row
                .get(2)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let start_ts: i64 = row
                .get(3)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let end_ts: i64 = row
                .get(4)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let fetched_at_str: String = row
                .get(5)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let row_count: i64 = row
                .get(6)
                .map_err(|e| BacktestError::Database(e.to_string()))?;

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

    // Historical crypto price methods (Binance klines)

    /// Insert multiple historical crypto prices (batch operation).
    pub async fn insert_crypto_prices(
        &self,
        prices: Vec<HistoricalCryptoPrice>,
    ) -> BacktestResult<()> {
        if prices.is_empty() {
            return Ok(());
        }

        let _guard = self.write_lock.lock().await;
        let conn = self.conn();

        for chunk in prices.chunks(500) {
            let mut sql = String::with_capacity(chunk.len() * 200);
            for p in chunk {
                let symbol = p.symbol.replace('\'', "''");
                let ts = p.timestamp.timestamp();
                let source = p.source.replace('\'', "''");
                sql.push_str(&format!(
                    "INSERT OR REPLACE INTO historical_crypto_prices (symbol, timestamp, open, high, low, close, volume, source) VALUES ('{symbol}', {ts}, '{}', '{}', '{}', '{}', '{}', '{source}');\n",
                    p.open, p.high, p.low, p.close, p.volume
                ));
            }
            conn.execute_batch(&sql)
                .await
                .map_err(|e| BacktestError::Database(e.to_string()))?;
        }

        Ok(())
    }

    /// Get historical crypto prices for a symbol within a time range.
    pub async fn get_crypto_prices(
        &self,
        symbol: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<Vec<HistoricalCryptoPrice>> {
        let conn = self.conn();
        let mut rows = conn
            .query(
                "SELECT symbol, timestamp, open, high, low, close, volume, source FROM historical_crypto_prices WHERE symbol = ?1 AND timestamp >= ?2 AND timestamp <= ?3 ORDER BY timestamp ASC",
                params![symbol, start.timestamp(), end.timestamp()],
            )
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?;

        let mut prices = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| BacktestError::Database(e.to_string()))?
        {
            let symbol: String = row
                .get(0)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let timestamp: i64 = row
                .get(1)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let open_str: String = row
                .get(2)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let high_str: String = row
                .get(3)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let low_str: String = row
                .get(4)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let close_str: String = row
                .get(5)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let volume_str: String = row
                .get(6)
                .map_err(|e| BacktestError::Database(e.to_string()))?;
            let source: String = row
                .get(7)
                .map_err(|e| BacktestError::Database(e.to_string()))?;

            let parse = |s: &str, field: &str| -> BacktestResult<Decimal> {
                s.parse::<Decimal>()
                    .map_err(|e| BacktestError::Database(format!("Failed to parse {field}: {e}")))
            };

            prices.push(HistoricalCryptoPrice {
                symbol,
                timestamp: DateTime::from_timestamp(timestamp, 0)
                    .ok_or_else(|| BacktestError::Database("Invalid timestamp".to_string()))?,
                open: parse(&open_str, "open")?,
                high: parse(&high_str, "high")?,
                low: parse(&low_str, "low")?,
                close: parse(&close_str, "close")?,
                volume: parse(&volume_str, "volume")?,
                source,
            });
        }

        Ok(prices)
    }

    // ── Batch query methods for optimized event preloading ──

    /// Get multiple historical markets by ID in a single query.
    /// Uses `WHERE market_id IN (...)` with chunking (200 per chunk).
    pub async fn get_historical_markets_batch(
        &self,
        market_ids: &[String],
    ) -> BacktestResult<Vec<HistoricalMarket>> {
        if market_ids.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn();
        let mut markets = Vec::with_capacity(market_ids.len());

        for chunk in market_ids.chunks(200) {
            let in_clause: String = chunk
                .iter()
                .map(|id| format!("'{}'", id.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",");

            let sql = format!(
                "SELECT market_id, slug, question, start_date, end_date, token_a, token_b, neg_risk \
                 FROM historical_markets WHERE market_id IN ({in_clause})"
            );

            let mut rows = conn
                .query(&sql, ())
                .await
                .map_err(|e| BacktestError::Database(e.to_string()))?;

            while let Some(row) = rows
                .next()
                .await
                .map_err(|e| BacktestError::Database(e.to_string()))?
            {
                let start_str: String = row
                    .get(3)
                    .map_err(|e| BacktestError::Database(e.to_string()))?;
                let end_str: String = row
                    .get(4)
                    .map_err(|e| BacktestError::Database(e.to_string()))?;
                let neg_risk_int: i64 = row
                    .get(7)
                    .map_err(|e| BacktestError::Database(e.to_string()))?;

                markets.push(HistoricalMarket {
                    market_id: row
                        .get(0)
                        .map_err(|e| BacktestError::Database(e.to_string()))?,
                    slug: row
                        .get(1)
                        .map_err(|e| BacktestError::Database(e.to_string()))?,
                    question: row
                        .get(2)
                        .map_err(|e| BacktestError::Database(e.to_string()))?,
                    start_date: DateTime::parse_from_rfc3339(&start_str)
                        .map_err(|e| BacktestError::Database(format!("Invalid start_date: {}", e)))?
                        .with_timezone(&Utc),
                    end_date: DateTime::parse_from_rfc3339(&end_str)
                        .map_err(|e| BacktestError::Database(format!("Invalid end_date: {}", e)))?
                        .with_timezone(&Utc),
                    token_a: row
                        .get(5)
                        .map_err(|e| BacktestError::Database(e.to_string()))?,
                    token_b: row
                        .get(6)
                        .map_err(|e| BacktestError::Database(e.to_string()))?,
                    neg_risk: neg_risk_int != 0,
                });
            }
        }

        Ok(markets)
    }

    /// Get historical prices for multiple tokens in a single query.
    /// Uses `WHERE token_id IN (...)` with chunking (200 tokens per chunk).
    pub async fn get_historical_prices_batch(
        &self,
        token_ids: &[String],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> BacktestResult<Vec<HistoricalPrice>> {
        if token_ids.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn();
        let start_ts = start.timestamp();
        let end_ts = end.timestamp();
        let mut prices = Vec::new();

        for chunk in token_ids.chunks(200) {
            let in_clause: String = chunk
                .iter()
                .map(|id| format!("'{}'", id.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",");

            let sql = format!(
                "SELECT token_id, timestamp, price, source FROM historical_prices \
                 WHERE token_id IN ({in_clause}) AND timestamp >= {start_ts} AND timestamp <= {end_ts} \
                 ORDER BY timestamp ASC"
            );

            let mut rows = conn
                .query(&sql, ())
                .await
                .map_err(|e| BacktestError::Database(e.to_string()))?;

            while let Some(row) = rows
                .next()
                .await
                .map_err(|e| BacktestError::Database(e.to_string()))?
            {
                let token_id: String = row
                    .get(0)
                    .map_err(|e| BacktestError::Database(e.to_string()))?;
                let timestamp: i64 = row
                    .get(1)
                    .map_err(|e| BacktestError::Database(e.to_string()))?;
                let price_str: String = row
                    .get(2)
                    .map_err(|e| BacktestError::Database(e.to_string()))?;
                let source: String = row
                    .get(3)
                    .map_err(|e| BacktestError::Database(e.to_string()))?;

                let price = price_str.parse::<Decimal>().map_err(|e| {
                    BacktestError::Database(format!("Failed to parse price: {}", e))
                })?;

                prices.push(HistoricalPrice {
                    token_id,
                    timestamp: DateTime::from_timestamp(timestamp, 0)
                        .ok_or_else(|| BacktestError::Database("Invalid timestamp".to_string()))?,
                    price,
                    source,
                });
            }
        }

        Ok(prices)
    }

    /// Stream trades from DB directly into bucket aggregation, producing synthesized price data.
    ///
    /// Only reads 4 columns (token_id, timestamp, price, side) — skips id, size, source.
    /// Trade rows are never materialized as `HistoricalTrade`; they go directly from
    /// DB cursor → bucket aggregation → `SynthesizedBucket` output.
    ///
    /// This eliminates ~65% of peak memory vs loading all trades then running synthesis + retain().
    pub async fn stream_trades_into_synthesis(
        &self,
        token_ids: &[String],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        fidelity_secs: u64,
    ) -> BacktestResult<Vec<SynthesizedBucket>> {
        if token_ids.is_empty() || fidelity_secs == 0 {
            return Ok(Vec::new());
        }

        let conn = self.conn();
        let start_ts = start.timestamp();
        let end_ts = end.timestamp();
        let fidelity = fidelity_secs as i64;
        let default_spread = Decimal::new(1, 2); // 0.01 (1 tick)

        // Aggregate trades into buckets per token
        let mut token_buckets: HashMap<String, BTreeMap<i64, BucketAgg>> = HashMap::new();
        let mut prev_prices: HashMap<String, Decimal> = HashMap::new();

        for chunk in token_ids.chunks(200) {
            let in_clause: String = chunk
                .iter()
                .map(|id| format!("'{}'", id.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",");

            let sql = format!(
                "SELECT token_id, timestamp, price, side FROM historical_trades \
                 WHERE token_id IN ({in_clause}) AND timestamp >= {start_ts} AND timestamp <= {end_ts} \
                 ORDER BY token_id, timestamp ASC"
            );

            let mut rows = conn
                .query(&sql, ())
                .await
                .map_err(|e| BacktestError::Database(e.to_string()))?;

            while let Some(row) = rows
                .next()
                .await
                .map_err(|e| BacktestError::Database(e.to_string()))?
            {
                let token_id: String = row
                    .get(0)
                    .map_err(|e| BacktestError::Database(e.to_string()))?;
                let timestamp: i64 = row
                    .get(1)
                    .map_err(|e| BacktestError::Database(e.to_string()))?;
                let price_str: String = row
                    .get(2)
                    .map_err(|e| BacktestError::Database(e.to_string()))?;
                let side_str: String = row
                    .get(3)
                    .map_err(|e| BacktestError::Database(e.to_string()))?;

                let price = price_str.parse::<Decimal>().map_err(|e| {
                    BacktestError::Database(format!("Failed to parse price: {}", e))
                })?;

                let bucket_start = (timestamp / fidelity) * fidelity;

                let bucket = token_buckets
                    .entry(token_id.clone())
                    .or_default()
                    .entry(bucket_start)
                    .or_insert(BucketAgg {
                        token_id: token_id.clone(),
                        last_price: price,
                        last_buy: None,
                        last_sell: None,
                    });

                bucket.last_price = price;

                // Use explicit side when available; fall back to price-movement heuristic
                let is_buy = match side_str.as_str() {
                    "buy" => true,
                    "sell" => false,
                    _ => {
                        let prev = prev_prices.get(&token_id).copied();
                        prev.is_none_or(|p| price >= p)
                    }
                };
                prev_prices.insert(token_id, price);

                if is_buy {
                    bucket.last_buy = Some(price);
                } else {
                    bucket.last_sell = Some(price);
                }
            }
        }

        // Convert buckets to SynthesizedBucket output
        let mut buckets = Vec::new();
        for per_token in token_buckets.values() {
            for (&bucket_start, agg) in per_token {
                let bucket_end = bucket_start + fidelity;

                let (best_bid, best_ask) = match (agg.last_sell, agg.last_buy) {
                    (Some(sell), Some(buy)) => (sell, buy),
                    (Some(sell), None) => (sell, sell + default_spread),
                    (None, Some(buy)) => ((buy - default_spread).max(Decimal::new(1, 2)), buy),
                    (None, None) => (
                        (agg.last_price - default_spread).max(Decimal::new(1, 2)),
                        agg.last_price + default_spread,
                    ),
                };

                buckets.push(SynthesizedBucket {
                    token_id: agg.token_id.clone(),
                    bucket_end,
                    last_price: agg.last_price,
                    best_bid,
                    best_ask,
                });
            }
        }

        Ok(buckets)
    }
}

/// Per-bucket trade aggregation (internal to `stream_trades_into_synthesis`).
struct BucketAgg {
    token_id: String,
    last_price: Decimal,
    last_buy: Option<Decimal>,
    last_sell: Option<Decimal>,
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

        store
            .insert_historical_prices(prices.clone())
            .await
            .unwrap();

        let retrieved = store
            .get_historical_prices(
                "token1",
                now - chrono::Duration::minutes(1),
                now + chrono::Duration::minutes(2),
            )
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

        store
            .insert_historical_trades(trades.clone())
            .await
            .unwrap();

        let retrieved = store
            .get_historical_trades(
                "token1",
                now - chrono::Duration::minutes(1),
                now + chrono::Duration::minutes(2),
            )
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
            .get_historical_prices(
                "token1",
                now - chrono::Duration::seconds(1),
                now + chrono::Duration::seconds(1),
            )
            .await
            .unwrap();

        // Should only have one entry (replaced)
        assert_eq!(retrieved.len(), 1);
        assert_eq!(retrieved[0].price, dec!(0.52));
    }
}
