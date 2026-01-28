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

> **Strategy Configuration:** The crypto arbitrage strategy supports additional configuration options (fees, sizing, stop-loss, cross-correlation, performance tracking). Currently, these must be configured programmatically by modifying `src/main.rs` to pass a custom `ArbitrageConfig` instead of `Default::default()`. See [Reference Strategy](#reference-strategy-crypto-arbitrage) section below for available options. Future versions will support TOML-based strategy configuration.

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

## Reference Strategy: Crypto Arbitrage

The included crypto arbitrage strategy exploits mispricing in 15-minute Up/Down crypto markets with four trading modes:

1. **Tail-End** — <2 min remaining, market >= 90% certainty (highest confidence, uses FOK taker orders for speed)
2. **Two-Sided** — Both outcomes priced below combined threshold (guaranteed profit, places both legs atomically)
3. **Confirmed** — Standard directional trades with dynamic confidence model (uses GTC maker orders when hybrid mode enabled)
4. **Cross-Correlated** — Leader coin spike (BTC) triggers follower signals (ETH, SOL) with discounted confidence

### Key Features

- **Fee-aware profit margins** — Net profit calculation accounts for Polymarket's dynamic taker fees (3.15% at 50/50, ~0% at extremes)
- **Hybrid order execution** — GTC maker orders (0% fee) for most trades, FOK taker orders only for tail-end urgency
- **Kelly criterion sizing** — Position size scales with confidence and edge, clamped to configured min/max
- **Spike detection** — Pre-filters small moves, triggers evaluation only on significant price changes
- **Trailing stop-loss** — Locks in profits as position moves favorably, with optional time decay near expiration
- **Performance tracking** — Per-mode win rate and P&L tracking with optional auto-disable for underperforming modes

### Configuration

Modify `ArbitrageConfig` in `src/main.rs` to customize behavior. Available sub-configs:

- **FeeConfig** — Taker fee model (default 3.15%)
- **SpikeConfig** — Spike detection thresholds and history
- **OrderConfig** — Hybrid maker/taker mode, limit order offset, max age
- **SizingConfig** — Kelly criterion parameters, min/max position size
- **StopLossConfig** — Dual-trigger + trailing stops, time decay
- **CorrelationConfig** — Cross-market correlation pairs (BTC → ETH/SOL)
- **PerformanceConfig** — Per-mode tracking, auto-disable thresholds

See `CLAUDE.md` for detailed configuration documentation and the complete list of available parameters.

### Dashboard

Strategy-specific dashboard available at `http://127.0.0.1:3000/strategy/crypto-arb` shows:
- Live positions with P&L and peak bid tracking
- Open limit orders (GTC maker orders)
- Active markets with reference prices and spreads
- Recent spike events for cross-correlation
- Per-mode performance statistics (win rate, total P&L, recent trades)

## License

MIT
