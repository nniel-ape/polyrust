# CryptoArb Service-Structured Refactor

## Overview
- Refactor `crates/polyrust-strategies/src/crypto_arb/` (13,386 LOC, 7 flat files) into clean service-oriented subfolders
- Two god objects need decomposition: `base.rs` (2,274 LOC) with 7+ mixed concerns and `tailend.rs` (4,169 LOC) monolithic event handler
- Additional fixes: upward dependency in `types.rs:626`, cross-strategy coupling (`dutch_book` imports `crypto_arb::escape_html`), fragmented tests (4,723 + ~700 LOC across 2 files)
- Purely structural — no trading behavior changes

## Context (from discovery)
- **God object `CryptoArbBase`** at `base.rs:199`: 30 `pub` fields, 60+ methods mixing pricing, market lifecycle, positions, orders, cooldowns, observability
- **Monolithic `TailEndStrategy::on_event`** at `tailend.rs:1839`: 590-line match statement dispatching to 10+ inline handlers
- **Upward dependency**: `types.rs:626` references `super::base::CompositePriceResult` (domain → runtime)
- **Cross-strategy coupling**: `dutch_book/dashboard.rs:26` imports `crate::crypto_arb::escape_html`
- **Clean files** (no changes needed): `config.rs` (673 LOC), already well-organized
- **External callsites**: `src/main.rs:16,191,747`, `crates/polyrust-backtest/src/sweep/runner.rs:7,240`, `crates/polyrust-strategies/src/lib.rs:6`
- **Baseline**: 599 tests passing, 9 ignored

## Target file tree
```
crypto_arb/
  mod.rs                           # Thin facade: module declarations + public re-exports
  config.rs                        # Unchanged (673 LOC)
  runtime.rs                       # CryptoArbRuntime struct def + new() + warm_up() + constants

  domain/
    mod.rs                         # Re-exports all domain types
    market.rs                      # ReferenceQuality, BoundarySnapshot, MarketWithReference,
                                   #   CompositePriceResult, CompositePriceSnapshot
    position.rs                    # ArbitrageOpportunity, ArbitragePosition, PendingOrder,
                                   #   OpenLimitOrder, ExitOrderMeta
    lifecycle.rs                   # PositionLifecycle, PositionLifecycleState, StopLossTriggerKind,
                                   #   TriggerEvalContext, compute_exit_clip()
    telemetry.rs                   # ModeStats, SpikeEvent, OrderTelemetry, StopLossRejectionKind

  services/
    mod.rs                         # Re-exports free functions
    fee_math.rs                    # taker_fee(), net_profit_margin(), kelly_position_size()
    pricing.rs                     # impl Runtime: price history, composite, reference, spike
    market.rs                      # impl Runtime: discovery, expiry, activation, coin tracking
    position.rs                    # impl Runtime: CRUD, reservations, lifecycle state, PnL
    order.rs                       # impl Runtime: order tracking, cooldowns, reconciliation
    observability.rs               # impl Runtime: skip stats, dashboard throttle, status summary

  strategy/
    tailend/
      mod.rs                       # TailEndStrategy struct + Strategy trait impl (thin dispatch)
      entry.rs                     # Opportunity evaluation, threshold logic, handle_external_price
      exit.rs                      # Exit trigger evaluation, build_exit_order, handle_orderbook_update
      order_events.rs              # on_order_placed, on_order_filled, rejected/cancelled handlers

  dashboard/
    mod.rs                         # CryptoArbDashboard struct + DashboardViewProvider impl
    render.rs                      # HTML section rendering (positions, prices, performance, skips)
    updates.rs                     # try_emit_dashboard_updates(), formatting helpers

  tests/
    mod.rs                         # Shared test helpers (make_market_info, make_runtime, etc.)
    test_domain.rs                 # ReferenceQuality, MarketWithReference, ModeStats tests
    test_config.rs                 # Config defaults, deserialization, validation
    test_pricing.rs                # Price history, composite, spike, reference, boundary tests
    test_markets.rs                # Market reservation, lifecycle, coin extraction tests
    test_orders.rs                 # Order reconciliation, cooldowns, rejection classification
    test_lifecycle.rs              # Position lifecycle FSM, evaluate_triggers
    test_tailend.rs                # TailEnd integration (entry, exits, PnL, fast-path)
```

## Development Approach
- **Testing approach**: Regular (code first, verify existing tests pass)
- Complete each task fully before moving to the next
- Make small, focused changes — move code, adjust imports, verify
- **CRITICAL: every task must end with `cargo build --workspace` + `cargo clippy --workspace -- -D warnings`**
- **CRITICAL: no new tests needed** — this is mechanical code movement, existing 599 tests cover all behavior
- **CRITICAL: all tests must pass before starting next task**
- Run tests after each change
- Maintain backward compatibility within the crate (breaking `pub` API allowed, all callsites updated)

## Progress Tracking
- Mark completed items with `[x]` immediately when done
- Add newly discovered tasks with ➕ prefix
- Document issues/blockers with ⚠️ prefix
- Update plan if implementation deviates from original scope

## Implementation Steps

### Task 1: Create shared helpers module and fix cross-strategy coupling
- [x] Create `crates/polyrust-strategies/src/shared.rs` with `escape_html` function (copy from `base.rs`)
- [x] Add `pub mod shared;` to `crates/polyrust-strategies/src/lib.rs`
- [x] Update `crates/polyrust-strategies/src/dutch_book/dashboard.rs:26` to `use crate::shared::escape_html`
- [x] Run `cargo build --workspace && cargo clippy --workspace -- -D warnings`

### Task 2: Extract domain types from `types.rs`
- [x] Create `crypto_arb/domain/` directory
- [x] Create `domain/mod.rs` with re-exports of all public types (matching current `types.rs` API)
- [x] Create `domain/market.rs` — move `ReferenceQuality`, `BoundarySnapshot`, `MarketWithReference`, `CompositePriceSnapshot` and their impls
- [x] Create `domain/position.rs` — move `ArbitrageOpportunity`, `ArbitragePosition`, `PendingOrder`, `OpenLimitOrder`, `ExitOrderMeta` and their impls
- [x] Create `domain/lifecycle.rs` — move `PositionLifecycle`, `PositionLifecycleState`, `StopLossTriggerKind`, `TriggerEvalContext`, `compute_exit_clip()` and their impls
- [x] Create `domain/telemetry.rs` — move `ModeStats`, `SpikeEvent`, `OrderTelemetry` and their impls
- [x] Move `CompositePriceResult` from `base.rs` to `domain/market.rs` (fix upward dependency)
- [x] Move `StopLossRejectionKind` from `base.rs` to `domain/telemetry.rs`
- [x] Fix `CompositePriceSnapshot::from_result` to reference local `CompositePriceResult` (same module)
- [x] Update `crypto_arb/mod.rs` to declare `mod domain` instead of `mod types`, re-export all types
- [x] Update all `use crate::crypto_arb::types::...` imports throughout crypto_arb to use `domain::`
- [x] Delete `types.rs` after all contents moved
- [x] Run `cargo build --workspace && cargo clippy --workspace -- -D warnings`

### Task 3: Create `runtime.rs` from `base.rs` struct definition
- [ ] Create `crypto_arb/runtime.rs` with `CryptoArbRuntime` struct (copy struct definition from `base.rs:199-284`)
- [ ] Rename `CryptoArbBase` → `CryptoArbRuntime` in the struct
- [ ] Change all field visibility from `pub` to `pub(super)` (except `config` which stays `pub` for read access)
- [ ] Move constants `PRICE_HISTORY_SIZE`, `BOUNDARY_TOLERANCE_SECS`, `WINDOW_SECS` to `runtime.rs`
- [ ] Move `CryptoArbRuntime::new()` and `warm_up()` to `runtime.rs`
- [ ] Move `update_event_time()` and `event_time()` to `runtime.rs`
- [ ] Add `mod runtime;` to `crypto_arb/mod.rs`, update re-exports from `base::CryptoArbBase` to `runtime::CryptoArbRuntime`
- [ ] Keep `base.rs` intact for now (methods still reference `CryptoArbBase` — will be migrated in tasks 4-5)
- [ ] Run `cargo build --workspace` — expect failures from rename, fix incrementally

### Task 4: Extract service modules from `base.rs` methods
- [ ] Create `crypto_arb/services/` directory with `mod.rs`
- [ ] Create `services/fee_math.rs` — move free functions: `taker_fee()`, `net_profit_margin()`, `kelly_position_size()`, `parse_slug_timestamp()` (if present)
- [ ] Create `services/pricing.rs` — move `impl CryptoArbRuntime` methods: `record_price`, `get_latest_price`, `get_settlement_price`, `check_sustained_direction`, `max_recent_volatility`, `are_feeds_fresh`, `record_signal_veto`, `composite_fair_price`, `update_sl_composite_cache`, `get_sl_composite`, `get_sl_single_fresh`, `detect_spike`, `record_spike`, `find_best_reference`, `prune_boundary_snapshots`, `try_upgrade_quality`
- [ ] Create `services/market.rs` — move: `on_market_discovered`, `on_market_expired`, `promote_pending_markets`, `activate_market`, `rebuild_nearest_expiry`, `is_tracked_coin`, `extract_coin`
- [ ] Create `services/position.rs` — move: `can_open_position`, `validate_min_order_size`, `has_market_exposure`, `record_position`, `ensure_lifecycle`, `remove_lifecycle`, `get_opposite_token`, `remove_position_by_token`, `reduce_or_remove_position_by_token`, `update_peak_bid`, `is_auto_disabled`, `record_trade_pnl`, `adjust_trade_pnl`, `add_recovery_cost`
- [ ] Create `services/order.rs` — move: `try_reserve_market`, `release_reservation`, `consume_reservation`, `handle_cancel_failed`, `reconcile_limit_orders`, `check_stale_limit_orders`, `record_rejection_cooldown`, `is_rejection_cooled_down`, `record_stale_market_cooldown`, `is_stale_market_cooled_down`, `record_recovery_exit_cooldown`, `is_recovery_exit_cooled_down`
- [ ] Create `services/observability.rs` — move: `record_tailend_skip`, `try_claim_dashboard_emit`, `maybe_log_status_summary`
- [ ] Update `services/mod.rs` to re-export free functions (`taker_fee`, `net_profit_margin`, `kelly_position_size`)
- [ ] Delete `base.rs` after all methods migrated
- [ ] Update `crypto_arb/mod.rs`: remove `mod base`, add `mod services`, update re-exports
- [ ] Fix all import paths in service files (`use super::runtime::CryptoArbRuntime`, domain imports, etc.)
- [ ] Run `cargo build --workspace && cargo clippy --workspace -- -D warnings`

### Task 5: Split `tailend.rs` into `strategy/tailend/`
- [ ] Create `crypto_arb/strategy/` and `strategy/tailend/` directories
- [ ] Create `strategy/tailend/mod.rs` — move `TailEndStrategy` struct, `new()`, Strategy trait impl (`name`, `description`, `on_start`, `on_stop`, `dashboard_view`, thin `on_event` dispatch table)
- [ ] Create `strategy/tailend/entry.rs` — move: `get_ask_threshold_impl()`, `evaluate_opportunity()`, `handle_external_price()`
- [ ] Create `strategy/tailend/exit.rs` — move: `evaluate_exits_on_price_change()`, `handle_orderbook_update()`, `build_exit_order()`, `write_lifecycle()`
- [ ] Create `strategy/tailend/order_events.rs` — move: `on_order_placed()`, `on_order_filled()`, extract inline `on_event` match arms as named methods: `handle_partially_filled()`, `handle_rejected()`, `handle_cancelled()`, `handle_cancel_failed()`, `handle_open_order_snapshot()`
- [ ] Refactor `on_event` in `mod.rs` to be a thin dispatcher calling methods from entry/exit/order_events
- [ ] Add `mod strategy;` to `crypto_arb/mod.rs`, update `TailEndStrategy` re-export path
- [ ] Delete `tailend.rs` after all contents moved
- [ ] Fix all import paths across strategy files
- [ ] Run `cargo build --workspace && cargo clippy --workspace -- -D warnings`

### Task 6: Split `dashboard.rs` into `dashboard/`
- [ ] Create `crypto_arb/dashboard/` directory
- [ ] Create `dashboard/mod.rs` — move `CryptoArbDashboard` struct, `DashboardViewProvider` impl, `render_view` orchestrator
- [ ] Create `dashboard/render.rs` — move `render_reference_prices()`, `render_positions()`, `render_performance()`, `render_skip_stats()`
- [ ] Create `dashboard/updates.rs` — move `try_emit_dashboard_updates()`, `fmt_usd()`, `fmt_market_price()`
- [ ] Update `crypto_arb/mod.rs`: `mod dashboard` now resolves to `dashboard/mod.rs` (automatic)
- [ ] Update dashboard imports from `base::` to `runtime::` / `services::` / `shared::`
- [ ] Delete old `dashboard.rs`
- [ ] Run `cargo build --workspace && cargo clippy --workspace -- -D warnings`

### Task 7: Reorganize tests into `tests/`
- [ ] Create `crypto_arb/tests/` directory
- [ ] Create `tests/mod.rs` — consolidate shared helpers from `tests.rs` and `tailend.rs` inline tests (deduplicate `make_market_info`)
- [ ] Create `tests/test_domain.rs` — move `ReferenceQuality`, `MarketWithReference`, `ModeStats`, `SpikeEvent` tests
- [ ] Create `tests/test_config.rs` — move config defaults, deserialization, validation tests
- [ ] Create `tests/test_pricing.rs` — move price history, composite, spike, reference, boundary tests
- [ ] Create `tests/test_markets.rs` — move market reservation, lifecycle, coin extraction tests
- [ ] Create `tests/test_orders.rs` — move order reconciliation, cooldowns, rejection classification tests
- [ ] Create `tests/test_lifecycle.rs` — move position lifecycle FSM, `evaluate_triggers` tests
- [ ] Create `tests/test_tailend.rs` — move TailEnd integration tests (entry, exits, PnL, fast-path) + inline tests from old `tailend.rs`
- [ ] Delete old `tests.rs`
- [ ] Update `crypto_arb/mod.rs`: `#[cfg(test)] mod tests` resolves to `tests/mod.rs`
- [ ] Run `cargo test --workspace` — verify 599 tests still pass
- [ ] Run `cargo clippy --workspace -- -D warnings`

### Task 8: Update external callsites
- [ ] Update `src/main.rs:16` — change import from `CryptoArbBase` to `CryptoArbRuntime`
- [ ] Update `src/main.rs:191,747` — `CryptoArbBase::new(` → `CryptoArbRuntime::new(`
- [ ] Update `crates/polyrust-backtest/src/sweep/runner.rs:7` — change import
- [ ] Update `crates/polyrust-backtest/src/sweep/runner.rs:240` — change construction call
- [ ] Update `crates/polyrust-strategies/src/lib.rs:6` — re-export `CryptoArbRuntime` (keep `CryptoArbBase` as deprecated type alias if needed, or remove cleanly)
- [ ] Run `cargo build --workspace && cargo clippy --workspace -- -D warnings`

### Task 9: Final verification and cleanup
- [ ] Verify no production file in `crypto_arb/` exceeds 900 LOC (check with `wc -l`)
- [ ] Verify domain module has zero imports from `runtime` or `services`
- [ ] Verify Dutch Book has zero imports from `crypto_arb` (only `crate::shared`)
- [ ] Run `cargo build --release`
- [ ] Run `cargo test --workspace` — verify 599 tests, 9 ignored
- [ ] Run `cargo clippy --workspace -- -D warnings`
- [ ] Clean up `~/.claude/plans/reflective-wiggling-yeti.md` (old plan file)

## Technical Details

### Method → Service Mapping

| Service File | Methods from `base.rs` |
|---|---|
| `runtime.rs` | struct (30 fields), `new()`, `warm_up()`, `update_event_time()`, `event_time()` |
| `services/fee_math.rs` | `taker_fee()`, `net_profit_margin()`, `kelly_position_size()` |
| `services/pricing.rs` | `record_price`, `get_latest_price`, `get_settlement_price`, `check_sustained_direction`, `max_recent_volatility`, `are_feeds_fresh`, `record_signal_veto`, `composite_fair_price`, `update_sl_composite_cache`, `get_sl_composite`, `get_sl_single_fresh`, `detect_spike`, `record_spike`, `find_best_reference`, `prune_boundary_snapshots`, `try_upgrade_quality` |
| `services/market.rs` | `on_market_discovered`, `on_market_expired`, `promote_pending_markets`, `activate_market`, `rebuild_nearest_expiry`, `is_tracked_coin`, `extract_coin` |
| `services/position.rs` | `can_open_position`, `validate_min_order_size`, `has_market_exposure`, `record_position`, `ensure_lifecycle`, `remove_lifecycle`, `get_opposite_token`, `remove_position_by_token`, `reduce_or_remove_position_by_token`, `update_peak_bid`, `is_auto_disabled`, `record_trade_pnl`, `adjust_trade_pnl`, `add_recovery_cost` |
| `services/order.rs` | `try_reserve_market`, `release_reservation`, `consume_reservation`, `handle_cancel_failed`, `reconcile_limit_orders`, `check_stale_limit_orders`, 6 cooldown methods |
| `services/observability.rs` | `record_tailend_skip`, `try_claim_dashboard_emit`, `maybe_log_status_summary` |

### Strategy Split

| Strategy File | Methods from `tailend.rs` |
|---|---|
| `strategy/tailend/mod.rs` | struct, `new()`, Strategy trait impl (thin `on_event` dispatch) |
| `strategy/tailend/entry.rs` | `get_ask_threshold_impl()`, `evaluate_opportunity()`, `handle_external_price()` |
| `strategy/tailend/exit.rs` | `evaluate_exits_on_price_change()`, `handle_orderbook_update()`, `build_exit_order()`, `write_lifecycle()` |
| `strategy/tailend/order_events.rs` | `on_order_placed()`, `on_order_filled()`, 5 extracted event handlers |

### Key Rust Pattern
`impl CryptoArbRuntime` blocks can exist in multiple files — each service file adds methods to the runtime struct without any trait indirection or wrapper types.

## Post-Completion

**Manual verification:**
- Spot-check that `cargo run -- --backtest` still produces identical results with the same config
- Verify dashboard renders correctly at `/strategy/crypto-arb-tailend`

**Documentation updates:**
- Update CLAUDE.md references from `CryptoArbBase` to `CryptoArbRuntime`
- Update file path references in CLAUDE.md to reflect new directory structure
