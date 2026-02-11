# TailEnd Strategy Performance Deep Research

Date: 2026-02-11

## Objective

Identify the highest-leverage ways to improve `crypto-arb-tailend` performance in this repo, based on:

1. Current implementation behavior.
2. Existing backtest/sweep evidence in this workspace.
3. Current Polymarket CLOB/RTDS mechanics from official docs.

## TL;DR

Your entry filters are already near convergence, but execution quality is the next bottleneck. The biggest remaining gains are likely from:

1. Hardening maker execution with `postOnly` + tick-aware pricing.
2. Using true CLOB batch order submission (already supported by your SDK version, not used in backend).
3. Improving data freshness control with heartbeat/sequence safeguards.
4. Adding external lead-lag signal fusion (Binance + Coinbase) for entry timing and stop-loss confirmation.
5. Making backtests execution-realistic enough to optimize live-sensitive knobs.

Current calibrated baseline is strong: around `99.65%` win rate, `0.141` Sharpe, `~0.82%` max drawdown, and `~2836` trades for the Dec 1, 2025 to Jan 31, 2026 window.

## Scope And Method

Research inputs used:

1. Current config and strategy implementation:
   - `config.toml:90`
   - `crates/polyrust-strategies/src/crypto_arb/tailend.rs:44`
   - `crates/polyrust-strategies/src/crypto_arb/tailend.rs:482`
   - `crates/polyrust-strategies/src/crypto_arb/tailend.rs:536`
   - `crates/polyrust-strategies/src/crypto_arb/base.rs:1685`
2. Latest sweep artifacts:
   - `docs/reports/tailend-calibration.md:467`
   - `sweep_results_sensitivity.csv:1`
3. Backtest engine assumptions:
   - `crates/polyrust-backtest/src/engine/mod.rs:1093`
   - `src/main.rs:612`
4. Execution/feed layer:
   - `crates/polyrust-execution/src/live.rs:304`
   - `crates/polyrust-market/src/clob_feed.rs:113`
   - `crates/polyrust-market/src/price_feed.rs:90`
5. Official Polymarket docs and changelog:
   - [Fees](https://docs.polymarket.com/developers/CLOB/introduction#fees)
   - [Rate Limits](https://docs.polymarket.com/quickstart/introduction/rate-limits)
   - [Data Feeds Best Practices](https://docs.polymarket.com/developers/CLOB/websocket/data-feeds)
   - [Market Channel](https://docs.polymarket.com/developers/CLOB/websocket/market-channel)
   - [Heartbeats](https://docs.polymarket.com/developers/Utility-Endpoints/heartbeat)
   - [Maker Rewards](https://docs.polymarket.com/developers/rewards/overview)
   - [CLOB Changelog (Jan 6, 2026)](https://docs.polymarket.com/changelog/changelog)
6. External venue market data docs:
   - [Binance Spot API Docs (official)](https://github.com/binance/binance-spot-api-docs)
   - [Coinbase Exchange WebSocket Channels](https://docs.cdp.coinbase.com/exchange/websocket-feed/channels)
   - [Coinbase Advanced Trade WebSocket Channels](https://docs.cdp.coinbase.com/coinbase-app/advanced-trade-apis/websocket/websocket-channels)

## Baseline Snapshot

Current production-like tailend config is:

1. `dynamic_thresholds = [[120, 0.90], [90, 0.92], [60, 0.96], [30, 0.94]]`
2. `max_spread_bps = 200`
3. `max_recent_volatility = 0.020`
4. `stale_ob_secs = 30`

Source: `config.toml:90`

Latest sweep evidence indicates:

1. Stop-loss/post-entry axes are inert in current backtest setup.
2. Mean metrics are identical across these swept values (`243` combinations): PnL, Sharpe, WR, drawdown.
3. Sensitivity rows are numerically identical for each tested value.

Sources:

1. `docs/reports/tailend-calibration.md:483`
2. `sweep_results_sensitivity.csv:2`

## Key Findings

### 1) Entry filtering is no longer the main limiter

Evidence from sweep history and config indicates core entry filters have already been tuned aggressively, especially around `dynamic_thresholds.60`, spread, and volatility. Most remaining alpha is likely in execution quality and queue outcomes, not one more threshold tweak.

### 2) TailEnd sizing is fixed and does not use Kelly path

TailEnd still uses fixed share count from `base_size / buy_price`:

- `crates/polyrust-strategies/src/crypto_arb/tailend.rs:482`

This is simple and robust, but it leaves performance on the table in heterogeneous liquidity and confidence regimes.

### 3) Quote aggressiveness is static and not tick-adaptive

TailEnd uses:

1. Fixed `limit_offset`.
2. Hard clamp to `0.99`.
3. No dynamic adaptation to market tick transitions.

Source: `crates/polyrust-strategies/src/crypto_arb/tailend.rs:538`

Polymarket market channel exposes `tick_size_change` events and `0.001` increments above high prices, which can materially affect queue position in tail-end zones.

Source: [Market Channel](https://docs.polymarket.com/developers/CLOB/websocket/market-channel)

### 4) Safety-depth cap is hardcoded

Order size is capped to `50%` of visible depth, hardcoded:

- `crates/polyrust-strategies/src/crypto_arb/tailend.rs:516`

This likely improves rejection control, but a single constant across all volatility/liquidity regimes is unlikely optimal.

### 5) Backend is not using available batch order post path

Live backend states SDK lacks batch order endpoint and falls back to sequential placement:

- `crates/polyrust-execution/src/live.rs:304`

But your locked SDK (`polymarket-client-sdk 0.4.1`) includes `post_orders()`:

- `Cargo.toml:33`
- `/Users/andrey/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/polymarket-client-sdk-0.4.1/src/clob/client.rs:1480`

This mismatch can add avoidable latency and rate-limit pressure in bursty windows.

### 6) Feed robustness opportunities exist

Current CLOB feed consumes orderbook stream only:

- `crates/polyrust-market/src/clob_feed.rs:113`

Official docs emphasize sequence handling, snapshot hygiene, and heartbeat-driven liveness checks for reliable low-latency trading loops.

Source: [Data Feeds Best Practices](https://docs.polymarket.com/developers/CLOB/websocket/data-feeds), [Heartbeats](https://docs.polymarket.com/developers/Utility-Endpoints/heartbeat)

### 7) Backtest cannot currently optimize several live-critical behaviors

Backtest executes immediate fills and notes missing historical depth:

- `crates/polyrust-backtest/src/engine/mod.rs:1093`

Also forces reference quality to `Current`:

- `src/main.rs:612`

So backtest under-represents queue/fill competition and reference-quality effects that are central to live tail-end performance.

### 8) External-feed frontrun logic is high-upside, but only with strict gating

Current state in repo:

1. RTDS gives Binance + Chainlink crypto prices:
   - `crates/polyrust-market/src/price_feed.rs:90`
2. Direct Binance spot/futures streams are already wired:
   - `crates/polyrust-market/src/binance_feed.rs:20`
3. No Coinbase feed adapter exists yet.

Opportunity:

1. Use external venues as lead-lag predictors for short-lived dislocations.
2. Trigger earlier entries when external momentum leads Polymarket repricing.
3. Trigger earlier defensive exits when external reversal arrives before local book updates.

Constraints from venue docs:

1. Coinbase `level2` guarantees delivery and is the correct basis for robust mid-price reconstruction.
2. Coinbase `ticker_batch` is 5s cadence and is too slow as a primary trigger for tail-end windows.
3. Heartbeat channels should be consumed to detect missed messages and stale subscriptions.

Sources:

1. [Coinbase Exchange WebSocket Channels](https://docs.cdp.coinbase.com/exchange/websocket-feed/channels)
2. [Coinbase Advanced Trade WebSocket Channels](https://docs.cdp.coinbase.com/coinbase-app/advanced-trade-apis/websocket/websocket-channels)

## Prioritized Improvement Plan

## P0: Execution Alpha Capture (Highest Impact)

### P0.0 Add external lead-lag signal stack (Binance + Coinbase)

Implementation shape:

1. Add Coinbase feed module in `polyrust-market` (`level2` + `ticker`, optional `heartbeats`).
2. Build composite fair price per coin from:
   - Binance spot
   - Binance futures mark
   - Coinbase top-of-book mid (or L2-derived mid)
3. Compute short-horizon lead signals:
   - external momentum (`100ms` to `1500ms`)
   - divergence between external fair price and Polymarket implied odds
4. Attach confidence tags to `ExternalPrice`-derived signals (freshness, source quorum, variance).

Entry gating rules:

1. Freshness gate: all required sources within max staleness budget.
2. Persistence gate: signal must hold for N consecutive ticks (avoid one-tick wicks).
3. Liquidity gate: local CLOB spread/depth must satisfy tailend thresholds.

Stop-loss gating rules:

1. Fast pre-trigger from external reversal.
2. Confirm with local CLOB deterioration before hard exit (reduce false stopouts).
3. Override path: immediate exit if external crash exceeds hard threshold.

Why this belongs in P0:

1. It directly targets timing edge, which is currently the primary residual alpha.
2. It improves both offense (earlier valid entries) and defense (faster risk-off).
3. It uses infrastructure you partially already have (Binance streams + RTDS).

### P0.1 Add `postOnly` mode for TailEnd entry orders

Why:

1. Enforces maker behavior.
2. Prevents accidental taker crosses during micro-jumps.
3. Better alignment with fee/reward structure.

Proof of feasibility:

1. API supports `postOnly`.
2. SDK has `post_only` field in signed order model.

Sources:

1. [CLOB Changelog](https://docs.polymarket.com/changelog/changelog)
2. `/Users/andrey/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/polymarket-client-sdk-0.4.1/src/clob/types/mod.rs:447`

### P0.2 Replace fixed `limit_offset` with tick-aware queue-step policy

Why:

1. Static +0.01 is too coarse near high-confidence prices.
2. Tick transitions imply finer queueing opportunities.

Implementation direction:

1. Compute one-tick or two-tick step based on active tick size.
2. React to tick-size changes from market channel.

### P0.3 Use real batch order posting in backend

Why:

1. Reduces latency and HTTP overhead.
2. Helps remain below matching-engine and endpoint caps during bursts.

Sources:

1. `crates/polyrust-execution/src/live.rs:304`
2. `/Users/andrey/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/polymarket-client-sdk-0.4.1/src/clob/client.rs:1480`
3. [Rate Limits](https://docs.polymarket.com/quickstart/introduction/rate-limits)

## P1: Data Quality And Fill Reliability

### P1.1 Add heartbeat and stale-feed gating

Why:

1. Avoid trades on stale market or price streams.
2. Decrease false entries in temporary disconnects.

Sources:

1. [Heartbeats](https://docs.polymarket.com/developers/Utility-Endpoints/heartbeat)
2. [Data Feeds Best Practices](https://docs.polymarket.com/developers/CLOB/websocket/data-feeds)

### P1.2 Track and optimize queue outcomes by reason

You already have skip-reason counters:

- `crates/polyrust-strategies/src/crypto_arb/base.rs:1646`

Extend observability to include:

1. Post-only reject count.
2. Time-to-fill distribution by seconds-to-expiry bucket.
3. Cancel-before-fill ratio by bucket and coin.

This gives direct feedback for offset/depth policy tuning.

### P1.3 Add per-source freshness and disagreement telemetry

Track these metrics per coin and per minute:

1. Source lag (`now - source_timestamp`) for Binance/Chainlink/Coinbase.
2. Cross-source dispersion (max-min basis points).
3. Signal veto counts by reason (`stale`, `disagreement`, `insufficient_persistence`).

This is required to safely run external-feed frontrun logic in production.

## P2: Capital Efficiency

### P2.1 Add bounded adaptive sizing for TailEnd

Current fixed size is robust but blunt:

- `crates/polyrust-strategies/src/crypto_arb/tailend.rs:482`

Proposed behavior:

1. Scale by liquidity confidence and recent fill probability.
2. Keep hard min/max caps and per-market exposure limits.

Expected result:

1. Better Sharpe under constrained balance.
2. Fewer tiny low-edge fills and fewer oversized low-depth attempts.

## P3: Backtest Realism Upgrade (Needed Before Further Parameter Sweeps)

### P3.1 Introduce delayed/partial fill simulation

Today immediate fills make several parameters inert:

- `crates/polyrust-backtest/src/engine/mod.rs:1093`

Add:

1. Queue delay model.
2. Partial-fill outcomes based on available depth proxies.
3. Cancellation race conditions.

### P3.2 Preserve and evaluate reference quality in backtest

Currently forced to `Current`:

- `src/main.rs:612`

This blocks realistic evaluation of `min_reference_quality` and quality-weighted confidence behavior.

## Validation Protocol (Definition Of Done)

Treat this plan as complete only when all are true:

1. Out-of-sample month (`2026-02-01` to `2026-02-28`) shows non-degraded win rate and improved Sharpe vs current baseline.
2. Live paper run confirms lower reject rate and stable fill latency at equal or better PnL per trade.
3. New execution metrics exist for post-only rejects, fill delay, and cancel-before-fill.
4. Backtest realism upgrade produces non-inert response for at least one currently inert axis.

## Risks

1. Over-optimizing for maker rebates can reduce fills too much if post-only thresholds are too strict.
2. Tick-aware re-pricing can increase cancel churn and hit rate limits without throttling.
3. More complex sizing can increase tail risk if not hard-capped and monitored.

## Immediate Next Experiment Set

If starting now, run this sequence:

1. Add Coinbase `level2` feed and build composite external fair price (no trading logic changes yet).
2. Add freshness/persistence/disagreement gates and emit diagnostics only (shadow mode).
3. Enable external pre-trigger for entries with strict gates; keep current stop-loss behavior unchanged.
4. Enable external-assisted stop-loss confirmation path and compare false-positive exits.
5. Enable `postOnly` for TailEnd buys, then add tick-aware offset (`1 tick` vs `2 ticks`).
6. Switch backend `place_batch_orders` to real SDK `post_orders`.
7. Run paper/live shadow comparison for 3-5 days and record fill/reject/latency deltas.
8. Only then tune sizing and depth-factor dynamics.
