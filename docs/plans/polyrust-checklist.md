# Plan: Build Polyrust Trading Bot Framework

## Overview
Build an autonomous Polymarket trading bot framework in Rust with modular event-driven architecture, trait-based strategy plugins, paper trading, Turso persistence, Axum+HTMX dashboard, and a crypto arbitrage reference strategy. Uses `rs-clob-client` (polymarket-client-sdk v0.4.1) for all Polymarket interactions.

Design doc: [`docs/brainstorms/polyrust-trading-framework.md`](../brainstorms/polyrust-trading-framework.md)
Detailed plan: [`docs/plans/polyrust-framework-implementation.md`](./polyrust-framework-implementation.md)

## Validation Commands
- `cargo build --workspace`
- `cargo test --workspace`
- `cargo clippy --workspace -- -D warnings`
- `cargo run --example simple_strategy`

---

## Milestone 1: Workspace Scaffolding & Core Types

### Task 1: Create Cargo workspace with 6 crates
> **Detailed reference:** [polyrust-framework-implementation.md → Task 1](./polyrust-framework-implementation.md#task-1-create-cargo-workspace) — full Cargo.toml contents, crate Cargo.toml template, .gitignore
- [x] Create workspace `Cargo.toml` with `resolver = "2"`, all 6 members, and `[workspace.dependencies]` for shared versions (tokio 1, serde 1, tracing 0.1, thiserror 2, async-trait 0.1, chrono 0.4, rust_decimal 1, uuid 1, tokio-stream 0.1, polymarket-client-sdk 0.4 with features: clob, ws, rtds, data, gamma, tracing, heartbeats, ctf)
- [x] Create `crates/polyrust-core/Cargo.toml` + `src/lib.rs` — engine, event bus, traits, shared state
- [x] Create `crates/polyrust-market/Cargo.toml` + `src/lib.rs` — market data feeds
- [x] Create `crates/polyrust-execution/Cargo.toml` + `src/lib.rs` — live + paper execution backends
- [x] Create `crates/polyrust-store/Cargo.toml` + `src/lib.rs` — Turso persistence
- [x] Create `crates/polyrust-dashboard/Cargo.toml` + `src/lib.rs` — Axum + HTMX monitoring
- [x] Create `crates/polyrust-strategies/Cargo.toml` + `src/lib.rs` — reference strategy implementations
- [x] Create `src/main.rs` with placeholder `fn main() { println!("polyrust"); }`
- [x] Create `.gitignore` (target/, *.db, *.db-journal, .env, config/local.toml)
- [x] Verify `cargo build --workspace` compiles with zero errors
- [x] Mark completed

### Task 2: Define core domain types in polyrust-core
> **Detailed reference:** [polyrust-framework-implementation.md → Task 2](./polyrust-framework-implementation.md#task-2-define-core-domain-types) — full type definitions, serde attributes, method implementations, test code
- [x] Create `crates/polyrust-core/src/types.rs` with type aliases: `MarketId = String`, `TokenId = String`, `OrderId = String`
- [x] Add enum `OutcomeSide { Up, Down, Yes, No }` with serde rename_all lowercase, derive Hash
- [x] Add enum `OrderSide { Buy, Sell }` with serde rename_all UPPERCASE
- [x] Add enum `OrderType { Gtc, Gtd, Fok }` with serde rename_all UPPERCASE
- [x] Add struct `OrderRequest { token_id, price: Decimal, size: Decimal, side, order_type, neg_risk: bool }`
- [x] Add struct `OrderResult { success: bool, order_id: Option, status: Option, message: String }`
- [x] Add struct `Order { id, token_id, side, price, size, filled_size, status: OrderStatus, created_at }`
- [x] Add enum `OrderStatus { Open, Filled, PartiallyFilled, Cancelled, Expired }` with SCREAMING_SNAKE_CASE
- [x] Add struct `Position { id: Uuid, market_id, token_id, side: OutcomeSide, entry_price, size, current_price, entry_time, strategy_name }` with `unrealized_pnl()` method
- [x] Add struct `Trade { id: Uuid, order_id, market_id, token_id, side, price, size, realized_pnl: Option, strategy_name, timestamp }`
- [x] Add struct `OrderbookLevel { price, size }` and `OrderbookSnapshot { token_id, bids, asks, timestamp }` with methods: `best_bid()`, `best_ask()`, `mid_price()`, `spread()`
- [x] Add struct `MarketInfo { id, slug, question, end_date, token_ids: TokenIds, accepting_orders, neg_risk }` with methods: `has_ended()`, `seconds_remaining()`
- [x] Add struct `TokenIds { outcome_a, outcome_b }`
- [x] Create `crates/polyrust-core/src/error.rs` with `PolyError` enum (Config, Execution, MarketData, Storage, Strategy, EventBus, Sdk, Other) and `type Result<T>`
- [x] Update `lib.rs` with module declarations and `prelude` module re-exporting all public types
- [x] Add `rust_decimal_macros = "1"` to workspace dev-dependencies
- [x] Write `crates/polyrust-core/tests/types_test.rs` — test `mid_price()` returns `(bid+ask)/2`, `unrealized_pnl()` returns `(current-entry)*size`, `spread()` returns `ask-bid`, serde roundtrip for `OrderRequest`
- [x] Verify `cargo test --workspace` passes
- [x] Mark completed

### Task 3: Define core traits — Strategy, ExecutionBackend, events, actions, context
> **Detailed reference:** [polyrust-framework-implementation.md → Task 3](./polyrust-framework-implementation.md#task-3-define-core-traits-strategy-executionbackend-marketdatafeed) — full Event enum, Action enum, StrategyContext, Strategy trait, ExecutionBackend trait with code
- [x] Create `crates/polyrust-core/src/events.rs` — `Event` enum with variants: `MarketData(MarketDataEvent)`, `OrderUpdate(OrderEvent)`, `PositionChange(PositionEvent)`, `Signal(SignalEvent)`, `System(SystemEvent)` plus `topic()` method returning `&'static str`
- [x] Define `MarketDataEvent` — variants: `OrderbookUpdate(OrderbookSnapshot)`, `PriceChange { token_id, price, side, best_bid, best_ask }`, `Trade { token_id, price, size, timestamp }`, `ExternalPrice { symbol, price, source, timestamp }`, `MarketDiscovered(MarketInfo)`, `MarketExpired(MarketId)`
- [x] Define `OrderEvent` — variants: `Placed(OrderResult)`, `Filled { order_id, token_id, price, size }`, `PartiallyFilled { order_id, filled_size, remaining_size }`, `Cancelled(OrderId)`, `Rejected { order_id: Option, reason }`
- [x] Define `PositionEvent` — variants: `Opened(Position)`, `Closed { position_id, realized_pnl }`, `Updated(Position)`
- [x] Define `SignalEvent { strategy_name, signal_type, payload: serde_json::Value, timestamp }`
- [x] Define `SystemEvent` — variants: `EngineStarted`, `EngineStopping`, `StrategyStarted(String)`, `StrategyStopped(String)`, `Error { source, message }`, `HealthCheck { strategies_active, positions_open, uptime_seconds }`
- [x] Create `crates/polyrust-core/src/actions.rs` — `Action` enum: `PlaceOrder(OrderRequest)`, `CancelOrder(OrderId)`, `CancelAllOrders`, `Log { level: LogLevel, message }`, `EmitSignal { signal_type, payload }`, `SubscribeMarket(MarketId)`, `UnsubscribeMarket(MarketId)` plus `LogLevel` enum
- [x] Create `crates/polyrust-core/src/context.rs` — `StrategyContext` with `Arc<RwLock<PositionState>>` (open_positions HashMap, open_orders HashMap, position_count(), positions_for_strategy(), total_unrealized_pnl()), `Arc<RwLock<MarketDataState>>` (orderbooks, markets, external_prices), `Arc<RwLock<BalanceState>>` (available_usdc, locked_usdc)
- [x] Create `crates/polyrust-core/src/strategy.rs` — `Strategy` trait with: `fn name() -> &str`, `fn description() -> &str`, `async fn on_start()`, `async fn on_event() -> Result<Vec<Action>>`, `async fn on_stop()` (default impls for start/stop)
- [x] Create `crates/polyrust-core/src/execution.rs` — `ExecutionBackend` trait with: `place_order()`, `cancel_order()`, `cancel_all_orders()`, `get_open_orders()`, `get_positions()`, `get_balance()`
- [x] Update `lib.rs` and `prelude` with all new modules: actions, context, events, execution, strategy
- [x] Verify `cargo build --workspace` compiles
- [x] Mark completed

---

## Milestone 2: Event Bus & Engine

### Task 4: Implement typed event bus with topic filtering
> **Detailed reference:** [polyrust-framework-implementation.md → Task 4](./polyrust-framework-implementation.md#task-4-implement-typed-event-bus) — full EventBus and EventSubscriber implementation with broadcast channel, topic filtering, lag handling
- [x] Create `crates/polyrust-core/src/event_bus.rs` — `EventBus` struct wrapping `broadcast::Sender<Event>` with const `DEFAULT_CAPACITY = 4096`
- [x] Implement `EventBus::new()`, `with_capacity(usize)`, `Default` trait
- [x] Implement `publish(Event)` — sends via broadcast, logs topic + receiver count, warns on no subscribers
- [x] Implement `subscribe()` → `EventSubscriber` (receives all events)
- [x] Implement `subscribe_topics(&[&str])` → `EventSubscriber` (filtered by topic strings)
- [x] Implement `subscriber_count()` → `usize`
- [x] Implement `EventSubscriber::recv()` — loops on `broadcast::Receiver::recv()`, filters by topic if set, handles `Lagged` (warn + continue) and `Closed` (return None)
- [x] Add `event_bus` module to `lib.rs` and `EventBus` to prelude
- [x] Write `crates/polyrust-core/tests/event_bus_test.rs`:
  - Test: publish MarketData event to 2 subscribers, both receive it
  - Test: topic-filtered subscriber for "market_data" does NOT receive System events
  - Test: publish with zero subscribers does not panic
  - Test: subscriber handles lag gracefully (publish > capacity events, subscriber recovers)
- [x] Verify `cargo test --workspace -- event_bus` passes
- [x] Mark completed

### Task 5: Implement engine core with builder pattern and lifecycle
> **Detailed reference:** [polyrust-framework-implementation.md → Task 5](./polyrust-framework-implementation.md#task-5-implement-engine-core) — full Config struct with TOML/env parsing, EngineBuilder, Engine::run() with strategy dispatch loop, execute_action() helper
- [x] Create `crates/polyrust-core/src/config.rs` — `Config` struct with sections: `EngineConfig` (event_bus_capacity: 4096, health_check_interval_secs: 30), `PolymarketConfig` (private_key, safe_address, builder API creds — all Option), `DashboardConfig` (enabled: true, port: 3000, host: "127.0.0.1"), `StoreConfig` (db_path: "polyrust.db"), `PaperConfig` (enabled: false, initial_balance: 10000)
- [x] Implement `Config::from_file(path)` — reads TOML, returns Result
- [x] Implement `Config::with_env_overrides()` — overrides from POLY_PRIVATE_KEY, POLY_SAFE_ADDRESS, POLY_BUILDER_API_KEY, POLY_BUILDER_API_SECRET, POLY_BUILDER_API_PASSPHRASE, POLY_DASHBOARD_PORT, POLY_DB_PATH, POLY_PAPER_TRADING
- [x] Add `toml = "0.8"` to workspace dependencies and polyrust-core
- [x] Create `crates/polyrust-core/src/engine.rs` — `EngineBuilder` struct with `new()`, `config(Config)`, `strategy(impl Strategy + 'static)`, `execution(impl ExecutionBackend + 'static)`, `async build() -> Result<Engine>`
- [x] Implement `EngineBuilder::build()` — validates execution backend present, creates EventBus with config capacity, creates StrategyContext, queries initial balance from backend
- [x] Implement `Engine` struct with fields: config, event_bus, strategies (Vec<Arc<RwLock<Box<dyn Strategy>>>>), execution (Arc<dyn ExecutionBackend>), context (StrategyContext), start_time
- [x] Implement `Engine::builder()`, `event_bus()`, `context()`, `config()` accessors
- [x] Implement `Engine::run()`:
  1. Set start_time, log engine starting
  2. Call `on_start()` on each strategy, publish StrategyStarted events
  3. Publish EngineStarted event
  4. Spawn tokio task per strategy: loop recv from EventBus, call `on_event()`, execute returned Actions via `execute_action()` helper, break on EngineStopping
  5. Wait for `tokio::signal::ctrl_c()`
  6. Publish EngineStopping, call `on_stop()` per strategy, await task handles
- [x] Implement `execute_action()` helper — match on Action variants: PlaceOrder → backend.place_order() + publish Placed event, CancelOrder → backend.cancel_order() + publish Cancelled, CancelAllOrders → backend.cancel_all_orders(), Log → tracing macro, EmitSignal → publish Signal event, Subscribe/Unsubscribe → warn not yet implemented
- [x] Add `config` and `engine` modules to `lib.rs`, add `Config` and `Engine` to prelude
- [x] Write tests:
  - Test: EngineBuilder without execution backend returns PolyError::Config
  - Test: Config::from_file with valid TOML string (write temp file) parses correctly
  - Test: Config env overrides apply (set env vars in test, verify config fields)
- [x] Verify `cargo test --workspace` passes
- [x] Mark completed

---

## Milestone 3: Persistence Layer (Turso)

### Task 6: Implement Turso persistence layer
> **Detailed reference:** [polyrust-framework-implementation.md → Task 6](./polyrust-framework-implementation.md#task-6-implement-turso-store) — full Store struct, migration SQL, StoreError, libsql fallback note, CRUD method patterns
- [x] Add dependencies to `crates/polyrust-store/Cargo.toml`: polyrust-core, turso (or libsql as fallback), serde, serde_json, chrono, rust_decimal, uuid, tracing, thiserror
- [x] Create `crates/polyrust-store/src/error.rs` — `StoreError` enum (Connection, Migration, Query) + `StoreResult<T>` type alias
- [x] Create `crates/polyrust-store/src/db.rs` — `Store` struct wrapping turso::Database
- [x] Implement `Store::new(path)` — `Builder::new_local(path).build().await`, then `run_migrations()`
- [x] Implement `Store::connect()` → turso::Connection
- [x] Implement `run_migrations()` — CREATE TABLE IF NOT EXISTS for: `trades` (id TEXT PK, order_id, market_id, token_id, side, price TEXT, size TEXT, realized_pnl TEXT nullable, strategy_name, timestamp, created_at), `orders` (id TEXT PK, token_id, side, price TEXT, size TEXT, filled_size TEXT default '0', status, strategy_name, created_at, updated_at), `events` (id INTEGER PK AUTOINCREMENT, event_type, topic, payload TEXT, timestamp), `pnl_snapshots` (id INTEGER PK AUTOINCREMENT, total_pnl TEXT, unrealized_pnl TEXT, realized_pnl TEXT, open_positions INT, open_orders INT, available_balance TEXT, timestamp) with indexes on trades(strategy_name), trades(timestamp), orders(status), events(topic), pnl_snapshots(timestamp)
- [x] Create `crates/polyrust-store/src/trades.rs` — `insert_trade(conn, &Trade)`, `get_trade(conn, id) -> Option<Trade>`, `list_trades(conn, strategy: Option<&str>, limit: usize) -> Vec<Trade>`
- [x] Create `crates/polyrust-store/src/orders.rs` — `insert_order(conn, &Order)`, `get_order(conn, id) -> Option<Order>`, `update_order_status(conn, id, status)`, `list_orders(conn, status: Option<OrderStatus>, limit: usize) -> Vec<Order>`
- [x] Create `crates/polyrust-store/src/events.rs` — `insert_event(conn, &Event)` (serialize Event to JSON payload), `list_events(conn, topic: Option<&str>, limit: usize) -> Vec<StoredEvent>`
- [x] Create `crates/polyrust-store/src/snapshots.rs` — `insert_snapshot(conn, &PnlSnapshot)`, `list_snapshots(conn, limit: usize) -> Vec<PnlSnapshot>`, `latest_snapshot(conn) -> Option<PnlSnapshot>`
- [x] Update `crates/polyrust-store/src/lib.rs` with module declarations and public re-exports
- [x] Store `Decimal` values as TEXT in SQLite for precision (store via `.to_string()`, parse via `Decimal::from_str()`)
- [x] Write tests using in-memory database (`":memory:"`):
  - Test: migrations are idempotent (run Store::new twice, no error)
  - Test: insert_trade + get_trade roundtrip preserves all fields including Decimal precision
  - Test: insert_order + update_order_status + get_order shows updated status
  - Test: insert_event + list_events with topic filter returns only matching events
  - Test: insert_snapshot + latest_snapshot returns most recent
  - Test: list_trades with strategy filter returns only matching strategy's trades
- [x] Verify `cargo test --workspace` passes
- [x] Mark completed

---

## Milestone 4: Market Data & Execution Backends

### Task 7: Implement market data feeds (CLOB orderbook + RTDS prices)
> **Detailed reference:** [polyrust-framework-implementation.md → Task 7](./polyrust-framework-implementation.md#task-7-implement-market-data-feeds) — MarketDataFeed trait, ClobFeed/PriceFeed implementation notes, OrderbookManager, Python reference files
- [x] Add dependencies to `crates/polyrust-market/Cargo.toml`: polyrust-core, polymarket-client-sdk (workspace), tokio, tracing, thiserror, async-trait
- [x] Create `crates/polyrust-market/src/feed.rs` — `MarketDataFeed` trait: `async fn start(&mut self, event_bus: EventBus)`, `async fn subscribe_market(&mut self, market: &MarketInfo)`, `async fn unsubscribe_market(&mut self, market_id: &str)`, `async fn stop(&mut self)`
- [x] Create `crates/polyrust-market/src/clob_feed.rs` — `ClobFeed` struct implementing MarketDataFeed
  - Connect to Polymarket WebSocket via rs-clob-client `ws` feature
  - Subscribe to token IDs for orderbook updates
  - Parse WS messages into `OrderbookSnapshot` structs
  - Publish `Event::MarketData(MarketDataEvent::OrderbookUpdate(...))` to EventBus
  - Handle reconnection with exponential backoff (reference: `../polymarket-trading-bot/src/websocket_client.py`)
- [x] Create `crates/polyrust-market/src/price_feed.rs` — `PriceFeed` struct implementing MarketDataFeed
  - Connect to RTDS WebSocket via rs-clob-client `rtds` feature
  - Subscribe to `crypto_prices_chainlink` topic (Polymarket resolution source) and `crypto_prices` (Binance, faster)
  - Parse into `Event::MarketData(MarketDataEvent::ExternalPrice { symbol, price, source, timestamp })`
  - Maintain thread-safe price cache `Arc<RwLock<HashMap<String, (Decimal, DateTime<Utc>)>>>`
- [x] Create `crates/polyrust-market/src/orderbook.rs` — `OrderbookManager` maintaining latest `OrderbookSnapshot` per token_id with `Arc<RwLock<HashMap<TokenId, OrderbookSnapshot>>>`, update-on-event, `get_snapshot()`, `get_mid_price()`
- [x] Update `crates/polyrust-market/src/lib.rs` with module declarations and public exports
- [x] Write tests:
  - Test: OrderbookManager updates snapshot and returns correct mid_price
  - Test: OrderbookManager handles missing token_id gracefully
  - Integration tests for live WS connections marked `#[ignore]`
- [x] Verify `cargo test --workspace` passes
- [x] Mark completed

### Task 8: Implement live execution backend (rs-clob-client)
> **Detailed reference:** [polyrust-framework-implementation.md → Task 8](./polyrust-framework-implementation.md#task-8-implement-live-execution-backend) — LiveBackend struct, SDK auth wiring, order mapping, tick size rounding, Python reference (bot.py, client.py)
- [x] Add dependencies to `crates/polyrust-execution/Cargo.toml`: polyrust-core, polymarket-client-sdk (workspace), tokio, tracing, thiserror, async-trait
- [x] Create `crates/polyrust-execution/src/live.rs` — `LiveBackend` struct wrapping authenticated rs-clob-client Client
- [x] Implement `LiveBackend::new(config: &Config)` — create signer from private_key, build Client with authentication_builder, authenticate (support EOA and GnosisSafe signature types based on config)
- [x] Implement `ExecutionBackend::place_order()` — build limit_order or market_order via client builder, sign with signer, post_order, map response to OrderResult
- [x] Implement `ExecutionBackend::cancel_order()` — delegate to client cancel
- [x] Implement `ExecutionBackend::cancel_all_orders()` — delegate to client cancel_all
- [x] Implement `ExecutionBackend::get_open_orders()` — query client, map SDK Order types to domain Order types
- [x] Implement `ExecutionBackend::get_positions()` — use data API feature, map to domain Position types
- [x] Implement `ExecutionBackend::get_balance()` — query USDC balance via SDK
- [x] Handle tick size rounding per market (reference: Python `src/signer.py` ROUNDING_CONFIG — 0.01 tick = 2 decimal price, 2 decimal size, 4 decimal amount)
- [x] Update `crates/polyrust-execution/src/lib.rs` with module declaration and public exports
- [x] Write tests:
  - Test: OrderRequest → SDK order type mapping is correct (price, size, side, order_type)
  - Test: SDK response → OrderResult mapping handles success and failure cases
  - Test: tick size rounding applied correctly for 0.01, 0.001 tick sizes
  - Live tests marked `#[ignore]`
- [x] Verify `cargo test --workspace` passes
- [x] Mark completed

### Task 9: Implement paper trading execution backend
> **Detailed reference:** [polyrust-framework-implementation.md → Task 9](./polyrust-framework-implementation.md#task-9-implement-paper-execution-backend) — PaperBackend/PaperState structs, FillMode enum, fill logic, Python reference (paper/engine.py)
- [x] Create `crates/polyrust-execution/src/paper.rs` — `PaperBackend` struct with `Arc<RwLock<PaperState>>` and `FillMode` enum (Immediate, Orderbook)
- [x] Define `PaperState` — `usdc_balance: Decimal`, `positions: HashMap<TokenId, Decimal>` (share count), `open_orders: HashMap<OrderId, PaperOrder>`
- [x] Define `PaperOrder` — id (UUID), token_id, side, price, size, filled_size, status, created_at
- [x] Implement `PaperBackend::new(initial_balance: Decimal, fill_mode: FillMode)`
- [x] Implement `place_order()`:
  - BUY: validate `usdc_balance >= price * size`, deduct balance, add to positions (Immediate mode) or add to open_orders (Orderbook mode)
  - SELL: validate `positions[token_id] >= size`, deduct position, add USDC revenue
  - Generate synthetic OrderId via UUID
  - Return OrderResult with success/failure
- [x] Implement `cancel_order()` — remove from open_orders, restore locked balance
- [x] Implement `cancel_all_orders()` — cancel all, restore all locked balance
- [x] Implement `get_open_orders()` — return open_orders values mapped to Order
- [x] Implement `get_positions()` — return positions mapped to Position structs
- [x] Implement `get_balance()` — return usdc_balance
- [x] Implement `update_orders_with_orderbook(token_id, orderbook: &OrderbookSnapshot)` — for Orderbook fill mode: match pending BUY orders against asks, SELL orders against bids, update filled_size, emit fills
- [x] Export PaperBackend and FillMode from `crates/polyrust-execution/src/lib.rs`
- [x] Write tests:
  - Test: BUY order with sufficient balance succeeds, balance reduced by price*size, position created
  - Test: BUY order with insufficient balance fails with error, balance unchanged
  - Test: SELL order with sufficient position succeeds, position reduced, balance increased
  - Test: SELL order with no position fails with error
  - Test: cancel_order restores locked balance
  - Test: cancel_all_orders cancels all open orders
  - Test: Orderbook fill mode matches BUY at ask price, SELL at bid price
  - Test: Immediate fill mode fills instantly at order price
  - Test: partial fill tracking — order partially matched against orderbook
- [x] Verify `cargo test --workspace` passes
- [x] Mark completed

---

## Milestone 5: Dashboard

### Task 10: Implement Axum + HTMX monitoring dashboard
> **Detailed reference:** [polyrust-framework-implementation.md → Task 10](./polyrust-framework-implementation.md#task-10-implement-axum--htmx-monitoring-dashboard) — Dashboard struct, Axum router setup, handler signatures, SSE endpoint, Askama template structure, HTMX attributes
- [x] Add dependencies to `crates/polyrust-dashboard/Cargo.toml`: polyrust-core, polyrust-store, axum 0.8, askama 0.13, askama_axum 0.5, tower-http 0.6 (features: fs, cors), tokio, tokio-stream, serde, serde_json, tracing
- [x] Create `crates/polyrust-dashboard/src/server.rs` — `Dashboard` struct with StrategyContext, Arc<Store>, EventBus
- [x] Implement `Dashboard::new(context, store, event_bus)` and `async fn serve(self, host: &str, port: u16)`
- [x] Define `AppState` (Clone) with Arc references to context, store, event_bus for Axum state extraction
- [x] Set up Axum Router with routes: `GET /` (index), `GET /positions` (positions table), `GET /trades` (trade history), `GET /health` (system health), `GET /events/stream` (SSE endpoint)
- [x] Create `crates/polyrust-dashboard/src/handlers.rs`:
  - `index` handler — render overview page: active strategy count, total PnL, unrealized PnL, available balance, system uptime
  - `positions` handler — render table of open positions from StrategyContext (id, market, side, entry_price, current_price, unrealized_pnl, strategy_name)
  - `trades` handler — query Store for recent trades, render paginated table (id, market, side, price, size, realized_pnl, timestamp)
  - `health` handler — render system health: uptime, event bus subscriber count, open position count, open order count
- [x] Implement `sse_events` handler — subscribe to EventBus, stream events as SSE with `axum::response::sse::Sse`, format events as partial HTML fragments for HTMX swap
- [x] Create Askama HTML templates in `crates/polyrust-dashboard/src/templates/`:
  - `base.html` — layout with nav bar, HTMX script (`<script src="https://unpkg.com/htmx.org@2.0.0"></script>`), Tailwind CSS CDN
  - `index.html` — extends base, overview cards + SSE connection (`hx-sse="connect:/events/stream"`)
  - `positions.html` — extends base, positions table with SSE live updates
  - `trades.html` — extends base, paginated trade log table
  - `health.html` — extends base, health metrics cards
  - `partials/position_row.html` — single position table row (for SSE swap)
  - `partials/pnl_summary.html` — PnL summary card (for SSE swap)
- [x] Update `crates/polyrust-dashboard/src/lib.rs` with module declarations and public exports
- [x] Write tests:
  - Test: all routes return 200 status with mock AppState
  - Test: SSE endpoint produces events when EventBus publishes
  - Test: positions handler reads from StrategyContext correctly
  - Test: trades handler queries Store and returns results
- [x] Verify `cargo test --workspace` passes
- [x] Mark completed

---

## Milestone 6: Reference Strategy (Crypto Arbitrage)

### Task 11: Port crypto arbitrage strategy from Python
> **Detailed reference:** [polyrust-framework-implementation.md → Task 11](./polyrust-framework-implementation.md#task-11-port-crypto-arbitrage-strategy) — MarketWithReference, ArbitrageOpportunity structs, confidence model formulas, Strategy trait impl, 4-step porting order, Python reference (crypto_arbitrage.py key methods at lines ~600, ~700, ~1200, ~1400)
- [x] Add dependencies to `crates/polyrust-strategies/Cargo.toml`: polyrust-core, serde, chrono, rust_decimal, tracing, async-trait
- [x] Create `crates/polyrust-strategies/src/crypto_arb.rs` (or submodule `crypto_arb/` if > 500 lines)
- [x] Define `ArbitrageConfig` struct: coins (Vec<String>, default ["BTC","ETH","SOL","XRP"]), position_size (Decimal, 5.0), max_positions (usize, 5), min_profit_margin (Decimal, 0.03), late_window_margin (Decimal, 0.02), stop_loss_reversal_pct (Decimal, 0.005), stop_loss_min_drop (Decimal, 0.05), scan_interval_secs (u64, 30), use_chainlink (bool, true)
- [x] Define `MarketWithReference` struct: market (MarketInfo), reference_price (Decimal), reference_approximate (bool), discovery_time (DateTime<Utc>), coin (String)
- [x] Implement `MarketWithReference::predict_winner(current_price)` — if current_price > reference_price → Up, else → Down (reference: crypto_arbitrage.py)
- [x] Implement `MarketWithReference::get_confidence(current_price, market_price, time_remaining_secs)` — multi-signal confidence 0-1:
  - Tail-end (< 120s + market_price >= 0.90): return 1.0
  - Late window (120-300s): base = distance_pct * 66, market_boost = 1.0 + (market_price - 0.5) * 0.5, return min(1.0, base * market_boost)
  - Early window (> 300s): return min(1.0, distance_pct * 50)
- [x] Define `ArbitrageMode` enum: TailEnd, TwoSided, Confirmed
- [x] Define `ArbitrageOpportunity` struct: mode, market_id, outcome_to_buy (OutcomeSide), token_id, buy_price (Decimal), confidence (Decimal), profit_margin (Decimal)
- [x] Define `ArbitragePosition` struct: market_id, token_id, side, entry_price, size, reference_price, coin, order_id (Option), entry_time
- [x] Define `CryptoArbitrageStrategy` struct: config (ArbitrageConfig), active_markets (HashMap<MarketId, MarketWithReference>), price_history (HashMap<String, VecDeque<(DateTime<Utc>, Decimal)>>), positions (HashMap<MarketId, ArbitragePosition>), last_scan (Option<Instant>)
- [x] Implement `CryptoArbitrageStrategy::new(config: ArbitrageConfig)`
- [x] Implement `Strategy` trait for CryptoArbitrageStrategy:
  - `name()` → "crypto-arbitrage"
  - `description()` → "Exploits mispricing in 15-min Up/Down crypto markets"
  - `on_start()` — emit SubscribeMarket actions for initial markets (use Gamma API via rs-clob-client)
  - `on_event()` — match on MarketDataEvent variants: ExternalPrice → on_crypto_price(), OrderbookUpdate → on_orderbook_update(), MarketDiscovered → on_market_discovered(), MarketExpired → on_market_expired()
  - `on_stop()` — cancel all open orders, log final PnL
- [x] Implement `on_crypto_price(symbol, price, ctx)`:
  - Record price in price_history VecDeque (keep last 12 entries = 60s at 5s intervals)
  - For each active market matching this coin: call evaluate_opportunity()
  - If opportunity found and position_count < max_positions: emit PlaceOrder action
- [x] Implement `evaluate_opportunity(market, current_price, ctx)` — check 3 modes in priority order:
  1. TailEnd: time_remaining < 120s AND market best_ask >= 0.90 → buy predicted winner
  2. TwoSided: up_ask + down_ask < 0.98 → buy both (guaranteed profit)
  3. Confirmed: confidence >= threshold AND profit_margin >= min → buy predicted winner
  - Return Option<ArbitrageOpportunity>
- [x] Implement `on_orderbook_update(snapshot, ctx)`:
  - Update market prices in context
  - Check stop-losses on open positions: trigger if crypto price reversed by stop_loss_reversal_pct (0.5%) AND market price dropped by stop_loss_min_drop (5¢) AND time_remaining > 60s
  - If stop-loss triggered: emit PlaceOrder SELL action
- [x] Implement `on_market_discovered(market, ctx)` — lookup current crypto price, create MarketWithReference with reference_price, add to active_markets
- [x] Implement `on_market_expired(market_id, ctx)` — remove from active_markets, clean up positions (attempt redemption via EmitSignal or Log)
- [x] Implement `compute_volatility(prices: &VecDeque)` — standard deviation of last N price points, return as Decimal
- [x] Update `crates/polyrust-strategies/src/lib.rs` with module declaration and public export of CryptoArbitrageStrategy
- [x] Write tests:
  - Test: predict_winner — BTC up → OutcomeSide::Up, BTC down → OutcomeSide::Down
  - Test: get_confidence tail-end — time < 120s, market >= 0.90 → confidence 1.0
  - Test: get_confidence late window — time 200s, distance 2% → expected confidence value
  - Test: get_confidence early window — time 600s, distance 1% → lower confidence
  - Test: evaluate_opportunity TailEnd mode — correct conditions produce TailEnd opportunity
  - Test: evaluate_opportunity TwoSided mode — up_ask 0.48 + down_ask 0.49 = 0.97 < 0.98 → detected
  - Test: evaluate_opportunity Confirmed mode — high confidence + sufficient margin → opportunity
  - Test: evaluate_opportunity returns None when confidence too low
  - Test: stop-loss triggers when reversal > 0.5% AND price drop > 5¢ AND time > 60s
  - Test: stop-loss does NOT trigger in final 60 seconds
  - Test: stop-loss does NOT trigger when price drop < 5¢ (avoid selling at entry)
  - Test: on_market_discovered creates MarketWithReference with correct reference_price
  - Test: on_market_expired removes market from active_markets
  - Test: volatility calculation returns correct std dev for known price series
- [x] Verify `cargo test --workspace` passes
- [x] Mark completed

---

## Milestone 7: Binary Entry Point & Integration

### Task 12: Wire binary entry point, config, example strategy
> **Detailed reference:** [polyrust-framework-implementation.md → Task 12](./polyrust-framework-implementation.md#task-12-wire-everything-together-in-mainrs) — full default.toml contents, complete main.rs code, simple_strategy.rs example code
- [x] Create `config/default.toml` with all config sections: [engine] (event_bus_capacity=4096, health_check_interval_secs=30), [polymarket] (comment: set via env vars), [dashboard] (enabled=true, port=3000, host="127.0.0.1"), [store] (db_path="polyrust.db"), [paper] (enabled=true, initial_balance=10000)
- [x] Update `src/main.rs`:
  - Initialize tracing with EnvFilter (default: "info,polyrust=debug")
  - Load Config from "config/default.toml" with fallback to Default, apply env overrides
  - Initialize Store with config.store.db_path
  - Choose execution backend: PaperBackend if config.paper.enabled, else LiveBackend
  - Build Engine with config, CryptoArbitrageStrategy, execution backend
  - Start market data feeds (ClobFeed + PriceFeed) with engine's event_bus
  - If dashboard enabled: spawn Dashboard::serve in background tokio task
  - Call engine.run() (blocks until Ctrl+C)
- [x] Create `examples/simple_strategy.rs` — minimal LoggingStrategy that implements Strategy trait, logs every event topic, runs with PaperBackend(10000, Immediate)
- [x] Verify `cargo build --workspace` succeeds
- [x] Verify `cargo run` starts in paper mode with dashboard at localhost:3000
- [x] Verify `cargo run --example simple_strategy` starts and responds to Ctrl+C
- [x] Verify `cargo build --release` produces single binary in target/release/polyrust
- [x] Mark completed

### Task 13: Add CLAUDE.md developer guide and README
> **Detailed reference:** [polyrust-framework-implementation.md → Task 13](./polyrust-framework-implementation.md#task-13-add-claudemd-developer-guide) — CLAUDE.md content outline, documentation update table
- [x] Create `CLAUDE.md` with sections:
  - Project overview (Polyrust = autonomous Polymarket trading framework in Rust)
  - Build commands: `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace`
  - Never commit binary artifacts, never run `go build`
  - Crate dependency graph (core → store/market/execution → strategies/dashboard → binary)
  - How to add a new strategy: implement Strategy trait, register with Engine::builder().strategy()
  - Paper mode vs live mode: set `[paper] enabled = true/false` or `POLY_PAPER_TRADING=true`
  - Config: `config/default.toml` + POLY_* env var overrides
  - Domain concepts: token IDs (ERC-1155), prices (0-1 probability), USDC (6 decimals), tick sizes (0.01 most markets), neg_risk (false for 15-min markets)
  - rs-clob-client features in use: clob, ws, rtds, data, gamma, tracing, heartbeats, ctf
  - Testing patterns: mock ExecutionBackend, in-memory Turso, dec!() macro, tokio::time::timeout for event tests
  - Polymarket API endpoints: CLOB (clob.polymarket.com), Gamma (gamma-api.polymarket.com), Data (data-api.polymarket.com), WS (ws-subscriptions-clob.polymarket.com)
- [x] Create `README.md` with:
  - Project title and one-line description
  - Architecture diagram (ASCII from design doc)
  - Quickstart (clone, cargo build, cargo run)
  - Configuration reference
  - Strategy plugin example
  - License
- [x] Verify all docs reference correct file paths
- [x] Mark completed

---

## Final Validation

### Task 14: End-to-end validation and cleanup
> **Detailed reference:** [polyrust-framework-implementation.md → Definition of Done](./polyrust-framework-implementation.md#7-definition-of-done) — full acceptance criteria checklist, testing strategy, coverage expectations
- [x] Run `cargo build --workspace` — zero errors
- [x] Run `cargo test --workspace` — all tests pass
- [x] Run `cargo clippy --workspace -- -D warnings` — zero warnings
- [x] Run `cargo run --example simple_strategy` — starts, logs events, Ctrl+C exits cleanly
- [x] Run `cargo run` — starts in paper mode, dashboard accessible at http://127.0.0.1:3000
- [x] Verify dashboard pages load: /, /positions, /trades, /health
- [x] Verify SSE endpoint /events/stream connects and receives events
- [x] Run `cargo build --release` — produces single binary at target/release/polyrust
- [x] Verify no TODO/FIXME/placeholder code remains in shipped crates (except clearly marked future work in comments)
- [x] Verify no secrets, private keys, or API credentials in committed code
- [x] Verify .gitignore covers: target/, *.db, *.db-journal, .env, config/local.toml
- [x] Mark completed

---

*Generated via /brainstorm-plan on 2026-01-27*
