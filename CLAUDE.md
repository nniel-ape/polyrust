# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Polyrust is an autonomous Polymarket trading bot framework in Rust. It uses an event-driven architecture where strategies are compile-time trait plugins. The framework handles market data ingestion, order execution, position tracking, persistence, and monitoring — strategy authors only implement signal generation logic.

Single binary deployment: engine + Axum/HTMX dashboard + embedded Turso (SQLite) database all in one process.

## Build & Test Commands

```fish
cargo build --workspace          # Build all crates
cargo test --workspace           # Run all tests
cargo clippy --workspace -- -D warnings  # Lint (must pass with zero warnings)
cargo test -p polyrust-core      # Test a single crate
cargo test --workspace -- event_bus  # Run tests matching a name
cargo test --workspace -- --ignored  # Run live API tests (requires credentials)
cargo run                        # Run bot (paper mode by default)
cargo run --example simple_strategy  # Run minimal example
cargo build --release            # Optimized single binary → target/release/polyrust
```

Never run `go build`. Never commit binary artifacts from `target/`.

## Architecture

### Crate Dependency Graph

```
polyrust-core (engine, event bus, traits, shared state)
  ├── polyrust-market (CLOB orderbook + RTDS price feeds)
  ├── polyrust-execution (live + paper backends)
  ├── polyrust-store (Turso persistence)
  ├── polyrust-strategies (reference: crypto arbitrage)
  └── polyrust-dashboard (Axum + HTMX monitoring UI)

src/main.rs → wires all crates into a single binary
```

### Core Traits (Plugin System)

- **`Strategy`** — `on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>>` — receives events, returns actions (place/cancel orders, emit signals)
- **`ExecutionBackend`** — abstracts order execution: `LiveBackend` (real CLOB API) vs `PaperBackend` (simulated fills)
- **`MarketDataFeed`** — market data producers: `ClobFeed` (WebSocket orderbooks) and `PriceFeed` (RTDS crypto prices)
- **`DashboardViewProvider`** — `view_name(&self) -> &str` + `render_view(&self) -> Result<String>` — optional trait for strategies to expose custom dashboard pages at `/strategy/<name>`

### Event Flow

1. Market feeds publish events to `EventBus` (tokio broadcast channels with topic filtering)
2. Engine routes events to all registered strategies
3. Strategies return `Vec<Action>` (PlaceOrder, CancelOrder, EmitSignal, etc.)
4. Engine executes actions via the execution backend
5. Action results become new events, flowing back through the bus

### Shared State

`StrategyContext` provides thread-safe access via `Arc<RwLock<...>>`:
- `PositionState` — open positions and orders
- `MarketDataState` — orderbooks, market info, external prices
- `BalanceState` — available and locked USDC
- `strategy_views` — registered `DashboardViewProvider` implementations (keyed by strategy name)

### Strategy Dashboard Views

Strategies can expose custom dashboard pages via the `DashboardViewProvider` trait (`crates/polyrust-core/src/dashboard_view.rs`). Each strategy optionally returns a view provider from `dashboard_view()`, which renders an HTML fragment for `/strategy/:name`. The dashboard auto-generates nav links for all registered strategy views.

Real-time updates use SSE: strategies emit `"dashboard-update"` signals, the SSE handler re-renders the view, and HTMX swaps the content in the browser. See the crypto arbitrage strategy for a reference implementation.

## Domain Concepts

- **Token IDs**: Each market has 2 outcomes (Up/Down or Yes/No), each is an ERC-1155 token
- **Prices**: Probabilities in [0, 1] range — use `rust_decimal::Decimal`, never floats
- **USDC**: 6 decimal places; store as `Decimal`, persist as TEXT in SQLite
- **Tick sizes**: Typically 0.01 (2 decimal price, 2 decimal size, 4 decimal amount)
- **neg_risk**: Boolean on orders — false for 15-minute markets (most common)

## Configuration

TOML config at `config/default.toml` with `POLY_*` environment variable overrides:
- `POLY_PRIVATE_KEY`, `POLY_SAFE_ADDRESS` — wallet credentials
- `POLY_BUILDER_API_KEY`, `POLY_BUILDER_API_SECRET`, `POLY_BUILDER_API_PASSPHRASE` — builder API
- `POLY_DASHBOARD_PORT`, `POLY_DB_PATH`, `POLY_PAPER_TRADING` — runtime settings

Paper mode: `[paper] enabled = true` or `POLY_PAPER_TRADING=true`

## Adding a New Strategy

1. Add `polyrust-core` as a dependency in your crate
2. Implement the `Strategy` trait on your struct
3. Register with `Engine::builder().strategy(YourStrategy::new())`
4. (Optional) Implement `DashboardViewProvider` for a custom dashboard page

```rust
use polyrust_core::prelude::*;

struct MyStrategy;

#[async_trait]
impl Strategy for MyStrategy {
    fn name(&self) -> &str { "my-strategy" }
    fn description(&self) -> &str { "My custom strategy" }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        match event {
            Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot)) => {
                // Analyze orderbook, return PlaceOrder actions
                Ok(vec![])
            }
            _ => Ok(vec![]),
        }
    }

    // Optional: provide a custom dashboard view at /strategy/my-strategy
    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        Some(self)
    }
}

impl DashboardViewProvider for MyStrategy {
    fn view_name(&self) -> &str { "my-strategy" }
    fn render_view(&self) -> Result<String> {
        Ok("<div>Strategy-specific HTML here</div>".to_string())
    }
}
```

See `examples/simple_strategy.rs` for a complete runnable example.

## Paper Mode vs Live Mode

- Paper mode (default): `[paper] enabled = true` in `config/default.toml` or `POLY_PAPER_TRADING=true`
  - Simulated fills, no real orders, configurable initial balance
  - Supports Immediate and Orderbook fill modes
- Live mode: `[paper] enabled = false` with valid Polymarket credentials
  - Requires `POLY_PRIVATE_KEY`, `POLY_SAFE_ADDRESS`, and builder API credentials
  - Uses `rs-clob-client` SDK for real CLOB interaction

## Key Dependencies

- **`polymarket-client-sdk`** (rs-clob-client) v0.4.1 — all Polymarket interactions. Features: `clob`, `ws`, `rtds`, `data`, `gamma`, `tracing`, `heartbeats`, `ctf`
- **`libsql`/turso** — embedded SQLite database (no external process)
- **`axum`** 0.8 + **`askama`** 0.13 — dashboard web framework and templates
- **`tokio`** — async runtime; broadcast channels for EventBus
- **`rust_decimal`** — precise decimal arithmetic (required for all prices/amounts)

## Testing Patterns

- Mock `ExecutionBackend` for strategy tests
- In-memory Turso (`:memory:` path) for store tests
- `rust_decimal_macros::dec!()` for precise decimal literals in tests
- `tokio::time::timeout` to prevent hanging async tests
- Deterministic timestamps in tests — avoid real `Utc::now()`
- Live API tests: mark with `#[ignore]`, run with `--ignored`

## Reference Strategy: Crypto Arbitrage

Ported from Python (`../polymarket-trading-bot/`). Exploits mispricing in 15-minute Up/Down crypto markets with three modes:
1. **Tail-End** (<2 min remaining, market >= 90%) — highest confidence
2. **Two-Sided** (both outcomes < $1 combined) — guaranteed profit
3. **Confirmed** (dynamic confidence model) — standard directional trading

## Polymarket API Endpoints

- CLOB API: `https://clob.polymarket.com`
- Gamma API: `https://gamma-api.polymarket.com` (market discovery, metadata)
- Data API: `https://data-api.polymarket.com` (positions, balances)
- WebSocket: `wss://ws-subscriptions-clob.polymarket.com` (orderbook streams)

## Design Documents

- `docs/brainstorms/polyrust-trading-framework.md` — goals, architecture, traits
- `docs/plans/polyrust-framework-implementation.md` — detailed implementation guide (2400 lines)
- `docs/plans/polyrust-checklist.md` — 14-milestone task checklist with validation commands
- `docs/plans/strategy-dashboard-views.md` — strategy dashboard views design and implementation plan
