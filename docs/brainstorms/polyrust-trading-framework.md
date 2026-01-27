# Polyrust — Autonomous Polymarket Trading Bot Framework

## Overview

Polyrust is an autonomous Polymarket trading bot framework written in Rust. It provides a modular, event-driven architecture where trading strategies are implemented as compile-time plugins via Rust traits. The framework handles all infrastructure concerns — market data ingestion, order execution, position tracking, risk management, and persistent logging — so strategy authors focus purely on signal generation and decision logic. It ships with a built-in paper trading engine, an Axum+HTMX monitoring dashboard, and uses Turso (embedded SQLite in Rust) for detailed trade/event logging. The `rs-clob-client` SDK is the primary interface to Polymarket's CLOB.

## Goals

- Provide a production-grade, event-driven framework for autonomous Polymarket trading
- Strategy-as-trait plugin system — implement `Strategy` trait, get full framework infrastructure
- Custom typed event bus over tokio channels with filtering and priority routing
- Multiple concurrent strategies with shared position/balance state and coordination
- Built-in paper trading engine via `ExecutionBackend` trait (live and simulated backends)
- Architecture designed so backtesting can be added as another `ExecutionBackend` in the future
- Turso (embedded SQLite-in-Rust) for all persistent storage: trades, orders, events, PnL snapshots
- Axum + HTMX server-rendered monitoring dashboard (real-time positions, PnL, trade log, system health)
- Leverage `rs-clob-client` for all Polymarket interactions (CLOB, WebSocket, auth flows)
- Single binary deployment — dashboard, bot engine, and database all in one process
- Structured logging with `tracing` crate for observability
- Ship a crypto arbitrage reference strategy — Rust analog of `crypto_arbitrage.py` demonstrating the full plugin API

## Non-Goals (v1)

- No backtesting engine (architecture supports it, implementation deferred)
- No dashboard control actions (monitor-only — no start/stop/configure from UI)
- No WASM or dynamic plugin loading — compile-time traits only
- No multi-bot orchestration or distributed deployment
- No mobile app or external API for third-party integrations

## User Experience

### Strategy Author Workflow

1. Add `polyrust-core` as a dependency
2. Implement `Strategy` trait on a struct
3. In `main.rs`, create an `Engine`, register strategies and an execution backend, call `engine.run()`
4. The framework handles: market data subscription, event routing, order execution, position tracking, persistence, and dashboard serving

```rust
// Minimal user code:
use polyrust_core::prelude::*;

struct MyStrategy { /* ... */ }

#[async_trait]
impl Strategy for MyStrategy {
    fn name(&self) -> &str { "my-strategy" }
    fn description(&self) -> &str { "My custom strategy" }
    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        match event {
            Event::MarketData(data) => { /* analyze and decide */ }
            _ => Ok(vec![])
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_file("config/bot.toml")?;
    let engine = Engine::builder()
        .config(config)
        .strategy(MyStrategy::new())
        .execution(LiveBackend::new(&config).await?)
        .dashboard(true)  // serve at :3000
        .build()
        .await?;
    engine.run().await
}
```

## Technical Approach

### High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        polyrust binary                          │
│                                                                 │
│  ┌──────────────┐   ┌──────────────┐   ┌────────────────────┐  │
│  │  Axum+HTMX   │   │   Engine      │   │  Turso (embedded)  │  │
│  │  Dashboard    │◄──│   Core        │──►│  - trades          │  │
│  │  (monitor)    │   │              │   │  - orders          │  │
│  └──────────────┘   │  ┌────────┐  │   │  - events          │  │
│                      │  │EventBus│  │   │  - pnl_snapshots   │  │
│                      │  └───┬────┘  │   └────────────────────┘  │
│                      │      │       │                            │
│         ┌────────────┼──────┼───────┼────────────┐              │
│         ▼            ▼      ▼       ▼            ▼              │
│  ┌───────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐       │
│  │ Strategy A │ │Strategy B│ │ Position │ │  Risk      │       │
│  │ (crypto    │ │(user's)  │ │ Manager  │ │  Manager   │       │
│  │  arb)      │ │          │ │ (shared) │ │            │       │
│  └─────┬─────┘ └────┬─────┘ └──────────┘ └────────────┘       │
│        │             │                                          │
│        ▼             ▼                                          │
│  ┌─────────────────────────────────┐                            │
│  │      ExecutionBackend trait      │                            │
│  │  ┌───────────┐ ┌──────────────┐ │                            │
│  │  │   Live     │ │   Paper      │ │                            │
│  │  │ (rs-clob)  │ │ (simulated)  │ │                            │
│  │  └───────────┘ └──────────────┘ │                            │
│  └─────────────────────────────────┘                            │
│                      │                                          │
│                      ▼                                          │
│  ┌─────────────────────────────────┐                            │
│  │      rs-clob-client SDK         │                            │
│  │  CLOB API · WebSocket · Auth    │                            │
│  └─────────────────────────────────┘                            │
└─────────────────────────────────────────────────────────────────┘
```

### Key Architectural Decisions

- **Event-driven core**: A typed `EventBus` built on tokio broadcast channels routes events (`MarketData`, `OrderFill`, `PositionUpdate`, `Signal`, `SystemHealth`) to subscribers with topic-based filtering
- **Strategy trait**: Strategies implement `async fn on_event(&mut self, event: &Event, ctx: &mut StrategyContext) -> Vec<Action>` — receive events, emit actions (place/cancel orders, log, etc.)
- **ExecutionBackend trait**: Abstracts order execution. `LiveBackend` delegates to `rs-clob-client`, `PaperBackend` simulates fills against orderbook snapshots. Future `BacktestBackend` replays historical data
- **Shared state via `StrategyContext`**: Provides thread-safe access to positions, balances, and market data. Uses `Arc<RwLock<...>>` for concurrent strategy access with coordination
- **Turso embedded**: No external database process. SQLite-compatible queries, async I/O, stored in a local file
- **Single process**: Engine, dashboard, and database all run in one tokio runtime. Dashboard reads state via shared `Arc` references and SSE for live updates

## Key Components

### Crate Structure

```
polyrust/
├── Cargo.toml              # workspace root
├── crates/
│   ├── polyrust-core/      # Engine, EventBus, traits, shared state
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── engine.rs         # Main engine lifecycle (start/stop/run loop)
│   │       ├── event_bus.rs      # Typed event bus with topic routing
│   │       ├── events.rs         # Event enum + typed payloads
│   │       ├── strategy.rs       # Strategy trait definition
│   │       ├── execution.rs      # ExecutionBackend trait
│   │       ├── context.rs        # StrategyContext (shared state handle)
│   │       ├── position.rs       # PositionManager (thread-safe)
│   │       ├── risk.rs           # RiskManager (limits, exposure checks)
│   │       └── config.rs         # Framework configuration
│   │
│   ├── polyrust-market/    # Market data ingestion & normalization
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── feed.rs           # MarketDataFeed trait
│   │       ├── clob_feed.rs      # rs-clob-client WebSocket feed
│   │       ├── price_feed.rs     # External price feeds (crypto prices)
│   │       └── orderbook.rs      # Orderbook aggregation & snapshots
│   │
│   ├── polyrust-execution/ # Execution backends
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── live.rs           # LiveBackend (rs-clob-client orders)
│   │       └── paper.rs          # PaperBackend (simulated fills)
│   │
│   ├── polyrust-store/     # Turso persistence layer
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── db.rs             # Turso connection & migrations
│   │       ├── trades.rs         # Trade log queries
│   │       ├── orders.rs         # Order history
│   │       ├── events.rs         # Event audit log
│   │       └── snapshots.rs      # PnL snapshots & metrics
│   │
│   ├── polyrust-dashboard/ # Axum + HTMX monitoring UI
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── server.rs         # Axum router + SSE endpoints
│   │       ├── handlers.rs       # Page handlers (positions, trades, health)
│   │       └── templates/        # HTML templates (askama or maud)
│   │
│   └── polyrust-strategies/ # Reference strategy implementations
│       └── src/
│           ├── lib.rs
│           └── crypto_arb.rs     # Crypto arbitrage (port from Python)
│
├── src/
│   └── main.rs             # Binary entry point, wires everything together
├── config/
│   └── default.toml        # Default configuration
└── examples/
    └── simple_strategy.rs  # Minimal strategy example
```

### Core Traits

```rust
// Strategy plugin interface
#[async_trait]
pub trait Strategy: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>>;
    async fn on_start(&mut self, ctx: &StrategyContext) -> Result<()> { Ok(()) }
    async fn on_stop(&mut self, ctx: &StrategyContext) -> Result<()> { Ok(()) }
}

// Execution backend abstraction
#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult>;
    async fn cancel_order(&self, order_id: &str) -> Result<()>;
    async fn get_open_orders(&self) -> Result<Vec<Order>>;
    async fn get_positions(&self) -> Result<Vec<Position>>;
}

// Market data feed abstraction
#[async_trait]
pub trait MarketDataFeed: Send + Sync {
    async fn subscribe(&mut self, markets: &[MarketId]) -> Result<()>;
    async fn unsubscribe(&mut self, markets: &[MarketId]) -> Result<()>;
    fn event_stream(&self) -> broadcast::Receiver<MarketEvent>;
}
```

### Event Types

```rust
pub enum Event {
    MarketData(MarketDataEvent),   // orderbook updates, trades, price ticks
    OrderUpdate(OrderEvent),        // fills, cancellations, rejections
    PositionChange(PositionEvent),  // position opened/closed/modified
    Signal(SignalEvent),            // strategy-emitted signals
    System(SystemEvent),            // health, errors, lifecycle
}
```

## Open Questions

- **Risk management scope**: Should v1 include position limits, max drawdown, and exposure caps — or leave risk checks to individual strategies?
- **Configuration format**: TOML (Rust-idiomatic) vs YAML (familiar from Python bot)?
- **Dashboard auth**: Should the monitoring dashboard require auth, or is localhost-only sufficient for v1?
- **Turso remote sync**: Turso supports syncing embedded DB to a remote server. Include this as an optional feature for multi-device monitoring?
- **Strategy coordination**: When multiple strategies share state, what conflict resolution policy applies? (First-come-first-served? Priority? Allocation quotas?)

---
*Generated via /brainstorm on 2026-01-27*
