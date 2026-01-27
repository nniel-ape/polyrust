# Strategy Dashboard Views

## Overview
Add custom per-strategy dashboard views so each strategy can render its own HTMX page with domain-specific data. The crypto arbitrage strategy will be the first implementation, showing reference prices, predictions, active markets, and positions — all updating in real-time via SSE.

**Problem**: The dashboard currently shows generic position/PnL data. Strategy-specific data (reference prices, confidence scores, market windows, predictions) is invisible to the operator.

**Integration**: Adds a `dashboard_view` method to the `Strategy` trait (default `None`), a new `/strategy/:name` route, and a `DashboardViewProvider` trait in `polyrust-core` for type-safe view data. The dashboard renders strategy views as separate pages with nav links auto-generated from registered strategies.

## Context
- Strategy trait: `crates/polyrust-core/src/strategy.rs:12-38`
- StrategyContext: `crates/polyrust-core/src/context.rs:9-26`
- Dashboard server: `crates/polyrust-dashboard/src/server.rs:1-68`
- Dashboard handlers: `crates/polyrust-dashboard/src/handlers.rs:1-265`
- Base template: `crates/polyrust-dashboard/templates/base.html:1-28`
- Crypto arb strategy: `crates/polyrust-strategies/src/crypto_arb.rs:174-186`
- Engine: `crates/polyrust-core/src/engine.rs:14-270`

## Development Approach
- **Testing approach**: Regular (code first, then tests)
- Complete each task fully before moving to the next
- Make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes
- **CRITICAL: all tests must pass before starting next task**
- **CRITICAL: update this plan file when scope changes during implementation**
- Run tests after each change
- Maintain backward compatibility (default trait impls, existing strategies unaffected)

## Testing Strategy
- **Unit tests**: Test view data serialization, trait default impls, handler routing
- **Integration tests**: Axum handler tests with tower::ServiceExt for strategy view endpoint

## Progress Tracking
- Mark completed items with `[x]` immediately when done
- Add newly discovered tasks with ➕ prefix
- Document issues/blockers with ⚠️ prefix

## Implementation Steps

### Task 1: Add `DashboardViewProvider` trait to polyrust-core
- [x] Create `crates/polyrust-core/src/dashboard_view.rs` with:
  - `DashboardViewProvider` trait: `fn view_name(&self) -> &str`, `fn render_view(&self) -> Result<String>` (returns HTML fragment)
  - Default impl in `Strategy` trait: `fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> { None }`
- [x] Add `pub mod dashboard_view;` to `crates/polyrust-core/src/lib.rs`
- [x] Export `DashboardViewProvider` in prelude
- [x] Write tests for default `None` return on Strategy trait
- [x] Run `cargo test -p polyrust-core` — must pass before next task

### Task 2: Expose strategy dashboard views through StrategyContext
- [x] Add `strategy_views: Arc<RwLock<HashMap<String, Arc<dyn DashboardViewProvider + Send + Sync>>>>` to `StrategyContext`
- [x] Initialize empty in `StrategyContext::new()`
- [x] In `Engine::build()`, after wrapping strategies, collect `dashboard_view()` from each strategy and populate `strategy_views`
- [x] Add `pub fn strategy_names(&self) -> Vec<String>` helper to StrategyContext (reads strategy_views keys)
- [x] Write tests for strategy_views registration and lookup
- [x] Run `cargo test -p polyrust-core` — must pass before next task

### Task 3: Add `/strategy/:name` route and handler in dashboard
- [x] Add route `.route("/strategy/{name}", get(handlers::strategy_view))` to `server.rs`
- [x] Implement `strategy_view` handler in `handlers.rs`:
  - Extract strategy name from path
  - Look up `strategy_views` in AppState context
  - Call `render_view()` → get HTML fragment
  - Wrap in `strategy_view.html` template (extends base.html)
  - Return 404 if strategy not found
- [x] Create `crates/polyrust-dashboard/templates/strategy_view.html`:
  - Extends base.html
  - Contains SSE connection div for real-time updates
  - Renders HTML fragment from strategy inside a content div
- [x] Create `crates/polyrust-dashboard/templates/partials/strategy_content.html`:
  - Simple wrapper partial for SSE swap target (id="strategy-content")
- [x] Write handler tests (strategy found, strategy not found)
- [x] Run `cargo test -p polyrust-dashboard` — must pass before next task

### Task 4: Add dynamic strategy nav links to base template
- [x] Add `strategy_names: Vec<String>` field to all template structs (IndexTemplate, PositionsTemplate, etc.)
- [x] Populate `strategy_names` in each handler by reading `context.strategy_views`
- [x] Update `base.html` to render strategy nav links dynamically:
  ```html
  {% for name in strategy_names %}
  <a href="/strategy/{{ name }}" class="hover:text-white">{{ name }}</a>
  {% endfor %}
  ```
- [x] Write test verifying nav links appear when strategies have views
- [x] Run `cargo test -p polyrust-dashboard` — must pass before next task

### Task 5: Implement crypto arb `DashboardViewProvider`
- [x] Implement `DashboardViewProvider` for `CryptoArbitrageStrategy`:
  - `view_name()` → `"crypto-arb"`
  - `render_view()` → HTML fragment with:
    - **Reference Prices & Predictions** section: coin, ref price, current price, % change, predicted winner
    - **Active Markets** section: market name, UP/DOWN prices, time remaining
    - **Open Positions** section: market, side, entry price, current price, PnL
  - Use Tailwind classes matching existing dashboard style (gray-900 cards, monospace)
- [x] Return `Some(self)` from `dashboard_view()` in `CryptoArbitrageStrategy`
- [x] Write tests for render_view HTML output (contains expected sections, handles empty state)
- [x] Run `cargo test -p polyrust-strategies` — must pass before next task

### Task 6: Add SSE updates for strategy views
- [x] Extend `sse_events` handler to detect Signal events from strategies
- [x] When a `Signal` event with `signal_type == "dashboard-update"` is received:
  - Look up the strategy's `DashboardViewProvider`
  - Call `render_view()` to get fresh HTML
  - Send as SSE event with event type `strategy-{name}-update`
- [x] Add periodic `EmitSignal` action from crypto arb `on_event()`:
  - Emit `"dashboard-update"` signal every ~5 seconds (throttled)
  - Payload contains serialized view state for the SSE partial
- [x] Update `strategy_view.html` to connect to SSE and swap content on update
- [x] Write tests for SSE signal routing
- [x] Run `cargo test --workspace` — must pass before next task

### Task 7: Verify acceptance criteria
- [x] Verify Strategy trait backward-compatible (strategies without views compile unchanged)
- [x] Verify crypto arb view shows reference prices, predictions, and active markets
- [x] Verify new `/strategy/crypto-arb` route returns 200 with correct content
- [x] Verify 404 returned for `/strategy/nonexistent`
- [x] Verify nav bar shows "crypto-arb" link when strategy is registered
- [x] Run full test suite: `cargo test --workspace`
- [x] Run linter: `cargo clippy --workspace -- -D warnings`

### Task 8: [Final] Update documentation
- [x] Update CLAUDE.md Architecture section to mention strategy dashboard views
- [x] Add dashboard view info to "Adding a New Strategy" section

## Technical Details

### Data Flow
```
Strategy.render_view() → HTML fragment
  ↓
/strategy/:name handler → wraps in template → full page
  ↓
SSE: Signal("dashboard-update") → handler re-renders → HTMX swap
```

### Crypto Arb View Layout
```
┌─ REFERENCE PRICES & PREDICTIONS ─────────────────────────┐
│ BTC: $87,618.67 (=ref) → $87,612.50 (-0.01%) → DOWN      │
│ ETH: $2,914.14 (=ref) → $2,917.77 (+0.12%) → UP          │
│ SOL: $123.49 (=ref) → $123.62 (+0.10%) → UP              │
│ Legend: =ref = exact at window start, ~ref = approximate │
├─ UP/DOWN MARKETS (3) ────────────────────────────────────│
│ BTC Up/Down | UP: 0.535  DOWN: 0.465 | Ends: 09:47       │
│ ETH Up/Down | UP: 0.635  DOWN: 0.365 | Ends: 09:47       │
│ SOL Up/Down | UP: 0.655  DOWN: 0.345 | Ends: 09:47       │
├─ OPEN POSITIONS (1) ─────────────────────────────────────│
│ BTC Down | Entry: 0.47 | Current: 0.52 | +$0.25          │
└──────────────────────────────────────────────────────────┘
```

### Thread Safety
- `DashboardViewProvider` requires `Send + Sync` (strategies are `Arc<RwLock<Box<dyn Strategy>>>`)
- `render_view()` takes `&self` — read-only, no locking issues
- Dashboard handler acquires read lock on strategy, calls `render_view()`, releases lock
- SSE updates are non-blocking (uses `try_read()` pattern like existing PnL partial)

## Post-Completion

**Manual verification**:
- Run bot in paper mode, navigate to `/strategy/crypto-arb` and verify data updates live
- Check that strategies without custom views don't break nav or routing
- Verify SSE reconnection works after page refresh
