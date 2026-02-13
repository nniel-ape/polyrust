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

# CLI modes
cargo run -- --backtest          # Run backtest with config.toml settings
cargo run -- --backtest-sweep    # Parameter grid search (requires [backtest.sweep] config)
cargo run -- --verify            # API connectivity smoke tests (Gamma, Chainlink, CLOB auth, approvals)
cargo run --example run_backtest # Minimal backtest example

# Docker deployment (local)
docker-compose up -d             # Build and start bot in background
docker-compose logs -f polyrust  # View real-time logs
docker-compose down              # Stop and remove containers
docker-compose restart           # Restart after config changes
docker-compose build --no-cache  # Rebuild from scratch

# Spot deployment (VPS at 31.172.70.91)
spot -p spot.yml -t prod -n deploy         # Full deploy: build → push GHCR → pull on server → restart
spot -p spot.yml -t prod -n update-config  # Config only: decrypt secrets, copy files, restart
spot -p spot.yml -t prod -n restart        # Just restart the container
spot -p spot.yml -t prod --dry             # Dry run (no changes)
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
  ├── polyrust-dashboard (Axum + HTMX monitoring UI)
  └── polyrust-backtest (historical data + backtesting engine)

src/main.rs → wires all crates into a single binary
```

### Core Traits (Plugin System)

- **`Strategy`** — `on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>>` — receives events, returns actions (place/cancel orders, emit signals)
- **`ExecutionBackend`** — abstracts order execution: `LiveBackend` (real CLOB API) vs `PaperBackend` (simulated fills). Also: `CtfRedeemer` (position redemption via Safe MultiSend), gasless `Relayer`
- **`MarketDataFeed`** — market data producers: `ClobFeed` (WebSocket orderbooks), `PriceFeed` (RTDS crypto prices), `BinanceFeed` (spot + futures), `CoinbaseFeed` (ticker), `DiscoveryFeed` (Gamma market discovery)
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

Real-time updates use SSE: strategies emit `"dashboard-update"` signals, the SSE handler re-renders the view, and HTMX swaps the content in the browser. For dashboard-only views (no event processing), use `DashboardStrategyWrapper` in `src/main.rs` to register a view provider as a no-op strategy. See the crypto arbitrage strategy's dashboard for a reference implementation.

## Domain Concepts

- **Token IDs**: Each market has 2 outcomes (Up/Down or Yes/No), each is an ERC-1155 token
- **Prices**: Probabilities in [0, 1] range — use `rust_decimal::Decimal`, never floats
- **USDC**: 6 decimal places; store as `Decimal`, persist as TEXT in SQLite
- **Tick sizes**: Typically 0.01 (2 decimal price, 2 decimal size). USDC amount precision is order-type-dependent:
  - GTC/GTD: `price_decimals + size_decimals` (e.g., 4 for tick=0.01), uses **tick-rounded** price
  - FOK: `price_decimals` only (e.g., 2 for tick=0.01), uses **raw price** (round UP for BUY, DOWN for SELL)
  - FOK must use raw price because tick-rounding drops effective bid below ask for sub-tick prices (e.g. 0.997→0.99)
- **neg_risk**: Boolean on orders — false for 15-minute markets (most common)

## Configuration

Copy `config.example.toml` → `config.toml` and customize. Polymarket credentials are **env-only** — copy `.env.example` → `.env` and fill in values (never committed to git):
- `POLY_PRIVATE_KEY`, `POLY_SAFE_ADDRESS` — wallet credentials
- `POLY_BUILDER_API_KEY`, `POLY_BUILDER_API_SECRET`, `POLY_BUILDER_API_PASSPHRASE` — builder API
- `POLY_RPC_URLS` — comma-separated Polygon RPC endpoints for Chainlink oracle queries
- `POLY_USE_RELAYER`, `POLY_RELAYER_URL` — gasless relayer settings

Other runtime overrides via `POLY_*` env vars: `POLY_DASHBOARD_HOST`, `POLY_DASHBOARD_PORT`, `POLY_DB_PATH`, `POLY_PAPER_TRADING`.

Paper mode: `[paper] enabled = true` or `POLY_PAPER_TRADING=true`
Docker deployment: Set `POLY_DASHBOARD_HOST=0.0.0.0` in `docker-compose.yml` to allow access from host machine.

Strategy configuration: Add `[arbitrage]` section (with `enabled = true` and nested `[arbitrage.tailend]`) to `config.toml`. The strategy is disabled by default. See `config.example.toml` for the complete reference.

Backtest configuration: Add `[backtest]` section to `config.toml` or use env overrides (`POLY_BACKTEST_START`, `POLY_BACKTEST_END`, etc.). Backtesting evaluates strategies on historical data without live/paper trading. See `config.example.toml` for the complete reference.

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

Ported from Python (`../polymarket-trading-bot/`). Exploits mispricing in 15-minute Up/Down crypto markets using high-confidence tail-end trades (<2 min remaining, market >= 90%).

### Strategy Configuration

Configured via `[arbitrage]` in `config.toml`. Directory structure at `crates/polyrust-strategies/src/crypto_arb/` with shared state through `Arc<CryptoArbBase>`:

- `TailEndStrategy` — high-confidence trades near expiration
- `CryptoArbDashboard` — unified dashboard view

Strategy is disabled by default; set `enabled = true` in `[arbitrage]` to activate.

`ArbitrageConfig` has core settings (coins, max_positions, min_profit_margin) plus `TailEndConfig` and shared sub-configs: `FeeConfig` (taker fee model), `SpikeConfig` (price spike detection), `OrderConfig` (GTC/FOK), `SizingConfig` (Kelly criterion), `StopLossConfig` (dual-trigger + trailing), `PerformanceConfig` (tracking + auto-disable). See `config.rs` for field details.

### Key Features

- **Fee-aware profit margins**: Net profit calculation accounts for Polymarket's dynamic taker fees (3.15% at 50/50, ~0% near 0/1)
- **Hybrid order execution**: GTC maker orders (0% fee) for most trades, FOK taker orders only for tail-end urgency. FOK has stricter USDC precision (see `rounding.rs`)
- **Kelly criterion sizing**: Position size scales with confidence and edge, clamped to [min_size, max_size]
- **Spike detection**: Pre-filters small moves, triggers evaluation only on significant price changes or when delta exceeds fee+margin threshold
- **Trailing stop-loss**: Locks in profits as position moves favorably, with optional time decay near expiration
- **Performance tracking**: Statistics with optional auto-disable when underperforming

## Polymarket API Endpoints

- CLOB API: `https://clob.polymarket.com`
- Gamma API: `https://gamma-api.polymarket.com` (market discovery, metadata)
- Data API: `https://data-api.polymarket.com` (positions, balances)
- WebSocket: `wss://ws-subscriptions-clob.polymarket.com` (orderbook streams)

## Backtesting Framework

The backtesting system (`crates/polyrust-backtest`) allows strategy evaluation on historical data before live/paper trading. It consists of two subsystems:

1. **Historical data pipeline** — fetch/cache trade data from Goldsky subgraph, market metadata from Gamma API
2. **Backtest engine** — deterministic event replay through strategies with simulated fills

### Architecture

Two isolated databases:
- **`backtest_data.db`** (persistent) — historical data cache (prices, trades, markets, fetch log). Reused across runs.
- **`:memory:` Store** (ephemeral) — receives simulated trades using existing live schema. Disposed after run; report extracted first.

### Data Sources

- **Gamma API** (`/markets`) — market discovery and metadata
- **Goldsky activity subgraph** — unlimited historical trade data via GraphQL

All trade data comes from the Goldsky subgraph (no CLOB API dependency). DataFetcher checks `data_fetch_log` before fetching to avoid duplicate API calls. PriceChange events are synthesized from trade data by the engine at configured `data_fidelity_secs` granularity.

### Backtest Engine

Synchronous deterministic event replay:
1. Load cached trade data from `backtest_data.db` for configured market_ids and date range
2. Synthesize PriceChange events from trades at `data_fidelity_secs` granularity (bucketed by time window)
3. Sort all events chronologically (trades + synthetic prices + lifecycle events)
4. For each event: advance simulated clock, update market data, call `strategy.on_event()`, execute actions with immediate fills at current market price
5. After replay: finalize results, generate `BacktestReport` with P&L metrics

Fill mode: Immediate only (historical orderbook depth not available from Polymarket APIs). Fills simulate at historical trade price with configurable fee model.

### Configuration

Add `[backtest]` section to `config.toml`:

```toml
[backtest]
strategy_name = "crypto-arb-tailend"
market_ids = []                         # Empty = auto-discover via Gamma API
start_date = "2025-01-01T00:00:00Z"     # Backtest window start (RFC3339)
end_date = "2025-01-31T23:59:59Z"       # Backtest window end (RFC3339)
initial_balance = 1000.00               # Starting USDC balance
data_fidelity_secs = 60                 # Price granularity in seconds (60 = 1min, 300 = 5min)
data_db_path = "backtest_data.db"       # Persistent historical data cache
fetch_concurrency = 10                  # Markets fetched in parallel (default 10)
offline = false                         # true = use only cached data, no network
market_duration_secs = 900              # Filter for 15-min markets

[backtest.fees]
taker_fee_rate = 0.0315  # 3.15% at 50/50 probability
```

Environment variable overrides: `POLY_BACKTEST_START`, `POLY_BACKTEST_END`, `POLY_BACKTEST_INITIAL_BALANCE`, `POLY_BACKTEST_DATA_DB_PATH`, `POLY_BACKTEST_FETCH_CONCURRENCY`, `POLY_BACKTEST_OFFLINE`, etc.

### Running Backtests

```fish
cargo run -- --backtest          # Single run with config.toml settings
cargo run -- --backtest-sweep    # Parameter grid search (requires [backtest.sweep])
cargo run --example run_backtest # Minimal example
```

Backtest report includes: total P&L, realized/unrealized P&L, win rate, max drawdown, Sharpe ratio, trade count, start/end balance, duration.

### Parameter Sweep

The sweep system (`crates/polyrust-backtest/src/sweep/`) runs a grid search over strategy parameters. Configure via `[backtest.sweep]` in `config.toml` with `ParamRange` (explicit values or min/max/step). `SweepRunner` orchestrates parallel runs, `SweepReport` provides sorting by metric (sharpe, pnl, win_rate), sensitivity analysis, and CSV/JSON export.

### Strategy Compatibility

Any `impl Strategy` works in backtest without modification — strategies receive the same `Event` stream and return `Vec<Action>` as in live/paper mode. The engine handles the rest. Note: backtests downgrade `min_reference_quality` to `ReferenceQualityLevel::Current` since historical quality tracking uses wall-clock timestamps.

## Deployment

Production runs on an ARM VPS (`31.172.70.91`) via [Spot](https://github.com/umputun/spot). Config: `spot.yml`.

**Pipeline**: build Docker image locally (native arm64) → push to `ghcr.io/nniel-ape/polyrust` → server pulls and restarts via `docker compose`.

**Three tasks**: `deploy` (full pipeline), `update-config` (secrets + config + restart, no rebuild), `restart` (just restart).

**Secrets**: SOPS-encrypted `secrets/prod.yaml` → decrypted to `.env.deploy` (dotenv format, keys uppercased) → copied as `.env` on server. The `docker-compose.yml` reads `.env` via `env_file`.

**Image reference**: `docker-compose.yml` defaults to `ghcr.io/nniel-ape/polyrust:latest` — no `IMAGE_NAME` override needed.

**Prerequisites**: `docker login ghcr.io` on both local machine (push) and server (pull). SSH key: `~/.ssh/id_deploy`, user: `deploy`.

**Spot gotchas**: `copy` paths must be absolute (`/home/deploy/polyrust/...`), not `~/...`. Use `mkdir: true` on the first copy entry to auto-create the directory.

### Network Tunnel (SSH SOCKS proxy)

All outbound traffic from the polyrust container is routed through a proxy VPS (`79.132.137.31`) via an SSH SOCKS5 tunnel. This is transparent — no code changes needed, WebSocket and HTTPS traffic both go through it.

**Architecture**: `tunnel` sidecar container (Alpine + autossh + redsocks) shares network namespace with polyrust via `network_mode: "service:tunnel"`. iptables redirects all TCP → redsocks → SOCKS5 → SSH tunnel → proxy VPS → internet.

**Files**: `tunnel/Dockerfile`, `tunnel/redsocks.conf`, `tunnel/entrypoint.sh`

**SSH key**: SOPS-encrypted at `secrets/tunnel-key.yaml` (YAML wrapper, extract with `sops -d --extract '["key"]'`). Public key is deployed to `root@79.132.137.31`.

**Tunnel env vars** (in `.env` or docker-compose): `TUNNEL_SSH_USER` (default: root), `TUNNEL_SSH_PORT` (default: 22), `SSH_HOST` (hardcoded: 79.132.137.31).

**Verify tunnel**: `docker exec polyrust wget -qO- https://ifconfig.me` should return `79.132.137.31`.

## Danger Zones & Approvals

- When adding a new workspace crate, update `Dockerfile` in 3 places: manifest `COPY`, dummy `RUN` source, and `find crates` touch
- Never push Docker images with `config.toml` baked in — it's `.dockerignore`d and mounted at runtime
- `cargo build --release --locked` in Docker requires `Cargo.lock` committed and up-to-date
- USDC rounding in `rounding.rs` must branch on `OrderType` — FOK uses **raw price** (`size * price`), GTC uses **tick-rounded price** (`size * rounded_price`). FOK also has stricter decimal precision (`price_decimals` only vs `price_decimals + size_decimals`). See rs-clob-client issue #114

## Design Documents

- `docs/brainstorms/polyrust-trading-framework.md` — goals, architecture, traits
- `docs/plans/polyrust-framework-implementation.md` — detailed implementation guide
- `docs/plans/polyrust-checklist.md` — milestone task checklist with validation commands
- `docs/plans/strategy-dashboard-views.md` — strategy dashboard views design
- `docs/plans/backtesting-framework.md` — backtesting framework design
- `docs/plans/arb-strategy-improvements.md` — arbitrage strategy improvement plan
- `docs/research/polymarket-price-discovery.md` — reference price discovery (CLOB midpoint, RTDS feeds, Chainlink/Binance oracles)
- `docs/research/crypto-arb-reference-price.md` — crypto arb reference price mechanics for 15-min markets
- `docs/research/arb-strategy-improvements.md` — arbitrage strategy improvement research
- `docs/research/polymarket-modern-strategies.md` — modern Polymarket trading strategies research
