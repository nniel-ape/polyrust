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
│  │ Strategy A│ │Strategy B│ │ Position │ │  Balance   │         │
│  │ (crypto   │ │(user's)  │ │ State    │ │  State     │         │
│  │  arb)     │ │          │ │ (shared) │ │            │         │
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

### Production Considerations

- **Secrets management**: Use Docker secrets or external secrets manager (not environment variables in production)
- **Live trading**: Set `[paper] enabled = false` in `config.toml` and provide credentials
- **Dashboard access**: For external access (outside container), set `POLY_DASHBOARD_HOST=0.0.0.0` in `docker-compose.yml`
- **Monitoring**: `docker-compose logs -f polyrust` for real-time logs
- **Health checks**: Dashboard health endpoint (implementation pending)

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

> **Strategy Configuration:** The crypto arbitrage strategy is configured via the `[arbitrage]` section in `config.toml`. See `config.example.toml` for all available options and the [Reference Strategy](#reference-strategy-crypto-arbitrage) section below.

## Strategy Plugin Example

Implement the `Strategy` trait to create a custom trading strategy:

```rust
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

## Reference Strategy: Crypto Arbitrage

The included crypto arbitrage strategy exploits mispricing in 15-minute Up/Down crypto markets using high-confidence tail-end trades (<2 min remaining, market >= 90% certainty, uses FOK taker orders for speed).

### Key Features

- **Fee-aware profit margins** — Net profit calculation accounts for Polymarket's dynamic taker fees (3.15% at 50/50, ~0% at extremes)
- **Hybrid order execution** — GTC maker orders (0% fee) for most trades, FOK taker orders only for tail-end urgency
- **Kelly criterion sizing** — Position size scales with confidence and edge, clamped to configured min/max
- **Spike detection** — Pre-filters small moves, triggers evaluation only on significant price changes
- **Position lifecycle state machine** — Per-position 6-state lifecycle (Healthy -> DeferredExit -> ExitExecuting -> ResidualRisk -> RecoveryProbe -> Cooldown) with 4-level trigger hierarchy for stop-loss decisions
- **Composite price stop-loss** — All stop-loss decisions use freshness-gated composite price from multiple sources
- **Execution ladder** — Depth-capped exit clips with geometric reduction, 2-second GTC refresh cycle, and recovery logic
- **Performance tracking** — Win rate and P&L tracking with optional auto-disable for underperforming trades

### Configuration

Configure via `[arbitrage]` section in `config.toml`. Available sub-configs:

- **FeeConfig** — Taker fee model (default 3.15%)
- **SpikeConfig** — Spike detection thresholds and history
- **OrderConfig** — Hybrid maker/taker mode, limit order offset, max age
- **SizingConfig** — Kelly criterion parameters, min/max position size
- **StopLossConfig** — Lifecycle state machine + dual-trigger + trailing stops + hard crash detection + recovery
- **PerformanceConfig** — Tracking, auto-disable thresholds

See `config.example.toml` for the complete reference and `CLAUDE.md` for detailed documentation.

### Dashboard

Strategy-specific dashboard available at `http://127.0.0.1:3000/strategy/crypto-arb` shows:
- Live positions with P&L and peak bid tracking
- Open limit orders (GTC maker orders)
- Active markets with reference prices and spreads
- Recent spike events for cross-correlation
- Performance statistics (win rate, total P&L, recent trades)

## Backtesting

Test your strategies on historical data before deploying to paper or live trading.

### Quick Start

```fish
# Configure backtest settings in config.toml
# Edit [backtest] section: set strategy_name, date range, markets

# Run backtest
cargo run -- --backtest

# Or use the minimal example
cargo run --example run_backtest
```

### Configuration

Add a `[backtest]` section to your `config.toml`:

```toml
[backtest]
strategy_name = "crypto-arb-tailend"      # Strategy to test
market_ids = []                            # Empty = auto-discover from coins
start_date = "2025-01-01T00:00:00Z"       # Backtest period start (RFC3339)
end_date = "2025-01-31T23:59:59Z"         # Backtest period end (RFC3339)
initial_balance = 1000.00                  # Starting USDC balance
data_fidelity_secs = 60                    # Price granularity in seconds (60 = 1min)
data_db_path = "backtest_data.db"         # Historical data cache (persistent)

[backtest.fees]
taker_fee_rate = 0.0315  # Match live fee model (3.15% at 50/50)
```

Environment variable overrides: `POLY_BACKTEST_START`, `POLY_BACKTEST_END`, `POLY_BACKTEST_INITIAL_BALANCE`, `POLY_BACKTEST_DATA_DB_PATH`, etc.

### How It Works

1. **Data Pipeline**: Fetches and caches historical market data from Polymarket APIs and Goldsky subgraphs
   - CLOB API: Last ~7 days (high-fidelity price timeseries)
   - Goldsky subgraph: Unlimited history (on-chain trade data)
   - Smart caching prevents duplicate API calls
2. **Event Replay**: Deterministically replays historical events through your strategy
3. **Simulated Fills**: Immediate fills at historical trade prices with configurable fee model
4. **Performance Report**: Comprehensive metrics including P&L, win rate, max drawdown, Sharpe ratio

### Report Metrics

- Total/realized/unrealized P&L
- Win rate and trade count
- Maximum drawdown percentage
- Sharpe ratio (annualized risk-adjusted returns)
- Start/end balance
- Backtest duration

### Supported Strategies

Currently supported crypto arbitrage strategy:
- `crypto-arb-tailend` - High-confidence tail-end trades (<2 min remaining)

Any strategy implementing the `Strategy` trait works without modification. See `examples/run_backtest.rs` for custom strategy usage.

## License

MIT
