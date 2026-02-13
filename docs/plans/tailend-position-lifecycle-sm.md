# TailEnd Position Lifecycle State Machine v2

## Overview
Refactor TailEnd position management into a per-position state machine that is risk-first, supports partial exits, controlled hedge/re-entry profit recovery, and is fully driven by explicit state transitions.

**Problems solved:**
- Stop-loss using stale single-source price while entry uses fresh composite (lost $10.05)
- Trailing trigger requiring mathematically impossible values (peak_bid >= 1.04 when max is 1.00)
- Contradictory defaults making time_decay inert and post-entry exit unreachable (lost $0.44 upside)
- PnL bugs: taker fee on maker entries, trigger bid used instead of actual fill price
- No freshness gating on stop-loss data
- Position state scattered across ~12 HashMaps with no transition enforcement

**Key design decisions:**
- 6-state lifecycle: Healthy → DeferredExit → ExitExecuting → ResidualRisk → RecoveryProbe → Cooldown
- Per-position trailing headroom: `effective_arm_distance = min(config, price_cap - entry_price)`
- 2-second cancel/replace cadence for stop-loss GTC exits ("short limit orders")
- Recovery always enabled (opposite-side set completion + alpha, same-side re-entry)
- Composite price for all stop-loss decisions with freshness gating
- Trigger hierarchy: hard crash → dual+hysteresis → trailing → post-entry deferred

## Context (from discovery)
- **Files involved**: config.rs, types.rs, base.rs, tailend.rs, orderbook.rs, tests.rs, main.rs
- **Related patterns**: `composite_fair_price()` (base.rs:709), `SizingConfig::validate()` (config.rs:252), `ask_depth_up_to()` (orderbook.rs), `StopLossRejectionKind` (base.rs:42), `handle_stop_loss_rejection()` (base.rs:1933), `reduce_or_remove_position_by_token()` (base.rs:1619)
- **Dependencies**: `ArbitragePosition` used by tailend.rs, twosided.rs, base.rs; `StopLossConfig` read by both strategies; orderbook types in polyrust-core

## Development Approach
- **Testing approach**: Regular (code first, then tests per task)
- Complete each task fully before moving to the next
- Make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
- **CRITICAL: all tests must pass before starting next task** — no exceptions
- **CRITICAL: update this plan file when scope changes during implementation**
- Run `cargo test --workspace` after each change
- Run `cargo clippy --workspace -- -D warnings` at task boundaries
- Maintain backward compatibility — TwoSided strategy unaffected

## Testing Strategy
- **Unit tests**: required for every task (config validation, state transitions, trigger logic, PnL math, depth sizing)
- **Integration tests**: replay incident scenarios (BTC Down at 0.23%, ETH Up trailing false exit)
- **Regression**: existing stop-loss and reconciliation tests adapted to new state machine

## Progress Tracking
- Mark completed items with `[x]` immediately when done
- Add newly discovered tasks with ➕ prefix
- Document issues/blockers with ⚠️ prefix
- Update plan if implementation deviates from original scope

## Implementation Steps

### Task 1: Add new StopLossConfig lifecycle fields
- [x] Add hard crash fields to `StopLossConfig` in `crates/polyrust-strategies/src/crypto_arb/config.rs`: `hard_drop_abs: Decimal` (0.08), `hard_reversal_pct: Decimal` (0.006), `hard_window_ms: i64` (2000)
- [x] Add freshness gating fields: `sl_max_book_age_ms: i64` (1200), `sl_max_external_age_ms: i64` (1500), `sl_min_sources: usize` (2), `sl_max_dispersion_bps: Decimal` (50)
- [x] Add hysteresis field: `dual_trigger_consecutive_ticks: usize` (2)
- [x] Add short-lived limit fields: `short_limit_refresh_secs: u64` (2), `short_limit_tick_offset: u32` (1)
- [x] Add trailing arming field: `trailing_arm_distance: Decimal` (0.015)
- [x] Add execution ladder fields: `exit_depth_cap_factor: Decimal` (0.80), `max_exit_retries: u32` (5)
- [x] Add recovery fields: `recovery_enabled: bool` (true), `recovery_max_set_cost: Decimal` (1.01), `recovery_max_extra_frac: Decimal` (0.15), `reentry_confirm_ticks: usize` (2), `reentry_cooldown_secs: i64` (8)
- [x] Update `Default` impl for `StopLossConfig` with all new fields
- [x] Update `config.example.toml` with new fields and comments
- [x] Write test: deserialize config with new fields from TOML
- [x] Write test: default values are sane (non-zero, positive where required)
- [x] Run tests — must pass before task 2

### Task 2: Fix contradictory defaults
- [x] Change `trailing_distance` default: 0.03 → 0.05 in `config.rs` StopLossConfig Default
- [x] Change `trailing_min_distance` default: 0.05 → 0.015
- [x] Change `min_sell_delay_secs` default: 15 → 10 in TailEndConfig Default
- [x] Change `post_entry_window_secs` default: 10 → 20
- [x] Change `min_strike_distance_pct` default: 0.0012 → 0.005
- [x] Change `reversal_pct` default: 0.005 → 0.003
- [x] Change `min_remaining_secs` default: 0 → 45
- [x] Change `gtc_stop_loss_max_age_secs` default: 10 → 2
- [x] Update any hardcoded test values that relied on old defaults
- [x] Write test: verify floor < base for trailing (`trailing_min_distance < trailing_distance`)
- [x] Write test: verify post-entry window > sell delay
- [x] Run tests — must pass before task 3

### Task 3: Add config validation methods
- [x] Add `StopLossConfig::validate() -> Result<(), String>` — check: `trailing_min_distance <= trailing_distance`, `short_limit_refresh_secs >= 1`, cooldown schedules non-empty, `exit_depth_cap_factor` in (0, 1]
- [x] Add `ArbitrageConfig::validate() -> Result<(), String>` — cross-config checks: `post_entry_window_secs > min_sell_delay_secs`, warn if dead zone `(reversal_pct - min_strike_distance_pct) > 0.003`
- [x] Call `config.validate()` from TailEndStrategy constructor — fail fast at startup
- [x] Wire validation call in `src/main.rs` strategy registration (or from strategy `new()`)
- [x] Write test: `trailing_min_distance > trailing_distance` → error with clear message
- [x] Write test: `post_entry_window_secs <= min_sell_delay_secs` → error with clear message
- [x] Write test: valid config passes validation
- [x] Write test: dead zone warning is emitted (check tracing output or return type)
- [x] Run tests — must pass before task 4

### Task 4: Define lifecycle types and StopLossTriggerKind
- [x] Add `StopLossTriggerKind` enum to `crates/polyrust-strategies/src/crypto_arb/types.rs`: `HardCrash`, `DualTrigger`, `TrailingStop`, `PostEntryExit` — each with relevant fields
- [x] Add `PositionLifecycleState` enum: `Healthy`, `DeferredExit { trigger, armed_at }`, `ExitExecuting { order_id, order_type, exit_price, submitted_at }`, `ResidualRisk { remaining_size, retry_count, last_attempt, use_gtc_next }`, `RecoveryProbe { recovery_order_id, probe_side, submitted_at }`, `Cooldown { until }`
- [x] Add `PositionLifecycle` struct: `state`, `dual_trigger_ticks: usize`, `trailing_unarmable: bool`, `last_composite: Option<CompositePriceResult>`, `last_composite_at: Option<DateTime<Utc>>`, `pending_exit_order_id: Option<OrderId>`, `transition_log: Vec<(DateTime<Utc>, String)>`
- [x] Implement `PositionLifecycle::new()` → initializes in `Healthy` state
- [x] Implement `PositionLifecycle::transition(&mut self, new_state, reason)` → validates transition legality, appends to log (capped at 50)
- [x] Add `ExitOrderMeta` struct for tracking exit order provenance (token_id, order_type, lifecycle state reference)
- [x] Derive `Debug, Clone` on all new types; derive `PartialEq` on StopLossTriggerKind and PositionLifecycleState
- [x] Write test: all valid transitions succeed (Healthy→DeferredExit, Healthy→ExitExecuting, DeferredExit→ExitExecuting, DeferredExit→Healthy, ExitExecuting→ResidualRisk, ResidualRisk→ExitExecuting, ResidualRisk→RecoveryProbe, RecoveryProbe→ExitExecuting, RecoveryProbe→Cooldown, Cooldown→Healthy)
- [x] Write test: invalid transitions return error (e.g. Healthy→ResidualRisk, Cooldown→ExitExecuting)
- [x] Write test: transition log caps at 50 entries
- [x] Run tests — must pass before task 5

### Task 5: Extend ArbitragePosition with fee/order metadata
- [x] Add `entry_order_type: OrderType` field to `ArbitragePosition` in `types.rs`
- [x] Add `entry_fee_per_share: Decimal` field (actual fee: 0 for GTC, computed for FOK)
- [x] Add `realized_pnl: Decimal` field (accumulated P&L from partial exits, starts at 0)
- [x] Update `ArbitragePosition::from_limit_order()` to set `entry_order_type = Gtc`, `entry_fee_per_share = Decimal::ZERO`
- [x] Update all other ArbitragePosition constructors (FOK path in tailend.ts, synthetic fill in base.rs) to set correct `entry_order_type` and compute `entry_fee_per_share`
- [x] Update any struct literals that construct ArbitragePosition to include new fields
- [x] Write test: GTC entry → `entry_fee_per_share == 0`
- [x] Write test: FOK entry → `entry_fee_per_share == taker_fee(price, rate)`
- [x] Run tests — must pass before task 6

### Task 6: Add orderbook bid depth helpers
- [x] Add `best_bid_depth(&self) -> Option<Decimal>` to `OrderbookSnapshot` in `crates/polyrust-market/src/orderbook.rs` (or `crates/polyrust-core/src/types.rs` wherever OrderbookSnapshot lives)
- [x] Add `bid_depth_down_to(&self, min_price: Decimal) -> Decimal` — sum bid sizes where `price >= min_price` (mirrors existing `ask_depth_up_to`)
- [x] Write test: empty bids → `best_bid_depth() == None`, `bid_depth_down_to(..) == 0`
- [x] Write test: single bid level → returns that level's size
- [x] Write test: multiple bid levels → sums correctly down to min_price threshold
- [x] Run tests — must pass before task 7

### Task 7: Fix PnL entry fee bug
- [x] In `tailend.rs` `on_order_filled` (~line 949), replace `pos.estimated_fee * size` with conditional: `if pos.entry_order_type == Gtc { ZERO } else { taker_fee(pos.entry_price, fee_rate) } * size`
- [x] Same fix in GTC stop-loss fill path (if exists separately)
- [x] Same fix in `on_market_expired` PnL calculation in `base.rs` if it uses `estimated_fee`
- [x] Write test: PnL for GTC entry + FOK exit → entry fee = 0, exit fee = taker
- [x] Write test: PnL for GTC entry + GTC exit → both fees = 0
- [x] Run tests — must pass before task 8

### Task 8: Fix PnL exit price bug
- [x] In `tailend.rs` `on_order_filled` FOK stop-loss path (~line 941), change `let exit_price = sl_info.exit_price` to `let exit_price = price` (use actual CLOB fill price)
- [x] Verify GTC stop-loss path also uses actual fill price (it likely already does since it gets `price` from Filled event)
- [x] Write test: FOK stop-loss fill at 0.93 when trigger bid was 0.92 → PnL uses 0.93
- [x] Write test: trigger bid and fill price same → no difference (sanity)
- [x] Run tests — must pass before task 9

### Task 9: Add lifecycle store to CryptoArbBase
- [ ] Add `position_lifecycle: RwLock<HashMap<TokenId, PositionLifecycle>>` to `CryptoArbBase`
- [ ] Add `exit_orders_by_id: RwLock<HashMap<OrderId, ExitOrderMeta>>` to `CryptoArbBase`
- [ ] Initialize both as empty in `CryptoArbBase::new()`
- [ ] Add helper: `ensure_lifecycle(&self, token_id) -> PositionLifecycle` — creates Healthy state if not exists (for existing positions during migration)
- [ ] Add helper: `remove_lifecycle(&self, token_id)` — cleanup on position close
- [ ] When a position is created (GTC fill, FOK fill, synthetic fill), also create its `PositionLifecycle` entry in Healthy state
- [ ] When a position is fully removed, also remove its lifecycle entry
- [ ] Write test: position creation creates lifecycle in Healthy state
- [ ] Write test: position removal cleans up lifecycle
- [ ] Run tests — must pass before task 10

### Task 10: Implement composite price caching for stop-loss
- [ ] Add `sl_composite_cache: RwLock<HashMap<String, (CompositePriceResult, DateTime<Utc>)>>` to `CryptoArbBase` (keyed by coin)
- [ ] On every `ExternalPrice` event in `handle_external_price` (or wherever it's processed), recompute composite for that coin and update cache
- [ ] Also update `PositionLifecycle.last_composite` and `last_composite_at` for positions with matching coin
- [ ] Add helper: `get_sl_composite(&self, coin: &str, max_age_ms: i64) -> Option<CompositePriceResult>` — returns cached composite if fresh enough
- [ ] Add helper: `get_sl_single_fresh(&self, coin: &str, max_age_ms: i64) -> Option<Decimal>` — fallback: freshest single source within age limit
- [ ] Write test: fresh composite returned when within age limit
- [ ] Write test: stale composite returns None
- [ ] Write test: single fresh source returned when composite unavailable
- [ ] Run tests — must pass before task 11

### Task 11: Implement trigger hierarchy (evaluate_triggers)
- [ ] Add `evaluate_triggers()` method on `PositionLifecycle` (or as free function) accepting: position, orderbook snapshot, composite price, config, now timestamp
- [ ] Implement freshness gate: check orderbook age <= `sl_max_book_age_ms`, external age <= `sl_max_external_age_ms`; if stale, only allow hard-crash trigger
- [ ] Implement Level 1 — Hard Crash: bid drop from entry >= `hard_drop_abs` OR external reversal >= `hard_reversal_pct`; requires 1 fresh source + fresh book; bypasses hysteresis
- [ ] Implement Level 2 — Dual Trigger + Hysteresis: crypto_reversed AND market_dropped must both hold for `dual_trigger_consecutive_ticks` consecutive evaluations; reset counter if either clears; requires fresh composite + fresh book
- [ ] Implement Level 3 — Trailing Stop with headroom fix: compute `price_cap = 1 - tick_size`, `headroom = max(0, price_cap - entry_price)`, `effective_arm_distance = min(trailing_arm_distance, headroom)`; if `effective_arm_distance < tick_size` mark `trailing_unarmable = true` and skip; otherwise arm when `peak_bid >= entry + effective_arm_distance`, trigger when `peak_bid - current_bid >= effective_distance` (with time decay, floor <= base)
- [ ] Implement Level 4 — Post-Entry Deferred: if within sell delay window and adverse move detected, return PostEntryExit trigger (caller handles DeferredExit state)
- [ ] Return first (highest priority) trigger that fires, or None
- [ ] Write test: hard crash fires when bid drops 0.08 from entry
- [ ] Write test: hard crash fires on external reversal 0.6%
- [ ] Write test: hard crash works with stale composite (only needs 1 fresh source)
- [ ] Write test: dual trigger requires 2 consecutive ticks (first tick returns None, second returns trigger)
- [ ] Write test: dual trigger resets counter when condition clears
- [ ] Write test: trailing at entry 0.99 → `trailing_unarmable = true`, trailing never fires, but hard/dual still work
- [ ] Write test: trailing at entry 0.90 → `effective_arm_distance = min(0.015, 0.09) = 0.015`, arms at 0.915, triggers on drop from peak
- [ ] Write test: trailing with time decay — effective distance decreases as time remaining decreases
- [ ] Write test: post-entry deferred triggers within sell delay window
- [ ] Write test: stale orderbook suppresses all triggers except hard-crash with fresh external
- [ ] Run tests — must pass before task 12

### Task 12: Implement depth-capped exit clip sizing
- [ ] Add `compute_exit_clip(remaining: Decimal, bid_depth: Decimal, cap_factor: Decimal, min_size: Decimal) -> Decimal` as helper function
- [ ] Logic: `capped = min(bid_depth * cap_factor, remaining)`; if `capped < min_size` return `Decimal::ZERO` (dust)
- [ ] Write test: remaining=10, bid_depth=20, cap=0.8 → clip=10 (remaining is limit)
- [ ] Write test: remaining=10, bid_depth=5, cap=0.8 → clip=4 (depth is limit)
- [ ] Write test: remaining=10, bid_depth=0.5, cap=0.8 → clip=0 (below min_size)
- [ ] Write test: remaining=0.001 → clip=0 (dust)
- [ ] Run tests — must pass before task 13

### Task 13: Replace check_stop_loss with lifecycle-driven evaluation in tailend.rs
- [ ] In `handle_orderbook_update` (tailend.rs), for each open position: get/create lifecycle, get cached composite, call `evaluate_triggers()`
- [ ] If trigger fires and position is sellable (past `min_sell_delay_secs`): transition lifecycle to `ExitExecuting`, build depth-capped FOK sell order, emit PlaceOrder action
- [ ] If trigger fires but NOT yet sellable: transition lifecycle to `DeferredExit` (arm the trigger for later)
- [ ] If position is in `DeferredExit` and now sellable: re-check trigger; if still valid transition to `ExitExecuting` and sell; if cleared transition back to `Healthy`
- [ ] If no trigger: ensure lifecycle is `Healthy`, update `dual_trigger_ticks` counter (reset if conditions cleared)
- [ ] Continue updating `peak_bid` on every orderbook update (existing logic)
- [ ] Store pending exit order in `exit_orders_by_id` for fill routing
- [ ] Remove old `check_stop_loss()` call path from tailend evaluation loop
- [ ] Write test: orderbook update with trigger condition → lifecycle transitions to ExitExecuting
- [ ] Write test: orderbook update during sell delay → lifecycle transitions to DeferredExit
- [ ] Write test: deferred exit re-checks and fires when sellable
- [ ] Write test: deferred exit clears when condition resolves
- [ ] Run tests — must pass before task 14

### Task 14: Implement execution ladder (FOK → short-lived GTC refresh)
- [ ] When ExitExecuting FOK is rejected for liquidity: transition to `ResidualRisk { use_gtc_next: true, retry_count: 1, remaining_size }`
- [ ] When in `ResidualRisk` and cooldown elapsed: build next exit order — if `use_gtc_next`, place GTC at `bid - short_limit_tick_offset * tick_size`
- [ ] Implement 2s GTC refresh: on each orderbook update, if GTC exit order age > `short_limit_refresh_secs` (2s), cancel and re-place at current `bid - offset`
- [ ] Track GTC exit orders in `exit_orders_by_id` for fill/cancel routing
- [ ] On partial fill: update `remaining_size`, keep in `ResidualRisk` if still above min_size
- [ ] On full fill: remove position and lifecycle
- [ ] Geometric clip reduction: after 2+ retries, halve the clip size (remaining * 0.5)
- [ ] Dust detection: if remaining < min_order_size after partial, remove position and log
- [ ] After `max_exit_retries` with remaining size > 0: transition to RecoveryProbe (task 15) or resolve with loss
- [ ] Write test: FOK rejected → ResidualRisk with retry_count=1
- [ ] Write test: GTC refresh cycle — order cancelled and replaced after 2s
- [ ] Write test: partial fill reduces remaining_size, stays in ResidualRisk
- [ ] Write test: geometric clip reduction after retries
- [ ] Write test: dust detection removes sub-min-size residual
- [ ] Write test: max retries exhausted → transitions appropriately
- [ ] Run tests — must pass before task 15

### Task 15: Implement recovery logic (opposite-side + re-entry)
- [ ] When `ResidualRisk` with max retries exhausted OR remaining under budget AND time > 30s: transition to `RecoveryProbe`
- [ ] Recovery evaluation step 1 — Set completion: if `entry_price + ask_other_side <= recovery_max_set_cost` (1.01), buy opposite side at best ask (depth-capped FOK). Net recovery = `(1.0 - combined_cost) * size` minus fees
- [ ] Recovery evaluation step 2 — Opposite-side alpha: if composite momentum confirms reversal for `reentry_confirm_ticks` consecutive ticks, buy other side. Guard: extra risk <= `recovery_max_extra_frac` (15%)
- [ ] Recovery evaluation step 3 — Same-side re-entry: after full exit, if original signal resumes with `reentry_confirm_ticks` fresh ticks AND `reentry_cooldown_secs` elapsed. Never bypasses risk caps or max_positions
- [ ] On recovery order fill: transition to `Cooldown { until: now + reentry_cooldown_secs }`
- [ ] On recovery order rejection/failure: accept loss, resolve position, log remaining exposure
- [ ] On `Cooldown` elapsed: transition to `Healthy` (position can be re-evaluated)
- [ ] Look up opposite token_id from market data (token_a vs token_b based on current side)
- [ ] Write test: set completion — entry 0.93, other ask 0.07 → combined 1.00 <= 1.01 → recovery buys
- [ ] Write test: set completion — entry 0.93, other ask 0.10 → combined 1.03 > 1.01 → skip set completion
- [ ] Write test: opposite-side alpha — momentum confirmed for 2 ticks → recovery buys within risk budget
- [ ] Write test: same-side re-entry — signal resumes after cooldown → re-enters
- [ ] Write test: same-side re-entry — cooldown not elapsed → blocks re-entry
- [ ] Write test: recovery failure → position resolved with loss logged
- [ ] Run tests — must pass before task 16

### Task 16: Route order events through lifecycle transitions
- [ ] In `on_order_filled` (tailend.rs): check `exit_orders_by_id` first — if matched, route to lifecycle transition handler
- [ ] ExitExecuting fill → fully filled: remove position + lifecycle; compute PnL with correct fee model
- [ ] ExitExecuting fill → partial: update remaining_size, transition to ResidualRisk
- [ ] RecoveryProbe fill → success: transition to Cooldown
- [ ] RecoveryProbe fill → partial: keep in RecoveryProbe or fallback to resolve
- [ ] On `OrderEvent::Rejected` for exit/recovery orders: classify rejection (reuse `StopLossRejectionKind`), apply cooldown escalation, transition state
- [ ] On `OrderEvent::Cancelled` for exit orders: if market expired → resolve; if stale GTC → re-place in next refresh cycle
- [ ] Clean up `exit_orders_by_id` entries after order resolution
- [ ] Write test: exit fill routes correctly through lifecycle
- [ ] Write test: partial exit fill transitions to ResidualRisk
- [ ] Write test: recovery fill transitions to Cooldown
- [ ] Write test: rejection escalates cooldown correctly
- [ ] Run tests — must pass before task 17

### Task 17: Remove old stop-loss HashMaps
- [ ] Remove `pending_stop_loss: RwLock<HashMap<TokenId, PendingStopLoss>>` from CryptoArbBase — replaced by lifecycle ExitExecuting state
- [ ] Remove `stop_loss_cooldowns: RwLock<HashMap<TokenId, DateTime>>` — replaced by ResidualRisk.last_attempt and lifecycle timing
- [ ] Remove `stop_loss_retry_counts: RwLock<HashMap<TokenId, u32>>` — replaced by ResidualRisk.retry_count
- [ ] Remove `stop_loss_use_gtc: RwLock<HashSet<TokenId>>` — replaced by ResidualRisk.use_gtc_next
- [ ] Remove `gtc_stop_loss_orders: RwLock<HashMap<OrderId, GtcStopLossOrder>>` — replaced by exit_orders_by_id
- [ ] Remove `PendingStopLoss` and `GtcStopLossOrder` structs from types if no longer used
- [ ] Remove `check_stop_loss()` method from base.rs (fully replaced by evaluate_triggers + lifecycle)
- [ ] Remove `is_stop_loss_cooled_down()`, `record_stop_loss_cooldown()` if lifecycle handles this
- [ ] Update any remaining references (TwoSided strategy?) — ensure TwoSided still compiles (it may use separate stop-loss logic or share base methods)
- [ ] Write test: ensure no compilation errors
- [ ] Run full `cargo test --workspace` — must pass
- [ ] Run `cargo clippy --workspace -- -D warnings` — zero warnings

### Task 18: Verify acceptance criteria
- [ ] Verify: trailing at entry 0.99 is automatically detected as unarmable (no 1.04 requirement)
- [ ] Verify: contradictory config defaults produce startup errors
- [ ] Verify: composite price used for stop-loss decisions (not stale single-source)
- [ ] Verify: PnL correctly computed (GTC entry=0% fee, actual fill price used)
- [ ] Verify: 2s GTC refresh cycle works (orders refreshed with current bid)
- [ ] Verify: partial exits reduce position without retry loops on dust
- [ ] Verify: recovery buys opposite side when sell fails and set cost <= 1.01
- [ ] Run full `cargo test --workspace` — all pass
- [ ] Run `cargo clippy --workspace -- -D warnings` — zero warnings
- [ ] Review state transition logs format (useful for dashboard)

### Task 19: [Final] Update documentation
- [ ] Update `config.example.toml` with all new stop-loss lifecycle fields and comments
- [ ] Update CLAUDE.md if any architectural patterns changed (stop-loss section)
- [ ] Add brief note to relevant design doc about state machine approach

## Technical Details

### State Machine
```
Healthy → DeferredExit     (trigger during sell delay)
Healthy → ExitExecuting    (trigger when sellable)
DeferredExit → ExitExecuting (delay elapsed, trigger persists)
DeferredExit → Healthy     (trigger cleared)
ExitExecuting → ResidualRisk (partial/rejected)
ExitExecuting → (resolved) (fully filled)
ResidualRisk → ExitExecuting (retry)
ResidualRisk → RecoveryProbe (risk under budget)
RecoveryProbe → ExitExecuting (recovery fails)
RecoveryProbe → Cooldown   (neutralized)
Cooldown → Healthy         (cooldown elapsed)
```

### Trailing Headroom Formula
```rust
let price_cap = Decimal::ONE - pos.tick_size; // 0.99 for tick=0.01
let headroom = (price_cap - pos.entry_price).max(Decimal::ZERO);
let effective_arm = config.trailing_arm_distance.min(headroom);
if effective_arm < pos.tick_size { trailing_unarmable = true; }
```

### Trigger Priority
1. Hard Crash (1 fresh source + fresh book)
2. Dual Trigger + 2-tick hysteresis (fresh composite + fresh book)
3. Trailing Stop with headroom fix
4. Post-Entry Deferred

### Exit Clip Sizing
```rust
clip = min(remaining, bid_depth_down_to(bid - n*tick) * depth_cap_factor)
```

### Recovery Guards
- Set completion: `entry + ask_other <= 1.01`
- Alpha: extra risk <= 15% of position value
- Re-entry: cooldown 8s + 2 confirm ticks

## Post-Completion

**Manual verification:**
- Paper trading session: monitor state transition logs in dashboard
- Verify stop-loss trigger reasons and freshness metrics in logs
- Stress test with volatile market replay (high-volatility windows)
- Confirm no impossible parameter combinations survive to runtime

**Dashboard updates (future):**
- Display lifecycle state per position in TailEnd dashboard view
- Show trigger reason on exit events
- Add freshness metrics to status logging
