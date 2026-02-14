# Dutch Book Arbitrage Strategy

## Overview

Port the **pure Dutch Book arbitrage** strategy from [rarb](../../../rarb) (Python) into polyrust as a new strategy module. Dutch Book arbitrage exploits mispricing across prediction market outcomes: when the combined ask price of YES + NO tokens is less than $1.00, buying both sides locks in a guaranteed profit upon market resolution.

**Key difference from existing crypto-arb**: This is a **market-neutral** strategy — it always buys BOTH sides of a market simultaneously. There is zero directional risk. Profit is realized only when the market resolves and positions are redeemed.

**Problem solved**: Risk-free profit extraction from Polymarket inefficiencies across all active markets (not limited to 15-min crypto markets).

**Integration**: New strategy module in `polyrust-strategies` crate alongside existing `crypto_arb/`, registered in `main.rs` with its own `[dutch_book]` config section.

## Context (from discovery)

**Files/components involved:**
- `crates/polyrust-strategies/src/dutch_book/` — new module (6-7 files)
- `crates/polyrust-strategies/src/lib.rs` — add module export
- `src/main.rs` — register strategy + dashboard + config loading
- `config.example.toml` — add `[dutch_book]` section
- `crates/polyrust-core/src/types.rs` — reference for MarketInfo, OrderbookSnapshot
- `crates/polyrust-core/src/actions.rs` — reference for PlaceBatchOrder, SubscribeMarket, RedeemPosition
- `crates/polyrust-core/src/events.rs` — reference for OrderbookUpdate, OrderEvent::Filled/Cancelled

**Related patterns found:**
- Existing crypto_arb strategy provides the template: config.rs, base.rs, types.rs, strategy.rs, dashboard.rs, tests.rs
- Strategy subscribes to markets via `Action::SubscribeMarket(MarketInfo)` — CLOB feed handles both tokens automatically
- Orderbook data accessed via `ctx.market_data.read().await.orderbooks.get(token_id)`
- Paired orders via `Action::PlaceBatchOrder(vec![order_yes, order_no])`

**Dependencies identified:**
- `reqwest` (already in workspace) — for Gamma API market discovery
- `polyrust-core` — Strategy trait, events, actions, types
- `polyrust-market` — reference for Gamma API patterns (discovery_feed.rs)

## Development Approach
- **Testing approach**: Regular (code first, then tests)
- Complete each task fully before moving to the next
- Make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
- **CRITICAL: all tests must pass before starting next task** — no exceptions
- **CRITICAL: update this plan file when scope changes during implementation**
- Run tests after each change
- Maintain backward compatibility with existing crypto-arb strategy

## Testing Strategy
- **Unit tests**: Required for every task — arbitrage detection math, config validation, position tracking state machine
- **Integration tests**: Strategy + mock events end-to-end (PlaceBatchOrder emitted on opportunity)
- **Backtest compatibility**: Strategy must work unchanged in backtest mode (no special backtest code)

## Progress Tracking
- Mark completed items with `[x]` immediately when done
- Add newly discovered tasks with ➕ prefix
- Document issues/blockers with ⚠️ prefix
- Update plan if implementation deviates from original scope
- Keep plan in sync with actual work done

## Architecture

```
crates/polyrust-strategies/src/dutch_book/
├── mod.rs           # Module exports, pub use
├── config.rs        # DutchBookConfig + nested sub-configs
├── types.rs         # PairedPosition, ArbitrageOpportunity, ExecutionState
├── scanner.rs       # GammaScanner — market discovery via Gamma API
├── analyzer.rs      # Arbitrage detection: track both orderbooks, detect opportunities
├── strategy.rs      # DutchBookStrategy — implements Strategy trait, orchestrates everything
├── dashboard.rs     # DutchBookDashboard — implements DashboardViewProvider
└── tests.rs         # Comprehensive test suite
```

### Data Flow

```
GammaScanner (background task, spawned in on_start)
  → populates pending_subscriptions queue
  → strategy drains queue on next on_event → emits SubscribeMarket actions

OrderbookUpdate events (from CLOB WebSocket feed)
  → ArbitrageAnalyzer checks both tokens' orderbooks
  → if combined_ask < max_combined_cost AND profit > threshold AND liquidity sufficient
  → emit PlaceBatchOrder with paired FOK orders (BUY YES + BUY NO)

OrderEvent::Filled / Cancelled
  → track paired execution state
  → both filled → record paired position, await resolution
  → one filled, one cancelled → emergency unwind (sell filled side at 97%)
  → both cancelled → no action, opportunity missed

MarketExpired / resolution detected
  → emit RedeemPosition action for resolved positions
  → engine's CtfRedeemer handles on-chain redemption
```

### Emergency Unwind (Critical Safety Mechanism)

When only one side of a paired order fills:
1. Detect partial fill: track both order IDs, wait for both fill/cancel events
2. Wait 5 seconds for on-chain settlement
3. Place GTC SELL order at 97% of buy price for the filled side
4. Track unwind until complete or timed out
5. Log loss for monitoring

This prevents holding unhedged directional risk.

## Implementation Steps

### Task 1: Configuration and domain types
- [x] Create `crates/polyrust-strategies/src/dutch_book/mod.rs` with module declarations
- [x] Create `crates/polyrust-strategies/src/dutch_book/config.rs` with `DutchBookConfig`:
  - `enabled: bool` (default false)
  - `max_combined_cost: Decimal` (default 0.99 — fee buffer)
  - `min_profit_threshold: Decimal` (default 0.005 = 0.5%)
  - `max_position_size: Decimal` (default 100 USDC per side)
  - `min_liquidity_usd: Decimal` (default 10000)
  - `max_days_until_resolution: u64` (default 7)
  - `scan_interval_secs: u64` (default 600 = 10 minutes)
  - `max_concurrent_positions: usize` (default 10)
  - `unwind_discount: Decimal` (default 0.03 = sell at 97% on emergency unwind)
  - `unwind_settle_secs: u64` (default 5)
  - Implement `Default`, `Deserialize`, and `validate()` method
- [x] Create `crates/polyrust-strategies/src/dutch_book/types.rs`:
  - `ArbitrageOpportunity { market_id, yes_ask, no_ask, combined_cost, profit_pct, max_size, detected_at }`
  - `PairedOrder { market_id, yes_order_id, no_order_id, size, submitted_at }`
  - `PairedPosition { market_id, yes_entry_price, no_entry_price, size, combined_cost, expected_profit, opened_at }`
  - `ExecutionState` enum: `AwaitingFills { yes_filled, no_filled }`, `BothFilled`, `PartialFill { filled_side, filled_order_id }`, `Unwinding { sell_order_id }`, `Complete`
  - `MarketEntry { market_id, token_a, token_b, neg_risk, end_date, liquidity }` — tracked market info
- [x] Add `pub mod dutch_book;` to `crates/polyrust-strategies/src/lib.rs`
- [x] Write tests for config validation (invalid thresholds, edge cases)
- [x] Write tests for type construction and state transitions
- [x] Run `cargo test -p polyrust-strategies` — must pass before next task

### Task 2: Market scanner (Gamma API discovery)
- [x] Create `crates/polyrust-strategies/src/dutch_book/scanner.rs` with `GammaScanner` struct
- [x] Implement `scan_markets()` async method:
  - Query `GET https://gamma-api.polymarket.com/markets` with filters: `active=true`, `closed=false`
  - Paginate through results (Gamma uses offset/limit)
  - Filter by: liquidity >= `min_liquidity_usd`, end_date within `max_days_until_resolution`
  - Parse response into `Vec<MarketInfo>` (reuse polyrust-core type)
  - Deduplicate against already-subscribed markets
- [x] Implement background scanning: `start_scanner()` spawns a tokio task that:
  - Runs `scan_markets()` periodically (every `scan_interval_secs`)
  - Pushes new markets to `Arc<Mutex<Vec<MarketInfo>>>` pending queue
  - Logs scan results (markets found, new markets, errors)
- [x] Write tests for market filtering logic (mock Gamma responses)
- [x] Write tests for deduplication (already-known markets skipped)
- [x] Run `cargo test -p polyrust-strategies` — must pass before next task

### Task 3: Arbitrage analyzer (opportunity detection)
- [x] Create `crates/polyrust-strategies/src/dutch_book/analyzer.rs` with `ArbitrageAnalyzer` struct
- [x] Maintain `tracked_markets: HashMap<MarketId, MarketEntry>` with token_a ↔ token_b mapping
- [x] Implement `add_market(&mut self, market: &MarketInfo)` — register both tokens for tracking
- [x] Implement `remove_market(&mut self, market_id: &str)` — unregister market
- [x] Implement `check_arbitrage(&self, token_id: &str, orderbooks: &HashMap<TokenId, OrderbookSnapshot>, config: &DutchBookConfig) -> Option<ArbitrageOpportunity>`:
  - Look up which market this token belongs to
  - Get both token orderbooks from the shared state
  - Extract best ask price and size from each side
  - Reject if either side has no asks
  - Calculate `combined_cost = yes_best_ask + no_best_ask`
  - Reject if `combined_cost >= max_combined_cost`
  - Calculate `profit_pct = (1 - combined_cost) / combined_cost`
  - Reject if `profit_pct < min_profit_threshold`
  - Calculate `max_size = min(yes_ask_size, no_ask_size, max_position_size)`
  - Reject if insufficient liquidity (size too small for minimum order)
  - Return `ArbitrageOpportunity` with all details
- [x] Write tests for arbitrage detection (opportunity exists, no opportunity, edge cases)
- [x] Write tests for profit calculation accuracy with `dec!()` literals
- [x] Write tests for size limiting (liquidity constraints, max position cap)
- [x] Run `cargo test -p polyrust-strategies` — must pass before next task

### Task 4: Strategy core (event handling + order placement)
- [ ] Create `crates/polyrust-strategies/src/dutch_book/strategy.rs` with `DutchBookStrategy`:
  - Fields: `config: DutchBookConfig`, `analyzer: ArbitrageAnalyzer`, `pending_subscriptions: Arc<Mutex<Vec<MarketInfo>>>`, `active_executions: HashMap<MarketId, PairedOrder>`, `open_positions: HashMap<MarketId, PairedPosition>`, `scanner_handle: Option<JoinHandle<()>>`
- [ ] Implement `Strategy::on_start`: spawn background scanner task via `GammaScanner::start_scanner()`
- [ ] Implement `Strategy::on_event` event routing:
  - **Any event**: drain `pending_subscriptions` queue → emit `SubscribeMarket` actions for new markets
  - **OrderbookUpdate**: call `analyzer.check_arbitrage()` → if opportunity found AND not already executing on this market AND under max concurrent positions → emit `PlaceBatchOrder` with paired FOK BUY orders, track in `active_executions`
  - **OrderEvent::Filled**: update execution state for the order's market, check if both sides filled
  - **OrderEvent::Cancelled / Rejected**: update execution state, trigger emergency unwind if other side filled
  - **MarketExpired**: remove from analyzer, check if positions need redemption → emit `RedeemPosition`
- [ ] Implement paired order creation: build two `OrderRequest` (FOK, BUY, best ask price, computed size) — one for each token
- [ ] Implement position limit check: reject new opportunities if `open_positions.len() >= max_concurrent_positions`
- [ ] Write tests for event routing (correct actions emitted for each event type)
- [ ] Write tests for position limit enforcement
- [ ] Write tests for paired order construction (correct prices, sizes, order type)
- [ ] Run `cargo test -p polyrust-strategies` — must pass before next task

### Task 5: Paired execution tracking and emergency unwind
- [ ] Implement execution state machine in strategy:
  - On PlaceBatchOrder: create `PairedOrder` in `active_executions` with `ExecutionState::AwaitingFills`
  - On Filled for YES token: mark `yes_filled = true`, check if both filled
  - On Filled for NO token: mark `no_filled = true`, check if both filled
  - On both filled: move from `active_executions` to `open_positions` as `PairedPosition`, log success
  - On one cancelled + other filled: transition to `PartialFill` state
- [ ] Implement emergency unwind logic:
  - On PartialFill detection: log warning with details
  - Calculate sell price: `filled_price * (1 - unwind_discount)` (default 97%)
  - Emit `PlaceOrder` (GTC, SELL, discounted price) for the filled side
  - Track unwind order ID in `ExecutionState::Unwinding`
  - On unwind fill: remove from `active_executions`, log realized loss
  - On unwind cancel/reject: log error, keep tracking (manual intervention may be needed)
- [ ] Implement order-to-market mapping: maintain `HashMap<OrderId, MarketId>` to route fill/cancel events
- [ ] Write tests for full execution lifecycle: both fill → PairedPosition
- [ ] Write tests for partial fill → emergency unwind → sell filled
- [ ] Write tests for both cancelled → clean removal
- [ ] Write tests for unwind price calculation
- [ ] Run `cargo test -p polyrust-strategies` — must pass before next task

### Task 6: Dashboard view
- [ ] Create `crates/polyrust-strategies/src/dutch_book/dashboard.rs` with `DutchBookDashboard`
- [ ] Implement `DashboardViewProvider` trait:
  - `view_name()` → `"dutch-book"`
  - `render_view()` → async HTML rendering
- [ ] Render sections:
  - **Summary**: total markets monitored, active positions, total P&L, opportunities detected
  - **Active Positions**: table with market_id, combined_cost, expected_profit, size, age
  - **Recent Opportunities**: last N detected opportunities (filled or missed)
  - **Execution Status**: any active/unwinding executions
- [ ] Store dashboard state in `Arc<RwLock<DutchBookState>>` shared between strategy and dashboard
- [ ] Write tests for HTML rendering (non-empty output, correct section headers)
- [ ] Run `cargo test -p polyrust-strategies` — must pass before next task

### Task 7: Integration (main.rs, config, registration)
- [ ] Add `DutchBookConfig` to `ConfigWrapper` in `src/main.rs` with `#[serde(default)]`
- [ ] Add `[dutch_book]` section to `config.example.toml` with all parameters documented
- [ ] Register strategy in `main.rs`:
  - Create shared state (`Arc<RwLock<DutchBookState>>`)
  - Instantiate `DutchBookStrategy` if `dutch_book_config.enabled`
  - Register with `builder.strategy(...)`
  - Register `DutchBookDashboard` via `DashboardStrategyWrapper`
- [ ] Add strategy name `"dutch-book"` to backtest strategy matching in `main.rs` backtest section
- [ ] Write integration test: create strategy with default config, send mock events, verify actions
- [ ] Run `cargo test --workspace` — must pass before next task
- [ ] Run `cargo clippy --workspace -- -D warnings` — must pass

### Task 8: Verify acceptance criteria
- [ ] Verify all requirements from Overview are implemented:
  - [ ] Market discovery via Gamma API with configurable filters
  - [ ] Real-time arbitrage detection from orderbook updates
  - [ ] Paired FOK order execution (buy YES + NO simultaneously)
  - [ ] Partial fill detection + emergency unwind
  - [ ] Position tracking (paired positions awaiting resolution)
  - [ ] Redemption via existing CtfRedeemer (RedeemPosition action)
  - [ ] Dashboard with opportunities, positions, and execution status
  - [ ] Configuration via `[dutch_book]` TOML section
- [ ] Verify edge cases:
  - [ ] No crash on empty orderbooks
  - [ ] No crash on markets with one-sided liquidity only
  - [ ] Position limit enforced
  - [ ] Emergency unwind fires on partial fills
- [ ] Run full test suite: `cargo test --workspace`
- [ ] Run linter: `cargo clippy --workspace -- -D warnings`
- [ ] Verify `cargo build --release` succeeds

### Task 9: [Final] Update documentation
- [ ] Update `config.example.toml` with complete `[dutch_book]` documentation
- [ ] Update `CLAUDE.md` with Dutch Book strategy section (brief, like existing crypto-arb section)
- [ ] Verify backtest compatibility: strategy works with `cargo run -- --backtest` (if test data available)

## Technical Details

### Arbitrage Formula
```
combined_cost = best_ask_yes + best_ask_no
profit_pct = (1.0 - combined_cost) / combined_cost
max_size = min(yes_ask_liquidity, no_ask_liquidity, config.max_position_size)

// Example: YES @ 0.48, NO @ 0.49
// combined = 0.97, profit = 3.09%, guaranteed on resolution
```

### Order Parameters
- **Type**: FOK (Fill-or-Kill) — must fill immediately or cancel
- **Side**: BUY (always buying both outcomes)
- **Price**: Best ask from orderbook for each token
- **Size**: `max_size` calculated above
- **neg_risk**: From MarketInfo (varies per market)

### USDC Precision (FOK)
- FOK orders use `price_decimals` only (e.g., 2 for tick=0.01)
- Amount = `size * raw_price` (NOT tick-rounded)
- See `crates/polyrust-strategies/src/crypto_arb/base.rs` rounding logic for reference

### Gamma API Query
```
GET https://gamma-api.polymarket.com/markets?active=true&closed=false&limit=100&offset=0
```
Filter client-side: `liquidity >= min_liquidity_usd`, `end_date <= now + max_days`

### Position Lifecycle
```
Opportunity Detected → PlaceBatchOrder (FOK YES + FOK NO)
  ├─ Both Fill → PairedPosition (awaiting resolution)
  │    └─ Market Resolves → RedeemPosition → USDC profit
  ├─ Both Cancel → Opportunity missed (no risk)
  └─ One Fill, One Cancel → Emergency Unwind
       └─ Sell filled side at 97% → Accept small loss → Safe
```

## Post-Completion

**Manual verification:**
- Test in paper mode: `POLY_PAPER_TRADING=true cargo run` with `[dutch_book] enabled = true`
- Verify WebSocket subscriptions work for discovered markets
- Verify dashboard renders at `/strategy/dutch-book`
- Monitor execution latency (detection → order submission)

**Performance considerations:**
- Gamma API polling rate: 10 min default (gentle on API)
- Orderbook check: O(1) per update (HashMap lookups)
- May need to tune `max_concurrent_positions` based on available balance
- FOK orders have ~300ms matching window — latency-sensitive

**Future enhancements (NOT in scope):**
- Fee-aware profit calculation (Polymarket's dynamic taker fee model)
- Multi-level orderbook depth analysis (not just best ask)
- Historical arbitrage frequency analysis for market selection
- Slack/webhook notifications for executions
