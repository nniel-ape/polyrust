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

# Docker deployment
docker-compose up -d             # Build and start bot in background
docker-compose logs -f polyrust  # View real-time logs
docker-compose down              # Stop and remove containers
docker-compose restart           # Restart after config changes
docker-compose build --no-cache  # Rebuild from scratch
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
- **`DashboardViewProvider`** — `view_name(&self) -> &str` + `render_view(&self) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>>` — optional async trait for strategies to expose custom dashboard pages at `/strategy/<name>`. Dashboard view names must be unique; the engine returns `PolyError::Config` on collision.

### Event Flow

1. Market feeds publish events to `EventBus` (tokio broadcast channels with topic filtering)
2. Engine routes events to all registered strategies
3. Strategies return `Vec<Action>` (PlaceOrder, CancelOrder, EmitSignal, etc.)
4. Engine executes actions via the execution backend
5. Action results become new events, flowing back through the bus
6. Trade persistence handler subscribes to `OrderEvent::Filled` events
7. For each fill, calculates realized P&L (for closing trades) and persists to database
8. Dashboard queries persisted trades for historical analysis

### Shared State

`StrategyContext` provides thread-safe access via `Arc<RwLock<...>>`:
- `PositionState` — open positions and orders
- `MarketDataState` — orderbooks (auto-populated by engine on OrderbookUpdate events), market info, external prices
- `BalanceState` — available and locked USDC
- `strategy_views` — registered `DashboardViewProvider` implementations (keyed by strategy name)

### Strategy Dashboard Views

Strategies can expose custom dashboard pages via the `DashboardViewProvider` trait (`crates/polyrust-core/src/dashboard_view.rs`). Each strategy optionally returns a view provider from `dashboard_view()`, which asynchronously renders an HTML fragment for `/strategy/:name`. The dashboard auto-generates nav links for all registered strategy views.

Real-time updates use SSE: strategies emit `"dashboard-update"` signals, the SSE handler re-renders the view, and HTMX swaps the content in the browser. For dashboard-only views (no event processing), use `DashboardStrategyWrapper` in `src/main.rs` to register a view provider as a no-op strategy. See the crypto arbitrage strategy's per-mode dashboards for a reference implementation.

## Domain Concepts

- **Token IDs**: Each market has 2 outcomes (Up/Down or Yes/No), each is an ERC-1155 token
- **Prices**: Probabilities in [0, 1] range — use `rust_decimal::Decimal`, never floats
- **USDC**: 6 decimal places; store as `Decimal`, persist as TEXT in SQLite
- **Tick sizes**: Typically 0.01 (2 decimal price, 2 decimal size, 4 decimal amount)
- **neg_risk**: Boolean on orders — false for 15-minute markets (most common)

## Configuration

Copy `config.example.toml` → `config.toml` and customize. Environment variable overrides use `POLY_*` prefix:
- `POLY_PRIVATE_KEY`, `POLY_SAFE_ADDRESS` — wallet credentials
- `POLY_BUILDER_API_KEY`, `POLY_BUILDER_API_SECRET`, `POLY_BUILDER_API_PASSPHRASE` — builder API
- `POLY_DASHBOARD_HOST`, `POLY_DASHBOARD_PORT`, `POLY_DB_PATH`, `POLY_PAPER_TRADING` — runtime settings
- `POLY_RPC_URLS` — comma-separated Polygon RPC endpoints for Chainlink oracle queries

Paper mode: `[paper] enabled = true` or `POLY_PAPER_TRADING=true`
Docker deployment: Set `POLY_DASHBOARD_HOST=0.0.0.0` in `docker-compose.yml` to allow access from host machine.

Strategy configuration: Add `[arbitrage]` section (and nested `[arbitrage.tailend]`, `[arbitrage.twosided]`, `[arbitrage.confirmed]`, `[arbitrage.correlation]`) to `config.toml`. All trading modes are disabled by default and must be explicitly enabled with `enabled = true`. See `config.example.toml` for the complete reference.

## Adding a New Strategy

1. Add `polyrust-core` as a dependency in your crate
2. Implement the `Strategy` trait on your struct
3. Register with `Engine::builder().strategy(YourStrategy::new())`
4. (Optional) Implement `DashboardViewProvider` for a custom dashboard page

```rust
use std::pin::Pin;
use std::future::Future;
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
    fn render_view(&self) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async { Ok("<div>Strategy-specific HTML here</div>".to_string()) })
    }
}
```

See `examples/simple_strategy.rs` for a complete runnable example.

## Paper Mode vs Live Mode

- Paper mode (default): `[paper] enabled = true` in `config.toml` or `POLY_PAPER_TRADING=true`
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

Ported from Python (`../polymarket-trading-bot/`). Exploits mispricing in 15-minute Up/Down crypto markets with four modes:
1. **Tail-End** (<2 min remaining, market >= 90%) — highest confidence
2. **Two-Sided** (both outcomes < $1 combined) — guaranteed profit
3. **Confirmed** (dynamic confidence model) — standard directional trading
4. **Cross-Correlated** (follower coin triggered by leader spike) — correlation-based signals

### Strategy Configuration

The crypto arbitrage strategy is configured via the `[arbitrage]` section in `config.toml`. It uses a modular directory structure (`crates/polyrust-strategies/src/crypto_arb/`) with per-mode strategy structs sharing state through `Arc<CryptoArbBase>`:

- `TailEndStrategy` — high-confidence trades near expiration
- `TwoSidedStrategy` — risk-free arbitrage
- `ConfirmedStrategy` — directional trades with confidence model
- `CrossCorrStrategy` — correlation-based signals

Each mode is conditionally registered with the engine based on its `enabled` flag.

```rust
pub struct ArbitrageConfig {
    // Core settings
    pub coins: Vec<String>,
    pub max_positions: usize,
    pub min_profit_margin: Decimal,
    pub late_window_margin: Decimal,
    pub scan_interval_secs: u64,
    pub use_chainlink: bool,

    // Per-mode configs (each mode disabled by default)
    pub tailend: TailEndConfig,       // TailEnd mode (enabled, time_threshold_secs, ask_threshold)
    pub twosided: TwoSidedConfig,     // TwoSided mode (enabled, combined_threshold)
    pub confirmed: ConfirmedConfig,   // Confirmed mode (enabled, min_confidence, min_margin)
    pub correlation: CorrelationConfig, // Cross-market correlation pairs

    // Shared configs (all with #[serde(default)])
    pub fee: FeeConfig,           // Taker fee model (default 3.15% at 50/50)
    pub spike: SpikeConfig,       // Spike detection (threshold, window, history)
    pub order: OrderConfig,       // Hybrid GTC/FOK orders (maker vs taker)
    pub sizing: SizingConfig,     // Kelly criterion position sizing
    pub stop_loss: StopLossConfig, // Dual-trigger + trailing stops
    pub performance: PerformanceConfig, // Performance tracking & auto-disable
}
```

#### Sub-Config Breakdown

- **TailEndConfig**: Per-mode toggle (`enabled`), `time_threshold_secs` (120), `ask_threshold` (0.90)
- **TwoSidedConfig**: Per-mode toggle (`enabled`), `combined_threshold` (0.98)
- **ConfirmedConfig**: Per-mode toggle (`enabled`), `min_confidence` (0.50), `min_margin` (0.02)
- **CorrelationConfig**: Per-mode toggle (`enabled`), cross-market pairs, `discount_factor` (0.7)
- **FeeConfig**: Taker fee rate for net profit margin calculation
- **SpikeConfig**: Price spike detection (threshold_pct, window_secs, history_size)
- **OrderConfig**: Hybrid order mode (hybrid_mode, limit_offset, max_age_secs)
  - GTC maker orders for Confirmed/TwoSided modes (0% fee)
  - FOK taker orders for TailEnd mode (speed matters)
- **SizingConfig**: Kelly criterion sizing (base_size, kelly_multiplier, min/max_size, use_kelly)
  - Scales position size with confidence and edge
  - Falls back to fixed sizing when disabled or for TwoSided mode
- **StopLossConfig**: Dual-trigger stop-loss + trailing stops
  - reversal_pct: crypto price reversal threshold
  - min_drop: minimum market price drop
  - trailing_enabled, trailing_distance: lock in profits as bid rises
  - time_decay: tighten stops near expiration
- **PerformanceConfig**: Per-mode tracking and auto-disable
  - Tracks win rate, P&L per mode (TailEnd, TwoSided, Confirmed, CrossCorrelated)
  - Auto-disable modes with low win rate after min_trades

### Key Features

- **Fee-aware profit margins**: Net profit calculation accounts for Polymarket's dynamic taker fees (3.15% at 50/50, ~0% near 0/1)
- **Hybrid order execution**: GTC maker orders (0% fee) for most trades, FOK taker orders only for tail-end urgency
- **Kelly criterion sizing**: Position size scales with confidence and edge, clamped to [min_size, max_size]
- **Spike detection**: Pre-filters small moves, triggers evaluation only on significant price changes or when delta exceeds fee+margin threshold
- **Trailing stop-loss**: Locks in profits as position moves favorably, with optional time decay near expiration
- **Batch order API**: TwoSided mode places both legs in a single API call for atomic execution
- **Cross-market correlation**: Leader coin spikes (BTC) generate signals for follower coins (ETH, SOL)
- **Performance tracking**: Per-mode statistics with optional auto-disable for underperforming modes

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
- `docs/research/polymarket-price-discovery.md` — how Polymarket discovers reference prices (CLOB midpoint, RTDS feeds, Chainlink/Binance oracles, confidence model)
- `docs/research/crypto-arb-reference-price.md` — crypto arb strategy reference price mechanics for 15-min markets (capture flow, confidence model, three trading modes, fee impact)
