# Polyrust

Autonomous Polymarket trading bot framework in Rust with event-driven architecture, trait-based strategy plugins, and single binary deployment.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        polyrust binary                          │
│                                                                 │
│  ┌──────────────┐   ┌──────────────┐   ┌────────────────────┐   │
│  │  Axum+HTMX   │   │   Engine     │   │  Turso (embedded)  │   │
│  │  Dashboard   │◄──│   Core       │──►│  - trades          │   │
│  │  (monitor)   │   │              │   │  - orders          │   │
│  └──────────────┘   │  ┌────────┐  │   │  - events          │   │
│                     │  │EventBus│  │   │  - pnl_snapshots   │   │
│                     │  └───┬────┘  │   └────────────────────┘   │
│                     │      │       │                            │
│        ┌────────────┼──────┼───────┼────────────┐               │
│        ▼            ▼      ▼       ▼            ▼               │
│  ┌───────────┐ ┌──────────┐ ┌──────────┐ ┌────────────┐         │
│  │ Crypto    │ │Dutch Book│ │ Position │ │  Balance   │         │
│  │ Arbitrage │ │Arbitrage │ │ State    │ │  State     │         │
│  │           │ │          │ │ (shared) │ │            │         │
│  └─────┬─────┘ └────┬─────┘ └──────────┘ └────────────┘         │
│        │             │                                          │
│        ▼             ▼                                          │
│  ┌─────────────────────────────────┐                            │
│  │      ExecutionBackend trait     │                            │
│  │  ┌───────────┐ ┌──────────────┐ │                            │
│  │  │   Live    │ │   Paper      │ │                            │
│  │  │ (rs-clob) │ │ (simulated)  │ │                            │
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

## Requirements

- Rust 1.85+ (edition 2024)

## Quickstart

```bash
git clone https://github.com/yourorg/polyrust.git
cd polyrust

# Build
cargo build --workspace

# Run in paper mode (default)
cargo run

# Run in backtest mode
cargo run -- --backtest

# Parameter grid search over strategy configs
cargo run -- --backtest-sweep

# API connectivity smoke tests (Gamma, Chainlink, CLOB auth, approvals)
cargo run -- --verify

# Run the example strategy
cargo run --example simple_strategy

# Run the backtest example
cargo run --example run_backtest

# Run tests
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings
```

The bot starts in paper trading mode by default with a $10,000 simulated balance. The monitoring dashboard is available at `http://127.0.0.1:3000`.

## Docker Deployment

### Quick Start

```bash
# Copy and customize config
cp config.example.toml config.toml
# Edit config.toml: set [dashboard] host = "0.0.0.0" for container access

# Build and start the bot
docker-compose up -d

# View logs
docker-compose logs -f polyrust

# Stop the bot
docker-compose down
```

### Configuration for Docker

1. **Copy config template**: `cp config.example.toml config.toml`
2. **Set dashboard host**: Update `[dashboard] host = "0.0.0.0"` (required for container access)
3. **Add secrets** (optional, for live trading):
   - Create `docker-compose.override.yml`:

```yaml
services:
  polyrust:
    environment:
      - POLY_PRIVATE_KEY=${POLY_PRIVATE_KEY}
      - POLY_SAFE_ADDRESS=${POLY_SAFE_ADDRESS}
      - POLY_BUILDER_API_KEY=${POLY_BUILDER_API_KEY}
      - POLY_BUILDER_API_SECRET=${POLY_BUILDER_API_SECRET}
      - POLY_BUILDER_API_PASSPHRASE=${POLY_BUILDER_API_PASSPHRASE}
```

4. **Access dashboard**: Navigate to `http://localhost:3000`

### Data Persistence

- Database stored in `./data/polyrust.db` (persisted across container restarts)
- To reset state: `rm -rf ./data && docker-compose restart`

### Production Deployment

Production runs on an ARM VPS via [Spot](https://github.com/umputun/spot) (`spot.yml`).

```bash
# Full deploy: build → push GHCR → pull on server → restart
spot -p spot.yml -t prod -n deploy

# Config only: decrypt secrets, copy files, restart
spot -p spot.yml -t prod -n update-config

# Just restart the container
spot -p spot.yml -t prod -n restart
```

Secrets are SOPS-encrypted (`secrets/prod.yaml`) and decrypted to `.env` at deploy time. Outbound traffic is routed through an SSH SOCKS5 tunnel via a sidecar container.

### Production Considerations

- **Secrets management**: SOPS + Age encryption for all credentials (see `secrets/` directory)
- **Live trading**: Set `[paper] enabled = false` in `config.toml` and provide credentials
- **Dashboard access**: Set `POLY_DASHBOARD_HOST=0.0.0.0` for access from outside the container
- **Monitoring**: `docker-compose logs -f polyrust` for real-time logs

## Configuration

Configuration is loaded from `config.example.toml` (copy to `config.toml`) with environment variable overrides:

| Setting | Env Variable | Default |
|---------|-------------|---------|
| Wallet private key | `POLY_PRIVATE_KEY` | — |
| Safe address | `POLY_SAFE_ADDRESS` | — |
| Builder API key | `POLY_BUILDER_API_KEY` | — |
| Builder API secret | `POLY_BUILDER_API_SECRET` | — |
| Builder API passphrase | `POLY_BUILDER_API_PASSPHRASE` | — |
| Dashboard host | `POLY_DASHBOARD_HOST` | 127.0.0.1 |
| Dashboard port | `POLY_DASHBOARD_PORT` | 3000 |
| Database path | `POLY_DB_PATH` | polyrust.db |
| RPC endpoints | `POLY_RPC_URLS` | ["https://polygon-rpc.com"] |
| Paper trading | `POLY_PAPER_TRADING` | true |
| Log level | `RUST_LOG` | info,polyrust=debug |

> **Note:** Paper mode defaults to `true` via `config/default.toml`. If the config file is missing, the Rust struct default is `false` (live mode). Always ensure the config file is present or set `POLY_PAPER_TRADING=true`.

> **Strategy Configuration:** Crypto arbitrage is configured via `[arbitrage]`, Dutch Book via `[dutch_book]` in `config.toml`. Both are disabled by default. See `config.example.toml` for all options.

## Strategy Plugin Example

Implement the `Strategy` trait to create a custom trading strategy:

```rust
use std::pin::Pin;
use std::future::Future;
use polyrust_core::prelude::*;

struct MyStrategy { /* state */ }

#[async_trait]
impl Strategy for MyStrategy {
    fn name(&self) -> &str { "my-strategy" }
    fn description(&self) -> &str { "My custom strategy" }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        match event {
            Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot)) => {
                if let Some(mid) = snapshot.mid_price() {
                    // Your trading logic here
                }
                Ok(vec![])
            }
            _ => Ok(vec![]),
        }
    }

    // Optional: expose a custom dashboard page at /strategy/my-strategy
    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        Some(self)
    }
}

// Optional: render strategy-specific HTML for the dashboard
impl DashboardViewProvider for MyStrategy {
    fn view_name(&self) -> &str { "my-strategy" }
    fn render_view(&self) -> Pin<Box<dyn Future<Output = Result<String>> + Send + '_>> {
        Box::pin(async { Ok("<div>Strategy-specific HTML here</div>".to_string()) })
    }
}
```

Register your strategy with the engine:

```rust
let engine = Engine::builder()
    .config(config)
    .strategy(MyStrategy::new())
    .execution(PaperBackend::new(dec!(10000), FillMode::Immediate))
    .build()
    .await?;

engine.run().await?;
```

## Crate Structure

- `polyrust-core` — engine, event bus, traits, shared state
- `polyrust-market` — CLOB orderbook + RTDS price feeds
- `polyrust-execution` — live + paper execution backends
- `polyrust-store` — Turso (embedded SQLite) persistence
- `polyrust-strategies` — reference strategy implementations
- `polyrust-backtest` — historical data pipeline + backtesting engine
- `polyrust-dashboard` — Axum + HTMX monitoring UI

## Included Strategies

### Crypto Arbitrage

Exploits mispricing in 15-minute Up/Down crypto markets using high-confidence tail-end trades (<2 min remaining, market >= 90% certainty). Configured via `[arbitrage]` in `config.toml`.

**Key features:**
- **Fee-aware profit margins** — accounts for Polymarket's dynamic taker fees (3.15% at 50/50, ~0% at extremes)
- **Hybrid order execution** — GTC maker orders (0% fee) for entries, FAK taker orders for fast exits
- **Kelly criterion sizing** — position size scales with confidence and edge
- **Position lifecycle state machine** — 3-state lifecycle (Healthy → ExitExecuting → Hedged) with 4-level trigger hierarchy: hard crash → dual-trigger + hysteresis → trailing stop → post-entry exit
- **Composite price stop-loss** — freshness-gated multi-source price (binance-futures > binance-spot > coinbase > chainlink), preventing stale single-source exits
- **Fast-path exit evaluation** — ExternalPrice events trigger exits 50-200ms ahead of CLOB updates using cached orderbook bids
- **FAK + GTC hybrid exits** — FAK for immediate partial fills, GTC residual at bid-tick for remainder. Proactive opposite-side hedge when set completion cost is within threshold
- **Performance tracking** — win rate and P&L tracking with optional auto-disable

**Dashboard** at `/strategy/crypto-arb`: live positions with P&L, open orders, active markets, spike events, and performance statistics. Real-time updates via SSE.

### Dutch Book Arbitrage

Market-neutral arbitrage: buys both YES and NO tokens when their combined ask price is below $1.00, locking in guaranteed profit upon resolution. Works across all active Polymarket markets. Configured via `[dutch_book]` in `config.toml`.

**Key features:**
- **Paired FOK execution** — buys both sides simultaneously via `PlaceBatchOrder`
- **Emergency unwind** — on partial fills (one side cancels), sells the filled side at a discount to avoid directional risk
- **Background market discovery** — `GammaScanner` periodically queries Gamma API for markets matching liquidity and resolution filters
- **Position tracking** — tracks paired positions from execution through resolution and redemption

**Dashboard** at `/strategy/dutch-book`.

Both strategies are disabled by default — set `enabled = true` in their respective config sections to activate. See `config.example.toml` for all options.

## Backtesting

Test strategies on historical data before deploying to paper or live trading.

### Quick Start

```bash
# Single run with config.toml settings
cargo run -- --backtest

# Parameter grid search over strategy configs
cargo run -- --backtest-sweep

# Minimal example
cargo run --example run_backtest
```

### Configuration

Add a `[backtest]` section to your `config.toml`:

```toml
[backtest]
strategy_name = "crypto-arb-tailend"      # Strategy to test
market_ids = []                            # Empty = auto-discover via Gamma API
start_date = "2025-01-01T00:00:00Z"       # Backtest period start (RFC3339)
end_date = "2025-01-31T23:59:59Z"         # Backtest period end (RFC3339)
initial_balance = 1000.00                  # Starting USDC balance
data_fidelity_secs = 60                    # Price granularity in seconds (60 = 1min)
data_db_path = "backtest_data.db"         # Historical data cache (persistent)
fetch_concurrency = 10                     # Markets fetched in parallel

[backtest.fees]
taker_fee_rate = 0.0315  # Match live fee model (3.15% at 50/50)
```

Environment variable overrides: `POLY_BACKTEST_START`, `POLY_BACKTEST_END`, `POLY_BACKTEST_INITIAL_BALANCE`, `POLY_BACKTEST_DATA_DB_PATH`, etc.

### How It Works

1. **Data Pipeline**: Fetches and caches historical trade data from the Goldsky orderbook subgraph. Smart caching prevents duplicate API calls across runs.
2. **Event Replay**: Deterministic chronological replay — synthesizes PriceChange events from trades at configured `data_fidelity_secs` granularity.
3. **Simulated Fills**: Immediate fills at historical trade prices with configurable fee model.
4. **Report**: P&L (total/realized/unrealized), win rate, max drawdown, Sharpe ratio, trade count.

### Parameter Sweep

Grid search over strategy parameters via `[backtest.sweep]` in `config.toml` with `ParamRange` (explicit values or min/max/step). Sorts results by metric (sharpe, pnl, win_rate) with sensitivity analysis and CSV/JSON export.

### Supported Strategies

- `crypto-arb-tailend` — crypto arbitrage tail-end trades
- `dutch-book` — Dutch Book arbitrage

Any `impl Strategy` works without modification — strategies receive the same event stream as in live/paper mode.

## License

MIT
