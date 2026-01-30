# Backtesting Framework

## Overview
- Add a complete backtesting system to polyrust: historical data ingestion, DB caching, deterministic event replay engine, and results analysis
- **Problem**: No way to evaluate strategy performance on historical data before live/paper trading
- **Solution**: New `polyrust-backtest` crate with two subsystems:
  1. **Data pipeline** — fetch historical market data from CLOB API (last ~7 days) and Goldsky subgraphs (unlimited history), cache in Turso DB
  2. **Backtest engine** — dedicated synchronous engine that replays cached data through strategies, simulates fills using PaperBackend logic, and produces P&L reports
- Integrates with existing `Strategy` trait — any strategy works in backtest without modification

## Context (from discovery)
- **Existing architecture supports backtesting**: strategies are pure `on_event()` → `Vec<Action>` functions, deterministic given event stream + context
- **Fill mode**: Immediate only — historical orderbook depth is not available from any Polymarket API (CLOB orderbook is off-chain, not archived). Fills simulate at historical trade price.
- **Store layer** uses Turso/libsql with TEXT-based Decimal storage
- **DB separation**: two isolated databases:
  1. `backtest_data.db` — persistent historical data cache (prices, trades, markets, fetch log). Reused across runs.
  2. Fresh `Store` instance (`:memory:` or temp file) using the **existing** live schema (trades, orders, events, pnl_snapshots) — backtest writes simulated trades here, no new table schema needed. Disposed after run; report extracted first.
- **Config pattern** established — add `[backtest]` section following existing `[paper]`, `[arbitrage]` patterns
- **Data sources identified**:
  - CLOB API: `GET /prices-history` — price timeseries with `startTs`/`endTs`/`fidelity` params
  - Data API: `GET /trades` — trade events with market/event filtering, pagination (limit 10k)
  - Goldsky activity subgraph — on-chain trade fills, unlimited history
  - Gamma API — market metadata (slug, question, dates, token IDs)

## Development Approach
- **Testing approach**: Regular (code first, then tests)
- Complete each task fully before moving to the next
- Make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
- **CRITICAL: all tests must pass before starting next task** — no exceptions
- **CRITICAL: update this plan file when scope changes during implementation**
- Run tests after each change
- Maintain backward compatibility with existing crates

## Testing Strategy
- **Unit tests**: required for every task
- **Integration tests**: test data fetching with mock HTTP responses, test engine with synthetic event data
- **Live API tests**: mark with `#[ignore]`, test actual CLOB/subgraph fetching

## Progress Tracking
- Mark completed items with `[x]` immediately when done
- Add newly discovered tasks with ➕ prefix
- Document issues/blockers with ⚠️ prefix
- Update plan if implementation deviates from original scope
- Keep plan in sync with actual work done

## Implementation Steps

### Task 1: Scaffold polyrust-backtest crate
- [x] Create `crates/polyrust-backtest/` with `Cargo.toml` depending on `polyrust-core`, `polyrust-store`, `polyrust-execution`
- [x] Add workspace dependencies: `reqwest` (HTTP client), `serde_json`, `chrono`, `rust_decimal`, `tokio`, `tracing`
- [x] Add `polyrust-backtest` to workspace `Cargo.toml` members
- [x] Create module structure: `lib.rs`, `config.rs`, `data/mod.rs`, `engine/mod.rs`, `report/mod.rs`
- [x] Add placeholder public API: `BacktestConfig`, `BacktestEngine`, `DataFetcher`
- [x] Verify `cargo build --workspace` compiles cleanly
- [x] Write basic smoke test that instantiates BacktestConfig with defaults
- [x] Run `cargo test --workspace` — must pass

### Task 2: Define historical data cache DB schema
- [x] Create `HistoricalDataStore` struct in `polyrust-backtest` (separate from live `Store`)
  - Opens/creates `backtest_data.db` file (configurable path via `[backtest] data_db_path`)
  - This DB is persistent and reused across backtest runs
- [x] Create migration tables in `backtest_data.db`:
  - `historical_prices` — token_id, timestamp, price (TEXT/Decimal), source (clob/subgraph)
  - `historical_trades` — token_id, timestamp, price, size, side, tx_hash, source
  - `historical_markets` — market_id, slug, question, start_date, end_date, token_a, token_b, neg_risk
  - `data_fetch_log` — source, token_id, start_ts, end_ts, fetched_at, row_count (track what's cached)
- [x] Add indexes: (token_id, timestamp) on prices/trades, market_id on markets
- [x] Implement insert methods: `insert_historical_prices()`, `insert_historical_trades()`, `insert_historical_market()`
- [x] Implement query methods: `get_historical_prices(token_id, start, end)`, `get_historical_trades(token_id, start, end)`
- [x] Implement `get_fetch_log(source, token_id)` — check what date ranges are already cached
- [x] Write tests for all insert/query methods using in-memory Turso (`:memory:`)
- [x] Run `cargo test --workspace` — must pass

### Task 3: CLOB API data fetcher (last ~7 days)
- [x] Create `data/clob_fetcher.rs` — HTTP client for CLOB REST API
- [x] Implement `fetch_price_history(token_id, start_ts, end_ts, fidelity_mins)` → `Vec<HistoricalPrice>`
  - Endpoint: `GET https://clob.polymarket.com/prices-history?market={token_id}&startTs={}&endTs={}&fidelity={}`
  - Parse response: `{"history": [{"t": timestamp, "p": price}]}`
- [x] Implement `fetch_trades(market_id, limit, offset)` → `Vec<HistoricalTrade>`
  - Endpoint: `GET https://data-api.polymarket.com/trades?market={}&limit={}&offset={}`
  - Handle pagination (max 10k per request)
- [x] Add rate limiting / retry logic with exponential backoff
- [x] Implement cache-aware fetching: check `data_fetch_log` before fetching, skip already-cached ranges
- [x] Write tests with mock HTTP responses (use `wiremock` or similar)
- [x] Write `#[ignore]` live API test that fetches real price history for a known token
- [x] Run `cargo test --workspace` — must pass

### Task 4: Gamma API market discovery fetcher
- [x] Create `data/gamma_fetcher.rs` — fetch market metadata for backtesting
- [x] Implement `fetch_markets_by_slug(slug_pattern)` → `Vec<HistoricalMarket>`
  - Endpoint: `GET https://gamma-api.polymarket.com/markets?slug_contains={}`
  - Extract: market_id, slug, question, start_date, end_date, token_ids, neg_risk
- [x] Implement `fetch_market_by_id(condition_id)` → `Option<HistoricalMarket>`
- [x] Implement `fetch_expired_markets(coin, date_range)` — discover historical 15-min crypto markets
- [x] Cache results in `historical_markets` table
- [x] Write tests with mock HTTP responses
- [x] Write `#[ignore]` live test that discovers BTC 15-min markets
- [x] Run `cargo test --workspace` — must pass

### Task 5: Goldsky subgraph fetcher (unlimited history)
- [x] Create `data/subgraph_fetcher.rs` — GraphQL client for Goldsky subgraphs
- [x] Define GraphQL query structures for activity subgraph:
  - Endpoint: `https://api.goldsky.com/api/public/project_cl6mb8i9h0003e201j6li0diw/subgraphs/activity-subgraph/0.0.4/gn`
  - Query trade events by token/market/time range with pagination (`first`, `skip`, `where` filters)
- [x] Implement `fetch_subgraph_trades(token_id, start_ts, end_ts)` → `Vec<HistoricalTrade>`
  - Handle GraphQL pagination (subgraphs limit to 1000 results per query, use `skip` or `id_gt` cursor)
- [x] Implement `fetch_subgraph_positions(market_id)` — optional, for volume/liquidity context
- [x] Add cache-aware logic: merge subgraph data with existing DB cache, avoid duplicates by tx_hash
- [x] Write tests with mock GraphQL responses
- [x] Write `#[ignore]` live test that fetches trades from activity subgraph
- [x] Run `cargo test --workspace` — must pass

### Task 6: Unified DataFetcher with smart caching
- [x] Create `data/fetcher.rs` — orchestrates CLOB, Gamma, and subgraph fetchers
- [x] Implement `DataFetcher::new(store: Arc<Store>, config: DataFetchConfig)`
- [x] Implement `fetch_market_data(market_id, start, end)` — smart routing:
  - If date range within last 7 days → use CLOB API (higher resolution)
  - If date range older than 7 days → use Goldsky subgraph
  - Check `data_fetch_log` first, only fetch missing ranges
  - Merge overlapping data, deduplicate by timestamp
- [x] Implement `fetch_and_cache(token_ids, date_range)` — bulk fetch for backtest preparation
- [x] Implement `get_cached_data(token_id, start, end)` → `CachedMarketData` (prices + trades from DB)
- [x] Add progress reporting via `tracing` (log fetching progress for long historical pulls)
- [x] Write tests for smart routing logic (mock both API sources)
- [x] Write test for cache hit/miss behavior
- [x] Run `cargo test --workspace` — must pass

### Task 7: BacktestConfig and CLI integration
- [x] Define `BacktestConfig` in `config.rs`:
  - `strategy_name: String` — which strategy to backtest
  - `market_ids: Vec<String>` — markets to include (or discover by pattern)
  - `start_date: DateTime<Utc>`, `end_date: DateTime<Utc>` — backtest window
  - `initial_balance: Decimal` — starting USDC
  - `data_fidelity_mins: u64` — price history granularity in minutes (default 1 min)
  - `data_db_path: String` — path to persistent historical data cache (default `backtest_data.db`)
  - `fee_model: FeeConfig` — reuse existing fee config
- [x] Add `[backtest]` section to `config.example.toml`
- [x] Parse from TOML with `#[derive(Deserialize)]` and `#[serde(default)]` for optional fields
- [x] Support env overrides: `POLY_BACKTEST_START`, `POLY_BACKTEST_END`, etc.
- [x] Write tests for config parsing (valid, defaults, env overrides)
- [x] Run `cargo test --workspace` — must pass

### Task 8: BacktestEngine — deterministic event replay
- [x] Create `engine/mod.rs` with `BacktestEngine` struct
- [x] Implement `BacktestEngine::new(config, strategy, data_store, store)` — initializes:
  - `data_store`: `HistoricalDataStore` — reads cached historical data
  - `store`: fresh `Store` instance (`:memory:`) using existing live schema — receives simulated trades/orders
  - `StrategyContext` with initial balance and empty positions
  - Simulated clock starting at `config.start_date`
- [x] Implement `run(&mut self)` — main synchronous event loop:
  1. Load cached data from DB for configured market_ids and date range
  2. Sort all events chronologically (prices + trades → unified timeline)
  3. For each event in order:
     a. Advance simulated clock to event timestamp
     b. Convert DB record to `Event` (PriceChange, Trade)
     c. Update `StrategyContext.market_data` with new data
     d. Call `strategy.on_event(&event, &ctx)` → collect `Vec<Action>`
     e. Execute actions: immediate fill at current market price, apply fee model
     f. Update positions, balance, emit fill events back to strategy
     g. Record trade in backtest results
  4. After all events: call `strategy.on_stop()`, finalize results
- [x] Implement immediate fill logic:
  - Fill at current market price (latest price from historical data)
  - Fee calculation using configured fee model
  - No orderbook depth simulation (historical orderbooks not available from Polymarket APIs)
- [x] Handle market expiration events (MarketExpired at end_date)
- [x] Write tests with synthetic event data:
  - Test single buy order fills correctly
  - Test strategy receives events in chronological order
  - Test position tracking through multiple fills
- [x] Run `cargo test --workspace` — must pass

### Task 9: Backtest results and reporting
- [x] Create `report/mod.rs` with `BacktestReport` struct:
  - `trades: Vec<BacktestTrade>` — all simulated trades with timestamps, prices, P&L
  - `total_pnl: Decimal`, `realized_pnl: Decimal`, `unrealized_pnl: Decimal`
  - `win_rate: Decimal` — winning trades / total trades
  - `max_drawdown: Decimal` — peak-to-trough equity decline
  - `sharpe_ratio: Option<Decimal>` — if enough data points
  - `total_trades: usize`, `winning_trades: usize`, `losing_trades: usize`
  - `start_balance: Decimal`, `end_balance: Decimal`
  - `duration: chrono::Duration`
- [x] Implement `BacktestReport::from_engine_results()` — compute all metrics from trade history
- [x] Implement `report.summary()` → formatted String for terminal output
- [x] Implement `report.to_json()` → serde_json::Value for programmatic use
- [x] Extract report from in-memory `Store` (query trades, orders, pnl_snapshots using existing Store API)
- [x] Optionally persist report summary to `backtest_runs` table in `backtest_data.db` (for comparing runs across sessions)
- [x] Write tests for metric calculations (known trade sequences → expected metrics)
- [x] Run `cargo test --workspace` — must pass

### Task 10: Integration — wire backtest into main binary
- [x] Add `backtest` subcommand or `--backtest` flag to `src/main.rs`
- [x] When backtest mode: load `[backtest]` config, open `HistoricalDataStore` (persistent), create fresh `:memory:` `Store`, instantiate `DataFetcher`, check/fetch data, run `BacktestEngine`, print report
- [x] Add `polyrust-backtest` dependency to root `Cargo.toml`
- [x] Create `examples/run_backtest.rs` — minimal example running crypto arb strategy on historical data
- [x] Write integration test: full pipeline from config → data fetch (mocked) → engine run → report
- [x] Run `cargo test --workspace` — must pass

### Task 11: Verify acceptance criteria
- [ ] Verify data fetching works for both CLOB API and subgraph sources
- [ ] Verify DB caching prevents re-fetching already-cached data
- [ ] Verify BacktestEngine replays events deterministically (same input → same output)
- [ ] Verify existing strategies work in backtest without modification
- [ ] Verify backtest report metrics are accurate against known trade sequences
- [ ] Run full test suite: `cargo test --workspace`
- [ ] Run clippy: `cargo clippy --workspace -- -D warnings`
- [ ] Verify test coverage for new crate

### Task 12: [Final] Update documentation
- [ ] Update CLAUDE.md with backtest module architecture, commands, and config
- [ ] Update `config.example.toml` with complete `[backtest]` section and comments
- [ ] Add backtest commands to Build & Test Commands section

## Technical Details

### Data Flow
```
                    ┌─────────────────┐
                    │  CLOB API       │ (last ~7 days)
                    │  /prices-history│ price timeseries
                    │  /trades        │ trade events
                    └────────┬────────┘
                             │
┌─────────────────┐          ▼          ┌──────────────┐
│  Gamma API      │──▶ DataFetcher ◀───│ Goldsky      │ (unlimited)
│  /markets       │    (smart cache)    │ Subgraphs    │ activity, trades
└─────────────────┘          │          └──────────────┘
                             ▼
                    ┌─────────────────┐
                    │ backtest_data.db│  persistent cache
                    │ (historical_*)  │  reused across runs
                    └────────┬────────┘
                             │ read
                             ▼
                    ┌─────────────────┐     ┌──────────────────┐
                    │ BacktestEngine  │────▶│ Store (:memory:) │
                    │ (synchronous)   │     │ existing schema   │
                    │  ┌───────────┐  │     │ trades, orders,   │
                    │  │ Strategy  │  │     │ events, pnl_*     │
                    │  └───────────┘  │     └────────┬─────────┘
                    │  ┌───────────┐  │              │ query
                    │  │ Fill Sim  │  │              ▼
                    │  └───────────┘  │     ┌──────────────────┐
                    └─────────────────┘     │ BacktestReport   │
                                            │ P&L, metrics     │
                                            └──────────────────┘
```

### DB Architecture

**Two separate databases — live trading DB is never touched:**

1. **`backtest_data.db`** (persistent, configurable path) — historical data cache with custom schema:

```sql
-- Price timeseries (from CLOB /prices-history or subgraph)
historical_prices (
    token_id TEXT, timestamp INTEGER, price TEXT,
    source TEXT,  -- 'clob' | 'subgraph'
    PRIMARY KEY (token_id, timestamp, source)
)

-- Individual trade events
historical_trades (
    id TEXT PRIMARY KEY,  -- tx_hash or synthetic ID
    token_id TEXT, timestamp INTEGER,
    price TEXT, size TEXT, side TEXT,
    source TEXT
)

-- Market metadata
historical_markets (
    market_id TEXT PRIMARY KEY,
    slug TEXT, question TEXT,
    start_date TEXT, end_date TEXT,
    token_a TEXT, token_b TEXT,
    neg_risk INTEGER
)

-- Fetch tracking (avoid re-fetching)
data_fetch_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT, token_id TEXT,
    start_ts INTEGER, end_ts INTEGER,
    fetched_at TEXT, row_count INTEGER
)
```

2. **`:memory:` Store** (ephemeral, per-run) — uses the **existing** `polyrust-store` schema as-is:
   - `trades`, `orders`, `events`, `pnl_snapshots` — same tables as live trading
   - BacktestEngine writes simulated trades/orders here using the regular `Store` API
   - Report extracts metrics from this DB before it's disposed
   - No schema changes needed in `polyrust-store`

### Config Example
```toml
[backtest]
strategy = "crypto-arb"
market_ids = []  # empty = auto-discover via Gamma API
coins = ["BTC", "ETH"]  # used for market discovery
start_date = "2025-01-01T00:00:00Z"
end_date = "2025-01-31T00:00:00Z"
initial_balance = 1000.00
data_fidelity_mins = 1
data_db_path = "backtest_data.db"  # persistent cache, reused across runs
# Fill mode is always Immediate — historical orderbook depth not available from Polymarket APIs

[backtest.fees]
taker_rate = 0.0315  # reuse fee model
```

### Key Design Decisions
1. **Two-DB architecture** — `backtest_data.db` (persistent historical cache) + fresh `:memory:` Store (existing schema for simulated trades). Live trading DB is never touched.
2. **Synchronous engine** — no tokio runtime needed for backtest loop; deterministic by design
3. **Immediate fills only** — no historical orderbook depth available from any Polymarket API (CLOB orderbook is off-chain, not archived). Fills at current market price with fee model applied.
4. **Source-aware caching** — `data_fetch_log` tracks what's been fetched per source/token/range, prevents duplicate API calls
5. **Smart routing** — DataFetcher automatically picks CLOB (recent, high-fidelity) vs subgraph (historical, lower-fidelity) based on date range
6. **Strategy-agnostic** — any `impl Strategy` works in backtest without modification
7. **Zero schema changes to polyrust-store** — backtest reuses existing Store as-is for trade recording; only `HistoricalDataStore` has new tables

## Post-Completion

**Manual verification:**
- Run backtest against known historical period with crypto arb strategy, verify P&L makes sense
- Test data fetching with various date ranges spanning the 7-day CLOB boundary
- Verify subgraph pagination handles markets with >1000 trades

**Future enhancements (not in scope):**
- Backtest results dashboard view (HTMX page with charts)
- Multi-strategy parallel backtesting
- Walk-forward optimization
- Orderbook-mode fills (requires live orderbook snapshot recorder — capture from WebSocket going forward)
- Slippage modeling
