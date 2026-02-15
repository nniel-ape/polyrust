# Fast-Exit v2 Architecture

> Date: 2026-02-15
> Context: Redesign of TailEnd stop-loss exit mechanism for sub-second reaction to price reversals in 15-minute crypto markets

## Motivation

The TailEnd strategy has four exit-path bottlenecks that compound into slow, unreliable exits in 15-minute markets where every second matters:

1. **Exit triggers only fire on OrderbookUpdate events** — external price feeds (Binance, Coinbase) arrive 50-200ms ahead of CLOB orderbook updates, but exits only evaluate on CLOB ticks. A 200ms head start on a $0.10 price move means the difference between selling at $0.92 and $0.90.

2. **FOK exits reject in thin markets** — Fill-Or-Kill is all-or-nothing; if the top-of-book depth can't absorb the full clip, the entire order rejects. The strategy then enters a slow retry loop (2s cooldown, geometric clip reduction at `exit_depth_cap_factor`), burning critical seconds.

3. **Sell delay gates all exits** — the `min_sell_delay_secs` (4s) prevents all exits including catastrophic hard crashes. A hard crash at 99→85 bid must wait 4 seconds before acting, during which the bid may drop further.

4. **Hedge triggers too late** — set completion (opposite-side buy) only activates after 5 failed exit retries in the `RecoveryProbe` state. By then the exit price has deteriorated and the opposite ask has risen.

## FOK vs FAK vs GTC for Exits

### Order Type Comparison

| Property | FOK (Fill-Or-Kill) | FAK (Fill-And-Kill) | GTC (Good-Til-Cancelled) |
|----------|-------------------|--------------------|-----------------------|
| Partial fills | No — all or nothing | Yes — fills what's available | Yes — rests on book |
| Unfilled portion | Cancelled entirely | Cancelled immediately | Stays on book |
| Fee | Taker (3.15% at 50/50) | Taker (same as FOK) | Maker (0%) |
| USDC precision | `price_decimals` only | Same as FOK | `price_decimals + size_decimals` |
| Price rounding | Raw (round UP buy, DOWN sell) | Same as FOK | Tick-rounded |
| Latency | Immediate | Immediate | Queued |
| SDK variant | `SdkOrderType::FOK` | `SdkOrderType::FAK` | `SdkOrderType::GTC` |

Implementation: `OrderType::Fak` added at `types.rs:44`. Rounding branches at `rounding.rs:121` pattern-match `(OrderType::Fok | OrderType::Fak, _)` — identical precision. SDK mapping at `rounding.rs:255`.

### Why FAK + GTC Hybrid

FOK's failure mode is binary: it either fills completely or rejects entirely. In thin 15-minute markets near expiry, top-of-book depth is often 30-70% of the position size. FOK rejects, the strategy retries with reduced clip, and 2+ seconds are lost.

FAK solves the immediate problem: fill whatever depth exists, cancel the rest. But the remaining position still needs to exit. Placing a GTC at `bid - tick_offset` captures the residual at maker rates (0% fee) and avoids the retry loop entirely.

The hybrid approach:
```
FAK sell at current_bid (depth-capped clip)
  ├─ full fill → position resolved
  ├─ partial fill → GTC placed for residual at bid - tick_offset
  │   ├─ GTC fills → position resolved
  │   └─ GTC stale (>2s) → cancel, replace at new bid (chase)
  └─ zero fill → GTC at bid - tick_offset (fallback)
```

This eliminates the geometric retry loop entirely. The GTC chase cycle (`gtc_stop_loss_max_age_secs = 2s`) is event-driven — it runs on every orderbook update, checking if the resting GTC is stale. Implementation at `tailend.rs:1084-1094`.

## Price-Feed Frontrunning Analysis

### Timing Window

External price feeds arrive ahead of CLOB orderbook updates:

| Source | Typical Latency | Update Frequency |
|--------|----------------|-----------------|
| Binance Futures | ~50ms | Continuous |
| Binance Spot | ~80ms | Continuous |
| Coinbase | ~100ms | Continuous |
| CLOB Orderbook (WS) | ~200-500ms | On trade/cancel |
| Chainlink Oracle | ~1000ms+ | Heartbeat + deviation |

When BTC drops 0.5% in 100ms, Binance futures reflects this at T+50ms. The CLOB orderbook doesn't update until market makers adjust their quotes at T+200-500ms. During this window, the strategy has information that the CLOB bids are stale and will likely drop.

### Fast-Path Implementation

`evaluate_exits_on_price_change()` at `tailend.rs:833-900`:

1. Receives `ExternalPrice` event with fresh source price and source timestamp
2. Looks up cached orderbook snapshot from `StrategyContext.market_data`
3. Checks book freshness: `book_age <= fast_path_max_book_age_ms` (default 2000ms)
4. If fresh enough: builds `TriggerEvalContext` with cached bid + fresh external price
5. Calls existing `evaluate_triggers()` (4-level hierarchy at `types.rs:734-857`)
6. If trigger fires: returns exit actions immediately

The freshness gate (`fast_path_max_book_age_ms`) prevents acting on arbitrarily stale book data. If the last orderbook update was >2s ago, the cached bid may no longer be available — skip and wait for a fresh orderbook tick.

This is called from `handle_external_price()` BEFORE entry evaluation — risk management takes priority over new entries.

Config: `fast_path_enabled` (default: true, `config.rs:120`), `fast_path_max_book_age_ms` (default: 2000, `config.rs:124`).

### Safety Considerations

The fast path uses cached CLOB bids, not live bids. Two failure modes:

1. **Stale bid (bid already dropped)**: Exit order at cached bid will fail. FAK handles this gracefully — fills at whatever depth exists, GTC captures residual. Net: slightly worse execution vs live bid, but still faster than waiting for CLOB tick.

2. **False signal (external price bounces back)**: The trigger hierarchy's thresholds (hard_drop_abs=0.50, post_entry_exit_drop=0.12) are calibrated from backtest to minimize false positives. The fast path doesn't change these thresholds — it just evaluates them earlier.

## Sell Delay Analysis

### Problem

`min_sell_delay_secs` (4s) was introduced to avoid selling immediately after entry when CLOB settlement hasn't confirmed the fill. But it gates ALL exits, including catastrophic hard crashes.

In a hard crash scenario (bid drops from 0.97 to 0.85), waiting 4 seconds costs ~$0.02-0.05 per token as remaining bids are consumed. On a 200-token position, that's $4-10 lost to the delay.

### Solution: Bypass for Hard Crashes

Hard crash triggers (`StopLossTriggerKind::HardCrash`) set `is_sellable = true` regardless of sell delay. Implementation at `tailend.rs:1225-1227`.

Non-hard triggers (PostEntryExit, DualTrigger, TrailingStop) respect sell delay as before. If they fire during the delay window, they simply skip and re-evaluate on the next tick. No state transition needed.

### DeferredExit Removal

The old `DeferredExit` state existed to "remember" that a trigger fired during sell delay and execute it once the delay expired. This added complexity (state transition tracking, timeout management) for a case that resolves naturally: triggers re-evaluate every tick, so if the condition persists after sell delay, it fires again.

`DeferredExit` was removed from `PositionLifecycleState`. Triggers during sell delay are simply no-ops that re-evaluate next tick.

## Proactive Hedge vs Reactive Recovery

### Before (Reactive Recovery)

```
Exit fails → retry 5 times with geometric reduction
  → all retries fail → RecoveryProbe state
    → try set completion (buy opposite)
    → if set completion fails → try alpha re-entry
    → Cooldown → eventually resolve
```

Problems:
- 5 failed retries × 2s cooldown = 10+ seconds before hedge attempt
- Opposite ask has risen during those 10 seconds
- Recovery is sequential — try set completion, wait, try alpha re-entry
- 3 extra lifecycle states (ResidualRisk, RecoveryProbe, Cooldown)

### After (Proactive Hedge)

```
Exit trigger fires → simultaneously:
  1. FAK sell at current bid (exit)
  2. GTC buy opposite at current ask (hedge)

Outcomes:
  - Both fill → set complete ($1.00 at expiry), position hedged
  - Sell fills first → cancel hedge, position resolved via sell
  - Hedge fills first → cancel sell, position in Hedged state (wait for expiry)
  - Both reject → GTC residual for sell, hedge was best-effort
```

Implementation at `tailend.rs:1319-1325` (hedge evaluation in build_exit_order) and `tailend.rs:1388-1467` (evaluate_hedge function).

Profitability check: `entry_price + opposite_ask <= recovery_max_set_cost` (default 1.01). If the hedge isn't profitable, sell-only exit proceeds.

### Benefits

- Hedge attempts at optimal price (simultaneous with exit, not 10s later)
- Eliminates 3 lifecycle states and ~300 lines of recovery code
- No retry loop — FAK + GTC handles partial fills natively
- Hedge is best-effort — rejection just means sell-only exit

## Lifecycle Simplification

### Before: 6 States

```
Healthy → DeferredExit → ExitExecuting → ResidualRisk → RecoveryProbe → Cooldown
```

| State | Purpose | Problem |
|-------|---------|---------|
| DeferredExit | Remember trigger during sell delay | Unnecessary — triggers re-evaluate every tick |
| ResidualRisk | Track FOK retry loop for partial exits | Eliminated by FAK + GTC hybrid |
| RecoveryProbe | Try set completion after exit failures | Replaced by proactive simultaneous hedge |
| Cooldown | Wait before re-entering market | Simplified to market-level cooldown (base.rs `recovery_exit_cooldowns`) |

### After: 3 States

```
Healthy → ExitExecuting → Hedged | (resolved)
```

Defined at `types.rs:508-538`:

- **Healthy**: Position open, monitoring triggers. Entry point for all new positions.
- **ExitExecuting**: Exit in progress. Tracks the active sell order (FAK or GTC residual), optional hedge order, and timestamps. Fields: `order_id`, `order_type` (Fak/Gtc), `exit_price`, `submitted_at`, `hedge_order_id`, `hedge_price`.
- **Hedged**: Both tokens held (original + opposite). Set complete, guaranteed $1.00 at market resolution. Fields: `hedge_cost`, `hedged_at`.

State transitions are simpler and all paths lead to resolution:
- Healthy → ExitExecuting (trigger fires)
- ExitExecuting → resolved (sell fills completely)
- ExitExecuting → Hedged (hedge fills, sell cancelled)
- Hedged → resolved (market expires, tokens redeemed)

## Timestamp Correctness

### Problem

Before fast-exit v2, `record_price()` used wall-clock `now` for both receive-time and source-time. When evaluating trigger freshness, a Binance price from 200ms ago looked the same as one from 0ms ago — both had `age = now - insert_time`, which was always ~0ms.

### Solution

`record_price()` at `base.rs:393-425` now accepts and stores source timestamp separately. ExternalPrice events carry their source's timestamp (e.g., Binance's exchange timestamp), which is passed through `handle_external_price()`.

Freshness gates in `evaluate_triggers()` use source timestamp age for external price, not wall-clock insertion time. This correctly identifies stale data even if it arrived recently.

### Source-Priority Fallback

`composite_fair_price()` at `base.rs:673-750` uses a weighted composite of multiple sources. When the composite quorum (min_sources) fails, it falls back to a single fresh source using priority order at `base.rs:719`:

1. `binance-futures` (weight 0.5) — highest liquidity, tightest spreads
2. `binance-spot` (weight 0.3) — second most liquid
3. `coinbase` (weight 0.2) — third
4. `chainlink` (fallback) — on-chain oracle, highest latency

This ensures exit decisions always have a price reference, even if most feeds are temporarily unavailable.

## Backtest Limitations

The backtest engine fills orders immediately at historical trade prices. This means:

- **FAK partial fills don't occur** — backtest always fills the full clip. The FAK→GTC residual path is untested in backtest.
- **Book depth is unknown** — historical orderbook depth isn't available from Polymarket APIs. The `exit_depth_cap_factor` sizing has no validation data.
- **Fast-path timing can't be measured** — backtest replays events chronologically without simulating feed latency. ExternalPrice and OrderbookUpdate arrive "simultaneously".
- **Hedge simultaneous execution isn't realistic** — both FAK sell and GTC hedge fill atomically in backtest, which doesn't reflect real execution risk.

These limitations are inherent to the available data. Paper trading and live monitoring are required to validate the fast-path timing advantage and FAK partial fill handling.

## Config Changes Summary

### New Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `fast_path_enabled` | `true` | Enable exit evaluation on ExternalPrice events |
| `fast_path_max_book_age_ms` | `2000` | Max age of cached orderbook for fast-path exits |

### Removed Parameters (backward-compatible via serde skip)

| Parameter | Reason |
|-----------|--------|
| `max_exit_retries` | No retry loop with FAK + GTC hybrid |
| `short_limit_refresh_secs` | GTC chase is event-driven (every orderbook update) |
| `short_limit_tick_offset` | Simplified to `gtc_fallback_tick_offset` |
| `reentry_confirm_ticks` | Proactive hedge replaces alpha re-entry |
| `reentry_cooldown_secs` | Market-level cooldown only |

### Kept Parameters

| Parameter | Usage |
|-----------|-------|
| `recovery_max_set_cost` | Hedge profitability threshold (default 1.01) |
| `exit_depth_cap_factor` | FAK initial clip sizing |
| `gtc_fallback_tick_offset` | GTC residual pricing |
| `gtc_stop_loss_max_age_secs` | Stale GTC refresh threshold (2s) |

Old configs with removed params still parse correctly — `#[serde(default)]` on remaining fields ensures backward compatibility.
