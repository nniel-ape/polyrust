# Polyrust

Autonomous Polymarket trading bot framework in Rust with event-driven architecture, trait-based strategy plugins, and single binary deployment.

## Architecture

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
│  │ Strategy A │ │Strategy B│ │ Position │ │  Balance   │       │
│  │ (crypto    │ │(user's)  │ │ State    │ │  State     │       │
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

# Run the example strategy
cargo run --example simple_strategy

# Run tests
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings
```

The bot starts in paper trading mode by default with a $10,000 simulated balance. The monitoring dashboard is available at `http://127.0.0.1:3000`.

## Configuration

Configuration is loaded from `config/default.toml` with environment variable overrides:

| Setting | Env Variable | Default |
|---------|-------------|---------|
| Wallet private key | `POLY_PRIVATE_KEY` | — |
| Safe address | `POLY_SAFE_ADDRESS` | — |
| Builder API key | `POLY_BUILDER_API_KEY` | — |
| Builder API secret | `POLY_BUILDER_API_SECRET` | — |
| Builder API passphrase | `POLY_BUILDER_API_PASSPHRASE` | — |
| Dashboard port | `POLY_DASHBOARD_PORT` | 3000 |
| Database path | `POLY_DB_PATH` | polyrust.db |
| Paper trading | `POLY_PAPER_TRADING` | true |
| Log level | `RUST_LOG` | info,polyrust=debug |

> **Note:** Paper mode defaults to `true` via `config/default.toml`. If the config file is missing, the Rust struct default is `false` (live mode). Always ensure the config file is present or set `POLY_PAPER_TRADING=true`.

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
- `polyrust-dashboard` — Axum + HTMX monitoring UI

## License

MIT
