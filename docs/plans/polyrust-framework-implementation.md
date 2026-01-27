# Polyrust Framework — Implementation Plan

> Design doc: [`docs/brainstorms/polyrust-trading-framework.md`](../brainstorms/polyrust-trading-framework.md)

## 1. Overview

We are building **Polyrust**, an autonomous Polymarket trading bot framework in Rust. It is a Cargo workspace with 6 crates: `polyrust-core` (engine, event bus, traits), `polyrust-market` (market data feeds), `polyrust-execution` (live + paper backends), `polyrust-store` (Turso persistence), `polyrust-dashboard` (Axum+HTMX monitor), and `polyrust-strategies` (reference crypto arbitrage strategy). The framework uses `rs-clob-client` (`polymarket-client-sdk v0.4.1`) as the primary Polymarket interface and Turso as an embedded SQLite-in-Rust database.

The reference Python implementation lives at `../polymarket-trading-bot/` and should be consulted for domain logic, especially `strategies/crypto_arbitrage.py` (2000+ lines of battle-tested arbitrage logic).

## 2. Prerequisites

### Tools & Versions
- **Rust**: 1.88.0+ (required by `polymarket-client-sdk`)
- **Cargo**: latest stable
- **Git**: any recent version
- **fish shell**: user's environment

### Install Rust (if not installed)
```fish
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup default stable
rustup update
```

### Environment Setup
```fish
# Clone and enter
cd /Users/andrey/Projects/polyrust

# Verify toolchain
rustc --version  # Must be >= 1.88.0
cargo --version
```

### API Keys Needed (for live trading only — not needed for development)
- Polymarket CLOB API key, secret, passphrase (Builder Program)
- Ethereum private key (for order signing)
- Gnosis Safe address (derived or existing)

### Key Dependencies (resolved via Cargo)
| Crate | Version | Purpose |
|-------|---------|---------|
| `polymarket-client-sdk` | 0.4.1 | Polymarket CLOB, WS, Gamma, Data APIs |
| `turso` | latest | Embedded SQLite database |
| `tokio` | 1.x | Async runtime |
| `axum` | 0.8.x | Web framework for dashboard |
| `askama` | 0.13.x | HTML templating |
| `serde` / `serde_json` | 1.x | Serialization |
| `toml` | 0.8.x | Config parsing |
| `tracing` / `tracing-subscriber` | 0.1.x / 0.3.x | Structured logging |
| `rust_decimal` | 1.x | Precise decimal arithmetic |
| `chrono` | 0.4.x | Date/time handling |
| `async-trait` | 0.1.x | Async trait support |
| `thiserror` | 2.x | Error types |
| `uuid` | 1.x | Unique IDs |
| `tokio-stream` | 0.1.x | Stream utilities for SSE |

---

## 3. Codebase Orientation

### Project Layout (what you'll build)
```
polyrust/
├── Cargo.toml                    # Workspace root
├── crates/
│   ├── polyrust-core/            # Engine, EventBus, traits, shared state
│   ├── polyrust-market/          # Market data feeds
│   ├── polyrust-execution/       # Live + Paper execution backends
│   ├── polyrust-store/           # Turso persistence
│   ├── polyrust-dashboard/       # Axum + HTMX monitoring
│   └── polyrust-strategies/      # Reference strategy (crypto arb)
├── src/main.rs                   # Binary entry point
├── config/default.toml           # Default configuration
└── examples/simple_strategy.rs   # Minimal example
```

### Reference Python Code (read these for domain context)
| Python File | What to Learn | Rust Equivalent |
|-------------|--------------|-----------------|
| `strategies/crypto_arbitrage.py` | Full arb logic, confidence model, 3 trading modes | `polyrust-strategies/src/crypto_arb.rs` |
| `strategies/base.py` | Strategy lifecycle, callback system | `polyrust-core/src/strategy.rs` |
| `src/bot.py` | TradingBot API surface, order management | `polyrust-execution/src/live.rs` |
| `src/client.py` | CLOB + Relayer HTTP clients, HMAC auth | Handled by `rs-clob-client` |
| `src/websocket_client.py` | Orderbook WS, callbacks, reconnection | `polyrust-market/src/clob_feed.rs` |
| `src/gamma_client.py` | 15-min market discovery | Handled by `rs-clob-client` gamma feature |
| `src/crypto_price_client.py` | RTDS WebSocket, price cache | `polyrust-market/src/price_feed.rs` |
| `lib/market_manager.py` | Market info struct, auto-switching | `polyrust-market/src/` |
| `lib/price_tracker.py` | Flash crash detection, volatility | `polyrust-strategies/` (strategy-specific) |
| `lib/position_manager.py` | Position tracking, TP/SL, PnL | `polyrust-core/src/position.rs` |
| `src/paper/engine.py` | Paper fill modes, order simulation | `polyrust-execution/src/paper.rs` |
| `src/config.py` | Config hierarchy, env vars | `polyrust-core/src/config.rs` |

### rs-clob-client Feature Flags to Enable
```toml
polymarket-client-sdk = { version = "0.4.1", features = [
    "clob",       # Core order placement, market data
    "ws",         # WebSocket orderbook streaming
    "rtds",       # Real-time data streams (Binance, Chainlink prices)
    "data",       # Data API (positions, trades, redeemable)
    "gamma",      # Gamma API (market discovery)
    "tracing",    # Structured logging
    "heartbeats", # Auto heartbeats to prevent order cancellation
    "ctf",        # Split/merge/redeem operations
] }
```

---

## 4. Implementation Tasks

Implementation is organized into **7 milestones**, each building on the previous. Each milestone produces a compilable, testable artifact.

---

### Milestone 1: Workspace Scaffolding & Core Types

---

#### Task 1: Create Cargo Workspace

**Goal:** Set up the workspace root with all 6 crates and the binary target. Everything compiles with `cargo build`.

**Files to create:**
- `Cargo.toml` — workspace root
- `crates/polyrust-core/Cargo.toml` + `src/lib.rs`
- `crates/polyrust-market/Cargo.toml` + `src/lib.rs`
- `crates/polyrust-execution/Cargo.toml` + `src/lib.rs`
- `crates/polyrust-store/Cargo.toml` + `src/lib.rs`
- `crates/polyrust-dashboard/Cargo.toml` + `src/lib.rs`
- `crates/polyrust-strategies/Cargo.toml` + `src/lib.rs`
- `src/main.rs` — binary entry point (just `fn main() {}` for now)

**Implementation steps:**

1. Create workspace `Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = [
    "crates/polyrust-core",
    "crates/polyrust-market",
    "crates/polyrust-execution",
    "crates/polyrust-store",
    "crates/polyrust-dashboard",
    "crates/polyrust-strategies",
]

[workspace.dependencies]
# Shared versions across all crates
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
thiserror = "2"
anyhow = "1"
async-trait = "0.1"
chrono = { version = "0.4", features = ["serde"] }
rust_decimal = { version = "1", features = ["serde-with-str"] }
uuid = { version = "1", features = ["v4", "serde"] }
tokio-stream = "0.1"

polymarket-client-sdk = { version = "0.4", features = [
    "clob", "ws", "rtds", "data", "gamma", "tracing", "heartbeats", "ctf"
] }

# Internal crates
polyrust-core = { path = "crates/polyrust-core" }
polyrust-market = { path = "crates/polyrust-market" }
polyrust-execution = { path = "crates/polyrust-execution" }
polyrust-store = { path = "crates/polyrust-store" }
polyrust-dashboard = { path = "crates/polyrust-dashboard" }
polyrust-strategies = { path = "crates/polyrust-strategies" }

[package]
name = "polyrust"
version = "0.1.0"
edition = "2024"

[dependencies]
polyrust-core.workspace = true
polyrust-market.workspace = true
polyrust-execution.workspace = true
polyrust-store.workspace = true
polyrust-dashboard.workspace = true
polyrust-strategies.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
anyhow.workspace = true
```

2. Each crate's `Cargo.toml` should reference workspace dependencies with `.workspace = true`. Example for `polyrust-core`:
```toml
[package]
name = "polyrust-core"
version = "0.1.0"
edition = "2024"

[dependencies]
tokio.workspace = true
serde.workspace = true
serde_json.workspace = true
tracing.workspace = true
thiserror.workspace = true
async-trait.workspace = true
chrono.workspace = true
rust_decimal.workspace = true
uuid.workspace = true
tokio-stream.workspace = true
```

3. Each `src/lib.rs` starts as an empty module file (just a comment).

4. `src/main.rs`:
```rust
fn main() {
    println!("polyrust");
}
```

5. Create `.gitignore`:
```
/target
*.db
*.db-journal
.env
config/local.toml
```

**Testing:**
```fish
cargo build
cargo test --workspace
```
Both must pass with zero errors.

**Verification:** `cargo build` succeeds, `cargo test --workspace` passes (no tests yet, 0 passed is fine).

**Commit:** `chore: scaffold cargo workspace with 6 crates`

---

#### Task 2: Define Core Domain Types

**Goal:** Define all shared domain types in `polyrust-core` that other crates depend on. These are the "language" of the framework.

**Files to touch:**
- `crates/polyrust-core/src/lib.rs` — module declarations + prelude
- `crates/polyrust-core/src/types.rs` — domain types (NEW)
- `crates/polyrust-core/src/error.rs` — error types (NEW)

**Implementation steps:**

1. Create `crates/polyrust-core/src/types.rs` with these types:

```rust
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Unique identifier for a market (Polymarket condition_id)
pub type MarketId = String;

/// ERC-1155 token identifier for a market outcome
pub type TokenId = String;

/// Unique identifier for an order
pub type OrderId = String;

/// Side of a market outcome (Up/Down, Yes/No)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutcomeSide {
    Up,
    Down,
    Yes,
    No,
}

/// Order side
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

/// Order type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderType {
    Gtc,  // Good Till Cancelled
    Gtd,  // Good Till Date
    Fok,  // Fill or Kill
}

/// A request to place an order (strategy → execution backend)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub token_id: TokenId,
    pub price: Decimal,       // 0-1 range (probability)
    pub size: Decimal,        // Number of shares
    pub side: OrderSide,
    pub order_type: OrderType,
    pub neg_risk: bool,
}

/// Result of an order placement
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResult {
    pub success: bool,
    pub order_id: Option<OrderId>,
    pub status: Option<String>,
    pub message: String,
}

/// An open order
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: OrderId,
    pub token_id: TokenId,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub filled_size: Decimal,
    pub status: OrderStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderStatus {
    Open,
    Filled,
    PartiallyFilled,
    Cancelled,
    Expired,
}

/// A position in a market outcome
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub id: Uuid,
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub side: OutcomeSide,
    pub entry_price: Decimal,
    pub size: Decimal,
    pub current_price: Decimal,
    pub entry_time: DateTime<Utc>,
    pub strategy_name: String,
}

impl Position {
    pub fn unrealized_pnl(&self) -> Decimal {
        (self.current_price - self.entry_price) * self.size
    }
}

/// A completed trade (for logging)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub id: Uuid,
    pub order_id: OrderId,
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub realized_pnl: Option<Decimal>,
    pub strategy_name: String,
    pub timestamp: DateTime<Utc>,
}

/// Orderbook level
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderbookLevel {
    pub price: Decimal,
    pub size: Decimal,
}

/// Orderbook snapshot for a single token
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderbookSnapshot {
    pub token_id: TokenId,
    pub bids: Vec<OrderbookLevel>,
    pub asks: Vec<OrderbookLevel>,
    pub timestamp: DateTime<Utc>,
}

impl OrderbookSnapshot {
    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.first().map(|l| l.price)
    }

    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.first().map(|l| l.price)
    }

    pub fn mid_price(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some((bid + ask) / Decimal::TWO),
            (Some(p), None) | (None, Some(p)) => Some(p),
            _ => None,
        }
    }

    pub fn spread(&self) -> Option<Decimal> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => Some(ask - bid),
            _ => None,
        }
    }
}

/// Market information (from Gamma API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketInfo {
    pub id: MarketId,           // condition_id
    pub slug: String,
    pub question: String,
    pub end_date: DateTime<Utc>,
    pub token_ids: TokenIds,
    pub accepting_orders: bool,
    pub neg_risk: bool,
}

/// Token IDs for the two outcomes of a market
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenIds {
    pub outcome_a: TokenId,  // "Up" or "Yes"
    pub outcome_b: TokenId,  // "Down" or "No"
}

impl MarketInfo {
    pub fn has_ended(&self) -> bool {
        Utc::now() >= self.end_date
    }

    pub fn seconds_remaining(&self) -> i64 {
        (self.end_date - Utc::now()).num_seconds().max(0)
    }
}
```

2. Create `crates/polyrust-core/src/error.rs`:
```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PolyError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Execution error: {0}")]
    Execution(String),

    #[error("Market data error: {0}")]
    MarketData(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Strategy error: {0}")]
    Strategy(String),

    #[error("Event bus error: {0}")]
    EventBus(String),

    #[error("Polymarket SDK error: {0}")]
    Sdk(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, PolyError>;
```

3. Update `crates/polyrust-core/src/lib.rs`:
```rust
pub mod error;
pub mod types;

/// Prelude for convenient imports
pub mod prelude {
    pub use crate::error::{PolyError, Result};
    pub use crate::types::*;
}
```

**Testing:**
- `crates/polyrust-core/tests/types_test.rs` — test `OrderbookSnapshot::mid_price()`, `Position::unrealized_pnl()`, `MarketInfo::seconds_remaining()`, serialization round-trips.

```rust
// crates/polyrust-core/tests/types_test.rs
use polyrust_core::prelude::*;
use rust_decimal_macros::dec;
use chrono::Utc;

#[test]
fn orderbook_mid_price() {
    let ob = OrderbookSnapshot {
        token_id: "tok1".into(),
        bids: vec![OrderbookLevel { price: dec!(0.50), size: dec!(100) }],
        asks: vec![OrderbookLevel { price: dec!(0.52), size: dec!(100) }],
        timestamp: Utc::now(),
    };
    assert_eq!(ob.mid_price(), Some(dec!(0.51)));
    assert_eq!(ob.spread(), Some(dec!(0.02)));
}

#[test]
fn position_pnl() {
    let pos = Position {
        id: uuid::Uuid::new_v4(),
        market_id: "m1".into(),
        token_id: "tok1".into(),
        side: OutcomeSide::Up,
        entry_price: dec!(0.50),
        size: dec!(10),
        current_price: dec!(0.60),
        entry_time: Utc::now(),
        strategy_name: "test".into(),
    };
    assert_eq!(pos.unrealized_pnl(), dec!(1.0)); // (0.60-0.50)*10
}

#[test]
fn order_request_serialization_roundtrip() {
    let req = OrderRequest {
        token_id: "tok1".into(),
        price: dec!(0.55),
        size: dec!(5),
        side: OrderSide::Buy,
        order_type: OrderType::Gtc,
        neg_risk: false,
    };
    let json = serde_json::to_string(&req).unwrap();
    let deserialized: OrderRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.price, dec!(0.55));
}
```

Add `rust_decimal_macros` to workspace dev-dependencies:
```toml
[workspace.dependencies]
rust_decimal_macros = "1"
```

**Verification:**
```fish
cargo test --workspace
```

**Commit:** `feat: define core domain types and error hierarchy`

---

#### Task 3: Define Core Traits (Strategy, ExecutionBackend, MarketDataFeed)

**Goal:** Define the three primary trait interfaces that form the plugin system.

**Files to create:**
- `crates/polyrust-core/src/strategy.rs`
- `crates/polyrust-core/src/execution.rs`
- `crates/polyrust-core/src/events.rs`
- `crates/polyrust-core/src/actions.rs`
- `crates/polyrust-core/src/context.rs`

**Implementation steps:**

1. Create `crates/polyrust-core/src/events.rs` — the typed event enum:

```rust
use crate::types::*;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// All events that flow through the EventBus
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    MarketData(MarketDataEvent),
    OrderUpdate(OrderEvent),
    PositionChange(PositionEvent),
    Signal(SignalEvent),
    System(SystemEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MarketDataEvent {
    OrderbookUpdate(OrderbookSnapshot),
    PriceChange {
        token_id: TokenId,
        price: Decimal,
        side: OrderSide,
        best_bid: Decimal,
        best_ask: Decimal,
    },
    Trade {
        token_id: TokenId,
        price: Decimal,
        size: Decimal,
        timestamp: DateTime<Utc>,
    },
    ExternalPrice {
        symbol: String,   // e.g., "BTC", "ETH"
        price: Decimal,
        source: String,   // "binance", "chainlink"
        timestamp: DateTime<Utc>,
    },
    MarketDiscovered(MarketInfo),
    MarketExpired(MarketId),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderEvent {
    Placed(OrderResult),
    Filled {
        order_id: OrderId,
        token_id: TokenId,
        price: Decimal,
        size: Decimal,
    },
    PartiallyFilled {
        order_id: OrderId,
        filled_size: Decimal,
        remaining_size: Decimal,
    },
    Cancelled(OrderId),
    Rejected {
        order_id: Option<OrderId>,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PositionEvent {
    Opened(Position),
    Closed {
        position_id: uuid::Uuid,
        realized_pnl: Decimal,
    },
    Updated(Position),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalEvent {
    pub strategy_name: String,
    pub signal_type: String,
    pub payload: serde_json::Value,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SystemEvent {
    EngineStarted,
    EngineStopping,
    StrategyStarted(String),
    StrategyStopped(String),
    Error {
        source: String,
        message: String,
    },
    HealthCheck {
        strategies_active: usize,
        positions_open: usize,
        uptime_seconds: u64,
    },
}

impl Event {
    /// Topic string for event bus routing
    pub fn topic(&self) -> &'static str {
        match self {
            Event::MarketData(_) => "market_data",
            Event::OrderUpdate(_) => "order_update",
            Event::PositionChange(_) => "position_change",
            Event::Signal(_) => "signal",
            Event::System(_) => "system",
        }
    }
}
```

2. Create `crates/polyrust-core/src/actions.rs` — actions strategies can emit:

```rust
use crate::types::*;

/// Actions a strategy can request
#[derive(Debug, Clone)]
pub enum Action {
    PlaceOrder(OrderRequest),
    CancelOrder(OrderId),
    CancelAllOrders,
    Log {
        level: LogLevel,
        message: String,
    },
    EmitSignal {
        signal_type: String,
        payload: serde_json::Value,
    },
    SubscribeMarket(MarketId),
    UnsubscribeMarket(MarketId),
}

#[derive(Debug, Clone, Copy)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}
```

3. Create `crates/polyrust-core/src/context.rs` — shared state handle for strategies:

```rust
use crate::types::*;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Thread-safe shared state accessible by all strategies
#[derive(Debug, Clone)]
pub struct StrategyContext {
    pub positions: Arc<RwLock<PositionState>>,
    pub market_data: Arc<RwLock<MarketDataState>>,
    pub balance: Arc<RwLock<BalanceState>>,
}

#[derive(Debug, Default)]
pub struct PositionState {
    pub open_positions: HashMap<uuid::Uuid, Position>,
    pub open_orders: HashMap<OrderId, Order>,
}

impl PositionState {
    pub fn position_count(&self) -> usize {
        self.open_positions.len()
    }

    pub fn positions_for_strategy(&self, name: &str) -> Vec<&Position> {
        self.open_positions
            .values()
            .filter(|p| p.strategy_name == name)
            .collect()
    }

    pub fn total_unrealized_pnl(&self) -> Decimal {
        self.open_positions.values().map(|p| p.unrealized_pnl()).sum()
    }
}

#[derive(Debug, Default)]
pub struct MarketDataState {
    pub orderbooks: HashMap<TokenId, OrderbookSnapshot>,
    pub markets: HashMap<MarketId, MarketInfo>,
    pub external_prices: HashMap<String, Decimal>, // symbol → price
}

#[derive(Debug)]
pub struct BalanceState {
    pub available_usdc: Decimal,
    pub locked_usdc: Decimal, // In open orders
}

impl Default for BalanceState {
    fn default() -> Self {
        Self {
            available_usdc: Decimal::ZERO,
            locked_usdc: Decimal::ZERO,
        }
    }
}

impl StrategyContext {
    pub fn new() -> Self {
        Self {
            positions: Arc::new(RwLock::new(PositionState::default())),
            market_data: Arc::new(RwLock::new(MarketDataState::default())),
            balance: Arc::new(RwLock::new(BalanceState::default())),
        }
    }
}
```

4. Create `crates/polyrust-core/src/strategy.rs`:

```rust
use crate::actions::Action;
use crate::context::StrategyContext;
use crate::error::Result;
use crate::events::Event;
use async_trait::async_trait;

/// Core strategy plugin interface.
///
/// Implement this trait to create a trading strategy.
/// The engine calls `on_event` for every event routed to this strategy.
/// Return a `Vec<Action>` of actions to take (or empty vec for no action).
#[async_trait]
pub trait Strategy: Send + Sync {
    /// Unique name for this strategy (used in logs, DB, dashboard)
    fn name(&self) -> &str;

    /// Human-readable description
    fn description(&self) -> &str;

    /// Called when the engine starts this strategy.
    /// Use for initialization: subscribe to markets, set up state.
    async fn on_start(&mut self, ctx: &StrategyContext) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Called for every event routed to this strategy.
    /// Return actions to execute (place orders, cancel, log, etc.)
    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>>;

    /// Called when the engine stops this strategy.
    /// Use for cleanup: cancel open orders, log final state.
    async fn on_stop(&mut self, ctx: &StrategyContext) -> Result<()> {
        let _ = ctx;
        Ok(())
    }
}
```

5. Create `crates/polyrust-core/src/execution.rs`:

```rust
use crate::error::Result;
use crate::types::*;
use async_trait::async_trait;

/// Abstraction over order execution.
///
/// `LiveBackend` sends real orders to Polymarket via rs-clob-client.
/// `PaperBackend` simulates fills against orderbook snapshots.
/// Future: `BacktestBackend` replays historical data.
#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    /// Place an order. Returns the result (success/failure + order ID).
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult>;

    /// Cancel a specific order by ID.
    async fn cancel_order(&self, order_id: &str) -> Result<()>;

    /// Cancel all open orders.
    async fn cancel_all_orders(&self) -> Result<()>;

    /// Get all currently open orders.
    async fn get_open_orders(&self) -> Result<Vec<Order>>;

    /// Get current positions.
    async fn get_positions(&self) -> Result<Vec<Position>>;

    /// Get available USDC balance.
    async fn get_balance(&self) -> Result<rust_decimal::Decimal>;
}
```

6. Update `crates/polyrust-core/src/lib.rs`:
```rust
pub mod actions;
pub mod context;
pub mod error;
pub mod events;
pub mod execution;
pub mod strategy;
pub mod types;

pub mod prelude {
    pub use crate::actions::*;
    pub use crate::context::*;
    pub use crate::error::{PolyError, Result};
    pub use crate::events::*;
    pub use crate::execution::ExecutionBackend;
    pub use crate::strategy::Strategy;
    pub use crate::types::*;
    pub use async_trait::async_trait;
    pub use rust_decimal::Decimal;
    pub use rust_decimal_macros::dec;
}
```

**Testing:**
```fish
cargo test --workspace
```
Ensure everything compiles. Trait definitions don't need unit tests — they'll be tested via implementations.

**Commit:** `feat: define Strategy, ExecutionBackend traits and event/action types`

---

### Milestone 2: Event Bus & Engine

---

#### Task 4: Implement Typed Event Bus

**Goal:** Build the custom event bus over tokio broadcast channels with topic-based filtering.

**Files to create:**
- `crates/polyrust-core/src/event_bus.rs`

**Implementation steps:**

The EventBus uses `tokio::sync::broadcast` with a wrapper that supports:
- Topic-based subscriptions (subscribe to only `market_data` events, etc.)
- A global subscription (receive all events)
- Priority routing is NOT needed for v1 — all subscribers receive events in broadcast order

```rust
use crate::events::Event;
use tokio::sync::broadcast;
use tracing::{debug, warn};

const DEFAULT_CAPACITY: usize = 4096;

/// Typed event bus built on tokio broadcast channels.
///
/// All events are broadcast to all subscribers. Subscribers can filter
/// by topic at receive time using `EventSubscriber::recv_filtered`.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publish an event to all subscribers
    pub fn publish(&self, event: Event) {
        let topic = event.topic();
        match self.sender.send(event) {
            Ok(receivers) => {
                debug!(topic, receivers, "event published");
            }
            Err(_) => {
                warn!(topic, "event published but no active subscribers");
            }
        }
    }

    /// Create a new subscriber that receives all events
    pub fn subscribe(&self) -> EventSubscriber {
        EventSubscriber {
            receiver: self.sender.subscribe(),
            topics: None,
        }
    }

    /// Create a subscriber filtered to specific topics
    pub fn subscribe_topics(&self, topics: &[&str]) -> EventSubscriber {
        EventSubscriber {
            receiver: self.sender.subscribe(),
            topics: Some(topics.iter().map(|t| t.to_string()).collect()),
        }
    }

    /// Number of active subscribers
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

pub struct EventSubscriber {
    receiver: broadcast::Receiver<Event>,
    topics: Option<Vec<String>>,
}

impl EventSubscriber {
    /// Receive the next event, respecting topic filter.
    /// Returns None if the channel is closed.
    pub async fn recv(&mut self) -> Option<Event> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => {
                    if let Some(ref topics) = self.topics {
                        if !topics.iter().any(|t| t == event.topic()) {
                            continue; // Skip events not matching our topics
                        }
                    }
                    return Some(event);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "event subscriber lagged, skipped events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return None;
                }
            }
        }
    }
}
```

**Testing:**
- `crates/polyrust-core/tests/event_bus_test.rs`:
  - Test publishing events to multiple subscribers
  - Test topic-filtered subscriptions (market_data subscriber doesn't get system events)
  - Test empty bus (publish with no subscribers doesn't panic)
  - Test lagged subscriber (fills buffer, continues receiving)

**Verification:**
```fish
cargo test --workspace -- event_bus
```

**Commit:** `feat: implement typed event bus with topic filtering`

---

#### Task 5: Implement Engine Core

**Goal:** Build the main `Engine` struct with builder pattern, lifecycle management, and the event-dispatch loop.

**Files to create:**
- `crates/polyrust-core/src/engine.rs`
- `crates/polyrust-core/src/config.rs`

**Implementation steps:**

1. Create `crates/polyrust-core/src/config.rs`:
```rust
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub engine: EngineConfig,
    #[serde(default)]
    pub polymarket: PolymarketConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub paper: PaperConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    /// Event bus capacity
    #[serde(default = "default_event_bus_capacity")]
    pub event_bus_capacity: usize,
    /// Health check interval in seconds
    #[serde(default = "default_health_interval")]
    pub health_check_interval_secs: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            event_bus_capacity: default_event_bus_capacity(),
            health_check_interval_secs: default_health_interval(),
        }
    }
}

fn default_event_bus_capacity() -> usize { 4096 }
fn default_health_interval() -> u64 { 30 }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PolymarketConfig {
    /// Private key (hex, with or without 0x prefix)
    /// Can also be set via POLY_PRIVATE_KEY env var
    pub private_key: Option<String>,
    /// Gnosis Safe address
    pub safe_address: Option<String>,
    /// Builder API key (for gasless trading)
    pub builder_api_key: Option<String>,
    pub builder_api_secret: Option<String>,
    pub builder_api_passphrase: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    #[serde(default = "default_dashboard_enabled")]
    pub enabled: bool,
    #[serde(default = "default_dashboard_port")]
    pub port: u16,
    #[serde(default = "default_dashboard_host")]
    pub host: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: default_dashboard_enabled(),
            port: default_dashboard_port(),
            host: default_dashboard_host(),
        }
    }
}

fn default_dashboard_enabled() -> bool { true }
fn default_dashboard_port() -> u16 { 3000 }
fn default_dashboard_host() -> String { "127.0.0.1".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    /// Path to the Turso/SQLite database file
    #[serde(default = "default_db_path")]
    pub db_path: String,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self { db_path: default_db_path() }
    }
}

fn default_db_path() -> String { "polyrust.db".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_initial_balance")]
    pub initial_balance: Decimal,
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            initial_balance: default_initial_balance(),
        }
    }
}

fn default_initial_balance() -> Decimal { Decimal::new(10_000, 0) }

impl Config {
    /// Load config from a TOML file
    pub fn from_file(path: impl AsRef<Path>) -> crate::error::Result<Self> {
        let contents = std::fs::read_to_string(path.as_ref())
            .map_err(|e| crate::error::PolyError::Config(format!("Failed to read config: {e}")))?;
        toml::from_str(&contents)
            .map_err(|e| crate::error::PolyError::Config(format!("Failed to parse config: {e}")))
    }

    /// Apply environment variable overrides (POLY_* prefix)
    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(v) = std::env::var("POLY_PRIVATE_KEY") {
            self.polymarket.private_key = Some(v);
        }
        if let Ok(v) = std::env::var("POLY_SAFE_ADDRESS") {
            self.polymarket.safe_address = Some(v);
        }
        if let Ok(v) = std::env::var("POLY_BUILDER_API_KEY") {
            self.polymarket.builder_api_key = Some(v);
        }
        if let Ok(v) = std::env::var("POLY_BUILDER_API_SECRET") {
            self.polymarket.builder_api_secret = Some(v);
        }
        if let Ok(v) = std::env::var("POLY_BUILDER_API_PASSPHRASE") {
            self.polymarket.builder_api_passphrase = Some(v);
        }
        if let Ok(v) = std::env::var("POLY_DASHBOARD_PORT") {
            if let Ok(port) = v.parse() {
                self.dashboard.port = port;
            }
        }
        if let Ok(v) = std::env::var("POLY_DB_PATH") {
            self.store.db_path = v;
        }
        if let Ok(v) = std::env::var("POLY_PAPER_TRADING") {
            self.paper.enabled = v == "true" || v == "1";
        }
        self
    }
}
```

Add `toml = "0.8"` to workspace dependencies and `polyrust-core`.

2. Create `crates/polyrust-core/src/engine.rs`:

```rust
use crate::actions::Action;
use crate::config::Config;
use crate::context::StrategyContext;
use crate::error::{PolyError, Result};
use crate::event_bus::EventBus;
use crate::events::{Event, SystemEvent};
use crate::execution::ExecutionBackend;
use crate::strategy::Strategy;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Main engine that orchestrates strategies, execution, and event routing.
pub struct Engine {
    config: Config,
    event_bus: EventBus,
    strategies: Vec<Arc<RwLock<Box<dyn Strategy>>>>,
    execution: Arc<dyn ExecutionBackend>,
    context: StrategyContext,
    start_time: Option<Instant>,
}

/// Builder for constructing an Engine instance.
pub struct EngineBuilder {
    config: Option<Config>,
    strategies: Vec<Box<dyn Strategy>>,
    execution: Option<Arc<dyn ExecutionBackend>>,
}

impl EngineBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            strategies: Vec::new(),
            execution: None,
        }
    }

    pub fn config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    pub fn strategy(mut self, strategy: impl Strategy + 'static) -> Self {
        self.strategies.push(Box::new(strategy));
        self
    }

    pub fn execution(mut self, backend: impl ExecutionBackend + 'static) -> Self {
        self.execution = Some(Arc::new(backend));
        self
    }

    pub async fn build(self) -> Result<Engine> {
        let config = self.config.unwrap_or_default();
        let execution = self.execution
            .ok_or_else(|| PolyError::Config("Execution backend is required".into()))?;

        let event_bus = EventBus::with_capacity(config.engine.event_bus_capacity);
        let context = StrategyContext::new();

        // Set initial balance from execution backend
        {
            let balance = execution.get_balance().await.unwrap_or_default();
            let mut state = context.balance.write().await;
            state.available_usdc = balance;
        }

        let strategies = self.strategies
            .into_iter()
            .map(|s| Arc::new(RwLock::new(s)))
            .collect();

        Ok(Engine {
            config,
            event_bus,
            strategies,
            execution,
            context,
            start_time: None,
        })
    }
}

impl Default for EngineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine: Default::default(),
            polymarket: Default::default(),
            dashboard: Default::default(),
            store: Default::default(),
            paper: Default::default(),
        }
    }
}

impl Engine {
    pub fn builder() -> EngineBuilder {
        EngineBuilder::new()
    }

    /// Access the event bus (for market feeds and other producers to publish)
    pub fn event_bus(&self) -> &EventBus {
        &self.event_bus
    }

    /// Access shared context (for dashboard to read)
    pub fn context(&self) -> &StrategyContext {
        &self.context
    }

    /// Access config
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Run the engine. Blocks until shutdown signal (Ctrl+C).
    pub async fn run(&mut self) -> Result<()> {
        self.start_time = Some(Instant::now());
        info!("engine starting");

        // Start all strategies
        for strategy in &self.strategies {
            let mut s = strategy.write().await;
            let name = s.name().to_string();
            info!(strategy = %name, "starting strategy");
            if let Err(e) = s.on_start(&self.context).await {
                error!(strategy = %name, error = %e, "strategy failed to start");
                return Err(e);
            }
            self.event_bus.publish(Event::System(SystemEvent::StrategyStarted(name)));
        }

        self.event_bus.publish(Event::System(SystemEvent::EngineStarted));

        // Spawn strategy event loops
        let mut strategy_handles = Vec::new();
        for strategy in &self.strategies {
            let strategy = Arc::clone(strategy);
            let mut subscriber = self.event_bus.subscribe();
            let context = self.context.clone();
            let execution = Arc::clone(&self.execution);
            let event_bus = self.event_bus.clone();

            let handle = tokio::spawn(async move {
                loop {
                    let event = match subscriber.recv().await {
                        Some(e) => e,
                        None => break, // Channel closed
                    };

                    // Skip engine lifecycle events to avoid recursion
                    if matches!(&event, Event::System(SystemEvent::EngineStopping)) {
                        break;
                    }

                    let mut s = strategy.write().await;
                    let name = s.name().to_string();

                    match s.on_event(&event, &context).await {
                        Ok(actions) => {
                            for action in actions {
                                if let Err(e) = execute_action(
                                    &action, &execution, &event_bus, &name,
                                ).await {
                                    error!(
                                        strategy = %name,
                                        error = %e,
                                        "failed to execute action"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            error!(strategy = %name, error = %e, "strategy error on event");
                        }
                    }
                }
            });
            strategy_handles.push(handle);
        }

        // Wait for shutdown signal
        tokio::signal::ctrl_c().await
            .map_err(|e| PolyError::Other(e.into()))?;

        info!("shutdown signal received, stopping engine");
        self.event_bus.publish(Event::System(SystemEvent::EngineStopping));

        // Stop all strategies
        for strategy in &self.strategies {
            let mut s = strategy.write().await;
            let name = s.name().to_string();
            info!(strategy = %name, "stopping strategy");
            if let Err(e) = s.on_stop(&self.context).await {
                warn!(strategy = %name, error = %e, "strategy error on stop");
            }
            self.event_bus.publish(Event::System(SystemEvent::StrategyStopped(name)));
        }

        // Wait for strategy tasks to finish
        for handle in strategy_handles {
            let _ = handle.await;
        }

        info!("engine stopped");
        Ok(())
    }
}

/// Execute a single action from a strategy
async fn execute_action(
    action: &Action,
    execution: &Arc<dyn ExecutionBackend>,
    event_bus: &EventBus,
    strategy_name: &str,
) -> Result<()> {
    match action {
        Action::PlaceOrder(req) => {
            let result = execution.place_order(req).await?;
            event_bus.publish(Event::OrderUpdate(
                crate::events::OrderEvent::Placed(result),
            ));
        }
        Action::CancelOrder(id) => {
            execution.cancel_order(id).await?;
            event_bus.publish(Event::OrderUpdate(
                crate::events::OrderEvent::Cancelled(id.clone()),
            ));
        }
        Action::CancelAllOrders => {
            execution.cancel_all_orders().await?;
        }
        Action::Log { level, message } => {
            match level {
                crate::actions::LogLevel::Debug => tracing::debug!(strategy = %strategy_name, "{message}"),
                crate::actions::LogLevel::Info => tracing::info!(strategy = %strategy_name, "{message}"),
                crate::actions::LogLevel::Warn => tracing::warn!(strategy = %strategy_name, "{message}"),
                crate::actions::LogLevel::Error => tracing::error!(strategy = %strategy_name, "{message}"),
            }
        }
        Action::EmitSignal { signal_type, payload } => {
            event_bus.publish(Event::Signal(crate::events::SignalEvent {
                strategy_name: strategy_name.to_string(),
                signal_type: signal_type.clone(),
                payload: payload.clone(),
                timestamp: chrono::Utc::now(),
            }));
        }
        Action::SubscribeMarket(_) | Action::UnsubscribeMarket(_) => {
            // Handled by market feed manager (future task)
            warn!("market subscribe/unsubscribe not yet implemented");
        }
    }
    Ok(())
}
```

3. Update `crates/polyrust-core/src/lib.rs` to include new modules:
```rust
pub mod actions;
pub mod config;
pub mod context;
pub mod engine;
pub mod error;
pub mod event_bus;
pub mod events;
pub mod execution;
pub mod strategy;
pub mod types;

pub mod prelude {
    pub use crate::actions::*;
    pub use crate::config::Config;
    pub use crate::context::*;
    pub use crate::engine::Engine;
    pub use crate::error::{PolyError, Result};
    pub use crate::event_bus::EventBus;
    pub use crate::events::*;
    pub use crate::execution::ExecutionBackend;
    pub use crate::strategy::Strategy;
    pub use crate::types::*;
    pub use async_trait::async_trait;
    pub use rust_decimal::Decimal;
    pub use rust_decimal_macros::dec;
}
```

**Testing:**
- Integration test: create a mock strategy + mock execution backend, run engine briefly, verify events flow correctly.
- Test EngineBuilder validation (missing execution backend returns error).
- Test config loading from TOML string + env override.

**Verification:**
```fish
cargo test --workspace
```

**Commit:** `feat: implement engine core with builder pattern and event dispatch loop`

---

### Milestone 3: Persistence Layer (Turso)

---

#### Task 6: Implement Turso Store

**Goal:** Build the persistence layer for trades, orders, events, and PnL snapshots using Turso embedded.

**Files to touch:**
- `crates/polyrust-store/Cargo.toml` — add turso dependency
- `crates/polyrust-store/src/lib.rs`
- `crates/polyrust-store/src/db.rs` — connection + migrations
- `crates/polyrust-store/src/trades.rs` — trade log
- `crates/polyrust-store/src/orders.rs` — order history
- `crates/polyrust-store/src/events.rs` — event audit log
- `crates/polyrust-store/src/snapshots.rs` — PnL snapshots

**Implementation steps:**

1. Add to `crates/polyrust-store/Cargo.toml`:
```toml
[dependencies]
polyrust-core.workspace = true
turso = "0.1"    # Check latest version on crates.io
serde.workspace = true
serde_json.workspace = true
chrono.workspace = true
rust_decimal.workspace = true
uuid.workspace = true
tracing.workspace = true
thiserror.workspace = true
```

> **Important**: Turso is in beta. If the `turso` crate is not yet published on crates.io, use `libsql` crate instead (`libsql = "0.6"`) which is the stable embedded SQLite library from the Turso team. The API is similar: `Builder::new_local(path).build().await?` → `conn.execute()` / `conn.query()`. Adjust imports accordingly.

2. `crates/polyrust-store/src/db.rs` — connection manager:

```rust
use crate::error::StoreResult;
use tracing::info;

pub struct Store {
    db: turso::Database,  // or libsql::Database
}

impl Store {
    pub async fn new(path: &str) -> StoreResult<Self> {
        let db = turso::Builder::new_local(path)
            .build()
            .await
            .map_err(|e| crate::error::StoreError::Connection(e.to_string()))?;

        let store = Self { db };
        store.run_migrations().await?;
        Ok(store)
    }

    pub fn connect(&self) -> StoreResult<turso::Connection> {
        self.db.connect()
            .map_err(|e| crate::error::StoreError::Connection(e.to_string()))
    }

    async fn run_migrations(&self) -> StoreResult<()> {
        let conn = self.connect()?;
        info!("running database migrations");

        conn.execute_batch("
            CREATE TABLE IF NOT EXISTS trades (
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
            );

            CREATE TABLE IF NOT EXISTS orders (
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
            );

            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_type TEXT NOT NULL,
                topic TEXT NOT NULL,
                payload TEXT NOT NULL,
                timestamp TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS pnl_snapshots (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                total_pnl TEXT NOT NULL,
                unrealized_pnl TEXT NOT NULL,
                realized_pnl TEXT NOT NULL,
                open_positions INTEGER NOT NULL,
                open_orders INTEGER NOT NULL,
                available_balance TEXT NOT NULL,
                timestamp TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE INDEX IF NOT EXISTS idx_trades_strategy ON trades(strategy_name);
            CREATE INDEX IF NOT EXISTS idx_trades_timestamp ON trades(timestamp);
            CREATE INDEX IF NOT EXISTS idx_orders_status ON orders(status);
            CREATE INDEX IF NOT EXISTS idx_events_topic ON events(topic);
            CREATE INDEX IF NOT EXISTS idx_pnl_timestamp ON pnl_snapshots(timestamp);
        ").await.map_err(|e| crate::error::StoreError::Migration(e.to_string()))?;

        info!("migrations complete");
        Ok(())
    }
}
```

3. Implement `trades.rs`, `orders.rs`, `events.rs`, `snapshots.rs` with insert/query methods. Each module follows the same pattern:
   - `insert_*()` — insert a record
   - `get_*()` — query by ID
   - `list_*()` — list with optional filters (strategy_name, date range, limit)
   - Use `Decimal` as TEXT in SQLite (store as string, parse on read) for precision

4. Create `crates/polyrust-store/src/error.rs`:
```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("Connection error: {0}")]
    Connection(String),
    #[error("Migration error: {0}")]
    Migration(String),
    #[error("Query error: {0}")]
    Query(String),
}

pub type StoreResult<T> = std::result::Result<T, StoreError>;
```

**Testing:**
- Use in-memory database for tests (`Builder::new_local(":memory:")`)
- Test insert + query roundtrip for each table
- Test that migrations are idempotent (run twice, no errors)

**Commit:** `feat: implement Turso persistence layer with trades, orders, events, and PnL snapshots`

---

### Milestone 4: Market Data & Execution Backends

---

#### Task 7: Implement Market Data Feeds

**Goal:** Build CLOB orderbook feed and RTDS crypto price feed using `rs-clob-client` WebSocket features.

**Files to touch:**
- `crates/polyrust-market/Cargo.toml`
- `crates/polyrust-market/src/lib.rs`
- `crates/polyrust-market/src/feed.rs` — MarketDataFeed trait
- `crates/polyrust-market/src/clob_feed.rs` — CLOB WebSocket feed
- `crates/polyrust-market/src/price_feed.rs` — RTDS price feed
- `crates/polyrust-market/src/orderbook.rs` — orderbook aggregation

**Implementation steps:**

1. Define the `MarketDataFeed` trait in `feed.rs`:
```rust
use polyrust_core::prelude::*;
use async_trait::async_trait;

#[async_trait]
pub trait MarketDataFeed: Send + Sync {
    async fn start(&mut self, event_bus: EventBus) -> Result<()>;
    async fn subscribe_market(&mut self, market: &MarketInfo) -> Result<()>;
    async fn unsubscribe_market(&mut self, market_id: &str) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
}
```

2. `clob_feed.rs` — wrap `rs-clob-client`'s `ws` feature:
   - Connect to Polymarket WebSocket
   - Subscribe to token IDs for orderbook updates
   - Parse messages into `OrderbookSnapshot` structs
   - Publish `Event::MarketData(MarketDataEvent::OrderbookUpdate(...))` to EventBus
   - Handle reconnection with exponential backoff

   **Key reference**: See `../polymarket-trading-bot/src/websocket_client.py` for message format and reconnection logic. The `rs-clob-client` SDK handles most of this — use its WebSocket API directly.

3. `price_feed.rs` — wrap `rs-clob-client`'s `rtds` feature:
   - Connect to RTDS WebSocket
   - Subscribe to `crypto_prices_chainlink` topic (used for Polymarket resolution)
   - Also subscribe to `crypto_prices` (Binance) for faster updates
   - Parse into `Event::MarketData(MarketDataEvent::ExternalPrice { ... })`
   - Maintain a thread-safe price cache (`Arc<RwLock<HashMap<String, CryptoPrice>>>`)

4. `orderbook.rs` — local orderbook state:
   - Maintain latest `OrderbookSnapshot` per token ID
   - Provide helper methods: `best_bid()`, `best_ask()`, `mid_price()`, `spread()`
   - Used by both the paper trading engine and strategies

**Testing:**
- Unit test orderbook aggregation with mock data
- Integration tests require live WebSocket (mark as `#[ignore]` for CI)

**Commit:** `feat: implement CLOB and RTDS market data feeds`

---

#### Task 8: Implement Live Execution Backend

**Goal:** Implement `ExecutionBackend` using `rs-clob-client` for real order placement.

**Files to touch:**
- `crates/polyrust-execution/Cargo.toml`
- `crates/polyrust-execution/src/lib.rs`
- `crates/polyrust-execution/src/live.rs`

**Implementation steps:**

1. `live.rs` wraps `rs-clob-client`'s authenticated `Client`:
```rust
use polyrust_core::prelude::*;
use async_trait::async_trait;

pub struct LiveBackend {
    // The authenticated rs-clob-client Client
    // client: polymarket_client_sdk::clob::Client<Authenticated>,
}

impl LiveBackend {
    pub async fn new(config: &Config) -> Result<Self> {
        // 1. Create signer from private key
        // 2. Build Client with authentication (Builder or EOA mode)
        // 3. Match whatever auth modes rs-clob-client supports
        todo!("Wire up rs-clob-client authentication")
    }
}

#[async_trait]
impl ExecutionBackend for LiveBackend {
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult> {
        // 1. Build order using client.limit_order() or client.market_order()
        // 2. Sign with signer
        // 3. Submit via client.post_order()
        // 4. Map response to OrderResult
        todo!()
    }

    async fn cancel_order(&self, order_id: &str) -> Result<()> {
        // client.cancel_order(order_id)
        todo!()
    }

    async fn cancel_all_orders(&self) -> Result<()> {
        // client.cancel_all_orders()
        todo!()
    }

    async fn get_open_orders(&self) -> Result<Vec<Order>> {
        // client.get_open_orders() → map to domain types
        todo!()
    }

    async fn get_positions(&self) -> Result<Vec<Position>> {
        // Use data API feature to get positions
        todo!()
    }

    async fn get_balance(&self) -> Result<Decimal> {
        // Query USDC balance
        todo!()
    }
}
```

**Key patterns from Python bot to replicate:**
- Order amounts must be rounded per tick size (see `src/signer.py` `ROUNDING_CONFIG`)
- Builder HMAC auth for gasless (rs-clob-client handles this via its auth builder)
- `neg_risk` flag varies by market type (15-min markets are `false`)

**Reference**: `../polymarket-trading-bot/src/bot.py` for the full API surface and `src/client.py` for auth patterns. Most of this is handled by `rs-clob-client` — the task is mapping domain types to SDK types and back.

**Testing:**
- Mock the rs-clob-client (or use its test utilities if available)
- Test order request → SDK order mapping
- Test order result → domain type mapping
- Live tests marked `#[ignore]`

**Commit:** `feat: implement live execution backend with rs-clob-client`

---

#### Task 9: Implement Paper Execution Backend

**Goal:** Implement `ExecutionBackend` for simulated paper trading. Port logic from `../polymarket-trading-bot/src/paper/engine.py`.

**Files to create:**
- `crates/polyrust-execution/src/paper.rs`

**Implementation steps:**

Port from Python's `PaperEngine`. Key behaviors:
- Maintain in-memory state: orders, positions, USDC balance
- Two fill modes: `Immediate` (fill at order price) and `Orderbook` (match against real orderbook levels)
- BUY validation: check `available_usdc >= price * size`
- SELL validation: check position has enough shares
- Generate synthetic `OrderId`s (UUID)
- Track partial fills when using orderbook mode
- `update_orders_with_orderbook()` method to process pending orders against new orderbook snapshots

```rust
pub struct PaperBackend {
    state: Arc<RwLock<PaperState>>,
    fill_mode: FillMode,
}

struct PaperState {
    usdc_balance: Decimal,
    positions: HashMap<TokenId, Decimal>,  // token_id → share count
    open_orders: HashMap<OrderId, PaperOrder>,
}

#[derive(Debug, Clone, Copy)]
pub enum FillMode {
    Immediate,
    Orderbook,
}
```

**Testing:**
- Test BUY order reduces balance, creates position
- Test SELL order increases balance, reduces position
- Test insufficient balance rejection
- Test insufficient position rejection
- Test orderbook fill mode with mock orderbook data
- Test cancel order
- Test cancel all orders

**Reference**: `../polymarket-trading-bot/src/paper/engine.py` lines 1-300 for the complete paper trading logic.

**Commit:** `feat: implement paper trading execution backend`

---

### Milestone 5: Dashboard

---

#### Task 10: Implement Axum + HTMX Monitoring Dashboard

**Goal:** Build a server-rendered monitoring dashboard showing positions, PnL, trade log, and system health. Real-time updates via SSE (Server-Sent Events).

**Files to touch:**
- `crates/polyrust-dashboard/Cargo.toml`
- `crates/polyrust-dashboard/src/lib.rs`
- `crates/polyrust-dashboard/src/server.rs` — Axum router + SSE
- `crates/polyrust-dashboard/src/handlers.rs` — page handlers
- `crates/polyrust-dashboard/src/templates/` — HTML templates

**Dependencies to add:**
```toml
[dependencies]
polyrust-core.workspace = true
polyrust-store = { workspace = true }
axum = "0.8"
askama = "0.13"
askama_axum = "0.5"
tower-http = { version = "0.6", features = ["fs", "cors"] }
tokio.workspace = true
tokio-stream.workspace = true
serde.workspace = true
serde_json.workspace = true
tracing.workspace = true
```

**Implementation steps:**

1. `server.rs` — set up Axum router:
```rust
pub struct Dashboard {
    context: StrategyContext,
    store: Arc<Store>,
    event_bus: EventBus,
}

impl Dashboard {
    pub fn new(context: StrategyContext, store: Arc<Store>, event_bus: EventBus) -> Self {
        Self { context, store, event_bus }
    }

    pub async fn serve(self, host: &str, port: u16) -> Result<()> {
        let app = Router::new()
            .route("/", get(handlers::index))
            .route("/positions", get(handlers::positions))
            .route("/trades", get(handlers::trades))
            .route("/health", get(handlers::health))
            .route("/events/stream", get(handlers::sse_events))
            .with_state(AppState { ... });

        let addr = format!("{host}:{port}");
        info!("dashboard listening on http://{addr}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;
        Ok(())
    }
}
```

2. Page handlers return Askama templates with HTMX attributes:
   - `/` — overview page (active strategies, total PnL, system uptime)
   - `/positions` — table of open positions with unrealized PnL
   - `/trades` — paginated trade history from Turso
   - `/health` — system health (uptime, event bus stats, DB size)

3. SSE endpoint `/events/stream` — subscribe to EventBus, stream events as SSE:
   - HTMX listens via `hx-sse="connect:/events/stream"`
   - Partial HTML fragments update specific DOM elements
   - Position table auto-updates when `PositionChange` events arrive
   - PnL display updates on each `OrderUpdate` fill event

4. HTML templates (use Askama with HTMX):
   - Base layout with navigation, HTMX script include
   - Tailwind CSS via CDN for styling (minimal, functional)
   - Tables for positions and trades
   - Cards for PnL summary and health metrics

**Testing:**
- Test that routes return 200 status codes
- Test SSE endpoint connects and receives events
- Visual testing: run dashboard, verify in browser

**Commit:** `feat: implement Axum + HTMX monitoring dashboard`

---

### Milestone 6: Reference Strategy (Crypto Arbitrage)

---

#### Task 11: Port Crypto Arbitrage Strategy

**Goal:** Port `../polymarket-trading-bot/strategies/crypto_arbitrage.py` to Rust as a reference `Strategy` implementation. This is the most complex task — the Python strategy is 2000+ lines.

**Files to touch:**
- `crates/polyrust-strategies/Cargo.toml`
- `crates/polyrust-strategies/src/lib.rs`
- `crates/polyrust-strategies/src/crypto_arb.rs`
- `crates/polyrust-strategies/src/crypto_arb/` (split into submodules if needed)

**Implementation steps:**

The strategy has 3 distinct trading modes and a complex confidence model. Port in this order:

**Step 1: Data structures**
```rust
/// Market enriched with reference crypto price at discovery time
struct MarketWithReference {
    market: MarketInfo,
    reference_price: Decimal,
    reference_approximate: bool, // true if mid-window discovery
    discovery_time: DateTime<Utc>,
    coin: String,
}

impl MarketWithReference {
    /// Predict winner based on current crypto price vs reference
    fn predict_winner(&self, current_price: Decimal) -> OutcomeSide { ... }

    /// Multi-signal confidence score (0-1)
    fn get_confidence(&self, current_price: Decimal, market_price: Decimal) -> Decimal { ... }
}

/// A detected arbitrage opportunity
struct ArbitrageOpportunity {
    mode: ArbitrageMode,
    market: MarketWithReference,
    outcome_to_buy: OutcomeSide,
    token_id: TokenId,
    buy_price: Decimal,
    confidence: Decimal,
    profit_margin: Decimal,
}

enum ArbitrageMode {
    TailEnd,      // < 2 min remaining, market >= 90%
    TwoSided,     // Both outcomes < $1, guaranteed profit
    Confirmed,    // Standard directional bet
}
```

**Step 2: Confidence model** (port from `compute_dynamic_confidence`)
```rust
/// Time-aware confidence calculation
/// Reference: crypto_arbitrage.py evaluate_opportunity()
fn compute_confidence(
    distance_pct: Decimal,     // |current - reference| / reference
    time_remaining_secs: i64,
    market_price: Decimal,     // Current Polymarket price of predicted winner
    volatility: Option<Decimal>,
    momentum: Option<Decimal>,
) -> Decimal {
    // Tail-end mode (< 120s): if market_price >= 0.90, confidence = 1.0
    // Late window (120-300s): base = distance_pct * 66, market boost
    // Early window (>300s): base = distance_pct * 50
    // Apply volatility damping and momentum adjustment
    ...
}
```

**Step 3: Implement Strategy trait**
```rust
pub struct CryptoArbitrageStrategy {
    config: ArbitrageConfig,
    // State
    active_markets: HashMap<MarketId, MarketWithReference>,
    price_history: HashMap<String, VecDeque<(DateTime<Utc>, Decimal)>>,
    positions: HashMap<MarketId, ArbitragePosition>,
}

#[async_trait]
impl Strategy for CryptoArbitrageStrategy {
    fn name(&self) -> &str { "crypto-arbitrage" }
    fn description(&self) -> &str { "Exploits mispricing in 15-min Up/Down crypto markets" }

    async fn on_start(&mut self, ctx: &StrategyContext) -> Result<()> {
        // Subscribe to crypto price feeds
        // Discover initial 15-min markets via Gamma API
        Ok(())
    }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        match event {
            Event::MarketData(MarketDataEvent::ExternalPrice { symbol, price, .. }) => {
                self.on_crypto_price(symbol, *price, ctx).await
            }
            Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot)) => {
                self.on_orderbook_update(snapshot, ctx).await
            }
            Event::MarketData(MarketDataEvent::MarketDiscovered(market)) => {
                self.on_market_discovered(market, ctx).await
            }
            Event::MarketData(MarketDataEvent::MarketExpired(id)) => {
                self.on_market_expired(id, ctx).await
            }
            _ => Ok(vec![]),
        }
    }
}
```

**Step 4: Core logic methods**
- `evaluate_opportunity()` — check all 3 modes (tail-end, two-sided, confirmed)
- `on_crypto_price()` — update price history, check for opportunities
- `on_orderbook_update()` — update market prices, check stop-losses
- `check_stop_loss()` — port stop-loss logic (0.5% reversal + 5¢ min drop)
- Market discovery: use `rs-clob-client`'s gamma feature to discover 15-min markets
- Position cleanup: handle market resolution, redemption attempts

**Configuration:**
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbitrageConfig {
    pub coins: Vec<String>,              // ["BTC", "ETH", "SOL", "XRP"]
    pub position_size: Decimal,          // USDC per trade (e.g., 5.0)
    pub max_positions: usize,            // Max concurrent (e.g., 5)
    pub min_profit_margin: Decimal,      // Standard min (e.g., 0.03)
    pub late_window_margin: Decimal,     // Late window min (e.g., 0.02)
    pub stop_loss_reversal_pct: Decimal, // Stop-loss trigger (e.g., 0.005)
    pub stop_loss_min_drop: Decimal,     // Min price drop for stop (e.g., 0.05)
    pub scan_interval_secs: u64,         // Market discovery interval (e.g., 30)
    pub use_chainlink: bool,             // Use Chainlink for resolution
}
```

**Testing:**
- Test `predict_winner()` — BTC goes up → Up wins, BTC goes down → Down wins
- Test `compute_confidence()` — tail-end returns 1.0, early window returns lower values
- Test `evaluate_opportunity()` with mock market data for each of the 3 modes
- Test stop-loss logic — trigger conditions, don't trigger in final 60s
- Test two-sided mode — combined price < 0.98 detected correctly
- Test market expiration cleanup

**Reference**: Read `../polymarket-trading-bot/strategies/crypto_arbitrage.py` carefully. Key methods:
- `evaluate_opportunity()` (line ~600)
- `compute_dynamic_confidence()` (line ~700)
- `_check_stop_losses()` (line ~1200)
- `_cleanup_ended_positions()` (line ~1400)

**Commit:** `feat: port crypto arbitrage reference strategy from Python`

---

### Milestone 7: Binary Entry Point & Integration

---

#### Task 12: Wire Everything Together in main.rs

**Goal:** Create the binary entry point that loads config, initializes all components, and runs the engine.

**Files to touch:**
- `src/main.rs`
- `config/default.toml`

**Implementation steps:**

1. Create `config/default.toml`:
```toml
[engine]
event_bus_capacity = 4096
health_check_interval_secs = 30

[polymarket]
# Set via environment variables:
# POLY_PRIVATE_KEY, POLY_SAFE_ADDRESS
# POLY_BUILDER_API_KEY, POLY_BUILDER_API_SECRET, POLY_BUILDER_API_PASSPHRASE

[dashboard]
enabled = true
port = 3000
host = "127.0.0.1"

[store]
db_path = "polyrust.db"

[paper]
enabled = true
initial_balance = 10000
```

2. Update `src/main.rs`:
```rust
use polyrust_core::prelude::*;
use polyrust_dashboard::Dashboard;
use polyrust_execution::{LiveBackend, PaperBackend, FillMode};
use polyrust_market::{ClobFeed, PriceFeed};
use polyrust_store::Store;
use polyrust_strategies::CryptoArbitrageStrategy;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,polyrust=debug")),
        )
        .init();

    // Load config
    let config = Config::from_file("config/default.toml")
        .unwrap_or_default()
        .with_env_overrides();

    // Initialize store
    let store = Arc::new(Store::new(&config.store.db_path).await?);

    // Choose execution backend
    let execution: Box<dyn ExecutionBackend> = if config.paper.enabled {
        Box::new(PaperBackend::new(config.paper.initial_balance, FillMode::Orderbook))
    } else {
        Box::new(LiveBackend::new(&config).await?)
    };

    // Build engine
    let mut engine = Engine::builder()
        .config(config.clone())
        .strategy(CryptoArbitrageStrategy::new(Default::default()))
        .execution(execution)
        .build()
        .await?;

    // Start market data feeds (publish to engine's event bus)
    let event_bus = engine.event_bus().clone();
    // TODO: Start ClobFeed and PriceFeed, pass event_bus

    // Start dashboard in background
    if config.dashboard.enabled {
        let dashboard = Dashboard::new(
            engine.context().clone(),
            Arc::clone(&store),
            engine.event_bus().clone(),
        );
        tokio::spawn(async move {
            if let Err(e) = dashboard.serve(
                &config.dashboard.host,
                config.dashboard.port,
            ).await {
                tracing::error!("dashboard error: {e}");
            }
        });
    }

    // Run engine (blocks until Ctrl+C)
    engine.run().await?;

    Ok(())
}
```

3. Create `examples/simple_strategy.rs`:
```rust
//! Minimal strategy example — logs every market data event.
use polyrust_core::prelude::*;

struct LoggingStrategy;

#[async_trait]
impl Strategy for LoggingStrategy {
    fn name(&self) -> &str { "logger" }
    fn description(&self) -> &str { "Logs every event (example strategy)" }

    async fn on_event(&mut self, event: &Event, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        Ok(vec![Action::Log {
            level: LogLevel::Info,
            message: format!("received event: {}", event.topic()),
        }])
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let mut engine = Engine::builder()
        .strategy(LoggingStrategy)
        .execution(polyrust_execution::PaperBackend::new(
            rust_decimal_macros::dec!(10000),
            polyrust_execution::FillMode::Immediate,
        ))
        .build()
        .await?;

    engine.run().await?;
    Ok(())
}
```

**Testing:**
```fish
# Paper mode (default)
cargo run

# With env overrides
POLY_PAPER_TRADING=true RUST_LOG=debug cargo run

# Run example
cargo run --example simple_strategy
```

**Verification:** Binary starts, dashboard serves at `http://127.0.0.1:3000`, Ctrl+C shuts down cleanly.

**Commit:** `feat: wire binary entry point with config, store, dashboard, and strategy`

---

#### Task 13: Add CLAUDE.md Developer Guide

**Goal:** Create the CLAUDE.md for the polyrust project with development guidelines.

**File to create:**
- `CLAUDE.md`

**Content should cover:**
- Project overview and architecture summary
- How to build: `cargo build`, `cargo test --workspace`
- Never run `go build` (this is Rust, not Go) — but keep the "never commit binaries" rule
- Crate dependency graph
- How to add a new strategy (implement `Strategy` trait)
- How to run in paper mode vs live mode
- Config file location and env var overrides
- Key domain concepts (token IDs, prices as 0-1 probabilities, USDC 6 decimals, tick sizes)
- rs-clob-client feature flags in use
- Testing patterns

**Commit:** `docs: add CLAUDE.md developer guide`

---

## 5. Testing Strategy

### Test Types

| Type | Location | What to Test | How to Run |
|------|----------|-------------|------------|
| Unit | `crates/*/tests/` or inline `#[cfg(test)]` | Individual functions, type methods, serialization | `cargo test --workspace` |
| Integration | `crates/*/tests/` | Cross-module interactions (engine + strategy + execution) | `cargo test --workspace` |
| Live/E2E | Marked `#[ignore]` | Real Polymarket API calls | `cargo test --workspace -- --ignored` |

### Key Test Patterns

1. **Mock ExecutionBackend**: Create a `MockBackend` struct implementing `ExecutionBackend` that records calls and returns predefined results. Use in engine and strategy tests.

2. **In-memory Turso**: Use `":memory:"` path for all store tests. Each test gets a fresh database.

3. **Deterministic time**: For strategy tests involving time (market expiration, confidence windows), use `chrono::Utc::now()` in production but inject fixed timestamps in tests via a `Clock` trait or by passing `DateTime<Utc>` parameters.

4. **Decimal precision**: Always use `rust_decimal_macros::dec!()` macro in tests for precise decimal literals. Never use `Decimal::from_f64()`.

5. **Event bus tests**: Use `tokio::time::timeout` to prevent hanging tests when waiting for events.

### Coverage Expectations

- Core types and domain logic: >90%
- Strategy confidence model: 100% (this is the money-making logic)
- Paper trading engine: >90%
- Dashboard handlers: >70% (focus on data correctness, not HTML structure)
- Live backend: minimal (mostly SDK passthrough, tested via integration)

---

## 6. Documentation Updates

| Document | When | Content |
|----------|------|---------|
| `README.md` | Task 12 | Project overview, quickstart, architecture diagram, config reference |
| `CLAUDE.md` | Task 13 | Developer guide (see Task 13) |
| `docs/brainstorms/polyrust-trading-framework.md` | Already exists | Design doc (reference only) |
| Inline `///` doc comments | Every task | All public types, traits, and functions |
| `examples/simple_strategy.rs` | Task 12 | Minimal working example |

---

## 7. Definition of Done

- [ ] **Workspace compiles**: `cargo build` succeeds with zero warnings
- [ ] **All tests pass**: `cargo test --workspace` passes
- [ ] **Core traits defined**: `Strategy`, `ExecutionBackend`, `MarketDataFeed` with doc comments
- [ ] **Event bus works**: Typed events with topic filtering, tested
- [ ] **Engine runs**: Builder pattern, lifecycle (start → run → stop), event dispatch
- [ ] **Turso persistence**: Trades, orders, events, PnL snapshots stored and queryable
- [ ] **Live backend**: Orders placed/cancelled via rs-clob-client
- [ ] **Paper backend**: Simulated fills with balance tracking, tested
- [ ] **Market data feeds**: CLOB orderbook + RTDS crypto prices → EventBus
- [ ] **Dashboard serves**: Axum + HTMX at localhost:3000 with positions, trades, health
- [ ] **SSE live updates**: Dashboard updates in real-time via Server-Sent Events
- [ ] **Crypto arb strategy**: All 3 modes (tail-end, two-sided, confirmed), confidence model, stop-loss
- [ ] **Config system**: TOML file + env var overrides
- [ ] **Single binary**: `cargo build --release` produces one executable
- [ ] **Developer guide**: CLAUDE.md with architecture and contribution patterns
- [ ] **Example strategy**: `examples/simple_strategy.rs` compiles and runs

---

## Dependency Graph (Build Order)

```
polyrust-core          ← Foundation (no internal deps)
    ↓
polyrust-store         ← Depends on core (types)
polyrust-market        ← Depends on core (types, events, event_bus)
polyrust-execution     ← Depends on core (types, execution trait)
    ↓
polyrust-strategies    ← Depends on core (strategy trait) + market (feeds)
polyrust-dashboard     ← Depends on core (context) + store (queries)
    ↓
polyrust (binary)      ← Depends on all crates
```

Build and implement in this order. Each milestone produces a working artifact.

---
*Generated via /brainstorm-plan on 2026-01-27*
