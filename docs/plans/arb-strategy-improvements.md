# Crypto Arb Strategy Improvements

## Overview
Upgrade the crypto arbitrage strategy to handle Polymarket's dynamic taker fee regime, improve capital efficiency, and add new alpha sources. The current strategy uses FOK-only taker orders with hardcoded margins — post-fee, many trades are unprofitable. This plan introduces fee awareness, hybrid maker/taker orders, adaptive sizing, spike detection, cross-market correlation, trailing stops, batch orders, and performance tracking.

Key benefits:
- **Fee elimination**: GTC maker orders pay $0 vs 3.15% taker fee at 50/50
- **Capital efficiency**: Kelly sizing scales with edge instead of fixed $5
- **New alpha**: spike detection and cross-coin correlation
- **Risk management**: trailing stops lock in profits
- **Adaptability**: auto-disable unprofitable modes

## Context
- Primary file: `crates/polyrust-strategies/src/crypto_arb.rs` (2,149 lines)
- Core types: `crates/polyrust-core/src/types.rs` (OrderRequest, OrderType: Gtc/Gtd/Fok)
- Actions: `crates/polyrust-core/src/actions.rs` (Action enum)
- Execution: `crates/polyrust-core/src/execution.rs` (ExecutionBackend trait)
- Live backend: `crates/polyrust-execution/src/live.rs` (SDK integration)
- Paper backend: `crates/polyrust-execution/src/paper.rs` (simulated fills)
- Engine: `crates/polyrust-core/src/engine.rs` (action routing)
- Research: `docs/research/arb-strategy-improvements.md`

Current state: FOK orders only, hardcoded 3¢/2¢ margins, fixed $5 sizing, dual-trigger stop-loss, no batch support, no spike detection, no cross-market signals, no performance tracking.

## Development Approach
- **Testing approach**: Regular (code first, then tests)
- Complete each task fully before moving to the next
- Make small, focused changes
- **CRITICAL: every task MUST include new/updated tests**
- **CRITICAL: all tests must pass before starting next task**
- **CRITICAL: update this plan file when scope changes during implementation**
- Run `cargo test --workspace && cargo clippy --workspace -- -D warnings` after each task

## Dependency Graph
```
T1 (Config restructure)
T2 (Fee-aware margins) ←── T1
T3 (Spike detection) ←── T1
T4 (Hybrid orders) ←── T2
T5 (Kelly sizing) ←── T2
T6 (Batch API) ←── T4
T7 (Cross-market) ←── T3
T8 (Trailing stops) ←── T1
T9 (Performance tracking) ←── T2
T10 (Verify + docs)
```

## Implementation Steps

### Task 1: Restructure ArbitrageConfig into sub-configs
- [x] Create `FeeConfig` struct with `taker_fee_rate: Decimal` (default 0.0315)
- [x] Create `SpikeConfig` struct with `threshold_pct: Decimal` (0.005), `window_secs: u64` (10), `history_size: usize` (50)
- [x] Create `OrderConfig` struct with `hybrid_mode: bool` (true), `limit_offset: Decimal` (0.01), `max_age_secs: u64` (30)
- [x] Create `SizingConfig` struct with `base_size: Decimal` (10), `kelly_multiplier: Decimal` (0.25), `min_size: Decimal` (2), `max_size: Decimal` (25), `use_kelly: bool` (true)
- [x] Create `StopLossConfig` struct — migrate existing `stop_loss_reversal_pct` and `stop_loss_min_drop` into it, add `trailing_enabled: bool` (true), `trailing_distance: Decimal` (0.03), `time_decay: bool` (true)
- [x] Create `CorrelationConfig` struct with `enabled: bool` (false), `min_spike_pct: Decimal` (0.01), `pairs: Vec<(String, Vec<String>)>` (default BTC→[ETH,SOL], ETH→[SOL])
- [x] Create `PerformanceConfig` struct with `min_trades: u64` (20), `min_win_rate: Decimal` (0.40), `window_size: usize` (50), `auto_disable: bool` (false)
- [x] Add all sub-configs to `ArbitrageConfig` with `#[serde(default)]`
- [x] Update `ArbitrageConfig::default()` to use sub-config defaults
- [x] Update all references to `self.config.stop_loss_reversal_pct` → `self.config.stop_loss.reversal_pct` etc.
- [x] Write tests for config deserialization with missing sub-configs (backward compat)
- [x] Write tests for config with explicit sub-config values
- [x] Run `cargo test --workspace && cargo clippy --workspace -- -D warnings`

### Task 2: Fee-aware profit margins
- [x] Add `taker_fee(price: Decimal, rate: Decimal) -> Decimal` helper: `2 * p * (1-p) * rate`
- [x] Add `net_profit_margin(entry_price: Decimal, fee_rate: Decimal, is_maker: bool) -> Decimal` helper: gross margin minus fee (maker fee = 0)
- [x] Add `estimated_fee: Decimal` and `net_margin: Decimal` fields to `ArbitrageOpportunity`
- [x] Update Tail-End mode (line ~532): compute `net_profit_margin` instead of `1 - ask`
- [x] Update Two-Sided mode (line ~551): subtract fees on both legs from combined margin
- [x] Update Confirmed mode (line ~586): use `net_profit_margin`, compare against `min_profit_margin`
- [x] Update dashboard `render_view()` to show fee and net margin columns
- [x] Write tests for `taker_fee` at prices 0.50, 0.80, 0.95 (verify against known values)
- [x] Write tests for `net_profit_margin` for maker vs taker orders
- [x] Write test: Confirmed mode at p=0.50 with 3¢ gross margin is filtered out (net < 0 after fee)
- [x] Write test: Tail-End at p=0.95 still passes (fee ~0.3¢, margin ~4.7¢)
- [x] Update existing margin assertion tests to use net margins
- [x] Run `cargo test --workspace && cargo clippy --workspace -- -D warnings`

### Task 3: Spike detection
- [x] Add `spike_events: VecDeque<SpikeEvent>` state to `CryptoArbitrageStrategy`
- [x] Define `SpikeEvent { coin, timestamp, change_pct, from_price, to_price, acted: bool }`
- [x] Implement `detect_spike(coin, current_price) -> Option<Decimal>` — check price change over `spike.window_secs` using `price_history`
- [x] In `on_crypto_price()`: before evaluating markets, compute spike. Add pre-filter: skip `evaluate_opportunity` unless `|price_delta| > taker_fee(mid_price) + min_margin` OR spike detected
- [x] Record spike events in `spike_events` (cap at `spike.history_size`)
- [x] Add spike events section to dashboard `render_view()`
- [x] Write tests for `detect_spike` returning Some for 1% move in 10s
- [x] Write tests for `detect_spike` returning None for 0.1% move
- [x] Write test: pre-filter skips evaluation for small moves (no spike, delta below fee+margin)
- [x] Write test: pre-filter allows evaluation when spike detected even with small absolute move
- [x] Run `cargo test --workspace && cargo clippy --workspace -- -D warnings`

### Task 4: Hybrid order mode — GTC default, FOK for tail-end
- [x] Extend `PendingOrder` with `order_type: OrderType`, `submitted_at: tokio::time::Instant`, `mode: ArbitrageMode`
- [x] Add `open_limit_orders: HashMap<OrderId, OpenLimitOrder>` state to strategy
- [x] Define `OpenLimitOrder { order_id, market_id, token_id, side, price, size, reference_price, coin, placed_at, mode }`
- [x] Modify order creation (line ~443-450): TailEnd → FOK at `best_ask`; Confirmed/TwoSided → GTC at `best_ask - limit_offset` (when `hybrid_mode` enabled)
- [x] Modify `on_order_placed()`: FOK success → create position (existing); GTC success → create `OpenLimitOrder`
- [x] Handle `OrderEvent::Filled` in `on_event()` → move from `open_limit_orders` to `positions`
- [x] Handle `OrderEvent::PartiallyFilled` → update size in `open_limit_orders`
- [x] Handle `OrderEvent::Cancelled` → remove from `open_limit_orders`
- [x] Implement `check_stale_limit_orders() -> Vec<Action>` — cancel orders older than `max_age_secs`
- [x] Call `check_stale_limit_orders()` on each event tick
- [x] Update position limit checks to count `open_limit_orders`
- [x] Update `net_profit_margin` calls: pass `is_maker=true` for GTC orders (0 fee)
- [x] Skip duplicate opportunities when limit order already open for market
- [x] Write test: Confirmed mode produces GTC order with price = `ask - 0.01`
- [x] Write test: TailEnd mode still produces FOK at ask price
- [x] Write test: stale order cancelled after `max_age_secs`
- [x] Write test: GTC order fill creates position correctly
- [x] Write test: duplicate detection skips market with open limit
- [x] Write test: `hybrid_mode=false` preserves all-FOK behavior
- [x] Run `cargo test --workspace && cargo clippy --workspace -- -D warnings`

### Task 5: Kelly criterion position sizing
- [x] Implement `kelly_position_size(confidence: Decimal, price: Decimal) -> Decimal`:
  - `payout = (1/price) - 1`
  - `kelly = (confidence * payout - (1 - confidence)) / payout`
  - `size = base_size * kelly * kelly_multiplier`, clamped to `[min_size, max_size]`
  - Return 0 for negative edge
- [x] Replace fixed sizing in order creation (line ~441): use Kelly for Confirmed/TailEnd, fixed for TwoSided
- [x] Add `kelly_fraction: Option<Decimal>` to `ArbitragePosition`
- [x] Update dashboard to show position sizes and Kelly fractions
- [x] Write test: `kelly_position_size(1.0, 0.95)` = large size (high confidence)
- [x] Write test: `kelly_position_size(0.5, 0.70)` = moderate size
- [x] Write test: `kelly_position_size(0.3, 0.60)` = 0 (negative edge, skip trade)
- [x] Write test: result clamped to `[min_size, max_size]`
- [x] Write test: TwoSided still uses fixed sizing
- [x] Run `cargo test --workspace && cargo clippy --workspace -- -D warnings`

### Task 6: Batch order API
- [ ] Add `PlaceBatchOrder(Vec<OrderRequest>)` variant to `Action` enum in `actions.rs`
- [ ] Add `place_batch_orders(&self, orders: &[OrderRequest]) -> Result<Vec<OrderResult>>` to `ExecutionBackend` trait with default sequential impl
- [ ] Update `Box<dyn ExecutionBackend>` impl to delegate `place_batch_orders`
- [ ] Handle `PlaceBatchOrder` in engine's `execute_action` — call `place_batch_orders`, publish individual `OrderEvent::Placed` per result
- [ ] Override `place_batch_orders` in `PaperBackend` — sequential processing with atomic balance updates
- [ ] Override `place_batch_orders` in `LiveBackend` — check if SDK has batch endpoint, otherwise sequential fallback
- [ ] Update TwoSided mode in strategy: emit single `PlaceBatchOrder(vec![up_order, down_order])` instead of two `PlaceOrder`
- [ ] Write test: batch with 2 orders produces 2 OrderResults
- [ ] Write test: paper backend processes batch with correct balance deduction
- [ ] Write test: engine routes PlaceBatchOrder and publishes correct events
- [ ] Write test: TwoSided mode emits PlaceBatchOrder
- [ ] Run `cargo test --workspace && cargo clippy --workspace -- -D warnings`

### Task 7: Cross-market correlation
- [ ] Add `CrossCorrelated { leader: String }` variant to `ArbitrageMode` enum
- [ ] In `on_crypto_price()`: when spike detected for leader coin, find active markets for follower coins from `correlation.pairs`
- [ ] Generate opportunities for followers: confidence = `leader_confidence * 0.7` (correlation discount)
- [ ] Skip if follower market ask already moved away from 0.50 (market caught up)
- [ ] Add correlation signals to dashboard
- [ ] Write test: BTC 2% spike with active ETH market generates ETH Up opportunity
- [ ] Write test: no signal when `correlation.enabled = false`
- [ ] Write test: no signal when follower market already moved (ask > 0.60 or < 0.40)
- [ ] Write test: confidence properly discounted by 0.7x factor
- [ ] Run `cargo test --workspace && cargo clippy --workspace -- -D warnings`

### Task 8: Trailing stop-loss
- [ ] Add `peak_bid: Decimal` field to `ArbitragePosition`, initialize to `entry_price`
- [ ] On `OrderbookUpdate` events: update `peak_bid = max(peak_bid, current_bid)` for matching positions
- [ ] In `check_stop_loss()`: add trailing stop check after existing dual-trigger logic
  - If `peak_bid > entry_price` and `peak_bid - current_bid > trailing_distance`, trigger sell
  - Time decay: `effective_distance = trailing_distance * (time_remaining / 900)` when `time_decay` enabled
- [ ] Keep existing dual-trigger logic unchanged (trailing is additive)
- [ ] Write test: trailing stop triggers when bid drops 3¢ from peak (peak=0.70, bid=0.67)
- [ ] Write test: trailing stop does NOT trigger when position is underwater (peak == entry)
- [ ] Write test: time decay tightens distance near expiry (30s remaining → tiny distance)
- [ ] Write test: `trailing_enabled=false` preserves existing behavior only
- [ ] Run `cargo test --workspace && cargo clippy --workspace -- -D warnings`

### Task 9: Performance tracking
- [ ] Define `ModeStats { entered: u64, won: u64, lost: u64, total_pnl: Decimal, recent_pnl: VecDeque<Decimal> }` with `win_rate()` and `avg_pnl()` methods
- [ ] Add `mode_stats: HashMap<ArbitrageMode, ModeStats>` state to strategy
- [ ] Add `mode: ArbitrageMode` field to `ArbitragePosition`
- [ ] On position close (market expiry or stop-loss): compute P&L including fees, update `ModeStats`
  - Winner: `pnl = (1.0 - entry_price) * size - estimated_fee`
  - Loser: `pnl = -entry_price * size - estimated_fee`
  - Stop-loss: `pnl = (exit_price - entry_price) * size - estimated_fee`
- [ ] In `evaluate_opportunity()`: check auto-disable — skip mode if `trades >= min_trades && win_rate < min_win_rate` (when `auto_disable` enabled)
- [ ] Add performance stats section to dashboard (per-mode table: trades, win rate, P&L, status)
- [ ] Write test: `ModeStats::win_rate()` correct with 7 wins, 3 losses
- [ ] Write test: auto-disable triggers after `min_trades` with low win rate
- [ ] Write test: auto-disable does NOT trigger before `min_trades`
- [ ] Write test: P&L calculation for wins, losses, stop-loss exits with fee deduction
- [ ] Run `cargo test --workspace && cargo clippy --workspace -- -D warnings`

### Task 10: Verify acceptance criteria
- [ ] Verify fee-aware margins filter unprofitable trades at mid-range prices
- [ ] Verify hybrid mode: Confirmed/TwoSided use GTC, TailEnd uses FOK
- [ ] Verify Kelly sizing scales with confidence
- [ ] Verify spike detection filters small moves, allows large moves
- [ ] Verify trailing stops lock in profits
- [ ] Verify batch orders work for TwoSided mode
- [ ] Verify cross-market correlation generates signals (when enabled)
- [ ] Verify performance tracking and auto-disable (when enabled)
- [ ] Run full test suite: `cargo test --workspace`
- [ ] Run linter: `cargo clippy --workspace -- -D warnings`
- [ ] Paper trading dry-run: `POLY_PAPER_TRADING=true cargo run` — check dashboard

### Task 11: [Final] Update documentation
- [ ] Update `CLAUDE.md` with new config sub-structures
- [ ] Update `docs/research/arb-strategy-improvements.md` with implementation status
- [ ] Add inline doc comments to new public types and functions

## Technical Details

### Fee Formula
```
taker_fee(p) = 2 * p * (1 - p) * 0.0315
```
| Price | Fee/share | Round-trip |
|-------|-----------|------------|
| 0.50  | 1.575¢    | 3.15¢      |
| 0.80  | 1.01¢     | 2.02¢      |
| 0.95  | 0.30¢     | 0.60¢      |

Maker fee = $0 (GTC orders). Exit at resolution ($1) has ~0% fee.

### Kelly Formula
```
payout = (1/price) - 1
kelly = (confidence * payout - (1 - confidence)) / payout
position = base_size * kelly * multiplier  // clamped to [min, max]
```

### Hybrid Order Flow
```
TailEnd mode → FOK at best_ask (speed matters, fee ~0% at 90%+)
Confirmed mode → GTC at best_ask - 0.01 (maker, $0 fee)
TwoSided mode → GTC batch at best_ask - 0.01 per leg (maker, $0 fee)
```

### Spike Detection
```
delta = (current - baseline) / baseline  // baseline = price N seconds ago
if |delta| >= threshold → spike event
pre-filter: skip evaluation unless |delta| > fee(mid) + min_margin OR spike
```

## Post-Completion

**Manual verification:**
- Paper trading session observing all modes fire correctly
- Verify dashboard shows fees, spikes, Kelly sizes, trailing stops, perf stats
- Monitor GTC order lifecycle: placement → fill/cancel → position/cleanup

**Future work (P3):**
- Market-making mode (4th strategy mode)
- Multi-source price oracle (Coinbase, aggregated VWAP)
- Historical performance persistence to database
