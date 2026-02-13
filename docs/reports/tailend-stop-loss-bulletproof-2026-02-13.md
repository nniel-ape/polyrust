# TailEnd Stop-Loss Deep Research and Bulletproof Variant (2026-02-13)

## TL;DR
Current TailEnd stop-loss logic is good in structure (dual-trigger + trailing + rejection cooldowns), but it still has failure paths that can leave risk unmanaged in live trading. The most critical issue is that `post_entry_exit` is effectively disabled under default settings because `min_sell_delay_secs` is checked first.

The most robust variant is a **state-machine stop-loss** with:
1. Strict data-freshness gating.
2. Trigger hierarchy (hard crash override -> external+LOB confirmation -> dual-trigger -> trailing).
3. Execution ladder (FAK when available; otherwise depth-capped FOK -> short-lived GTC fallback).
4. Deferred fail-fast logic during settlement delay.
5. Explicit observability and circuit-breakers.

## Scope and Definition of Done
This review focused on TailEnd stop-loss behavior in live conditions (latency, microstructure, and fill uncertainty), not backtest-only behavior.

A “bulletproof” variant here means:
- Exits can still happen under stale/volatile conditions without runaway retries.
- No single feed or single orderbook tick can spuriously trigger an exit.
- Failure to fill degrades gracefully to partial risk reduction.
- Behavior is testable and observable with clear trigger reasons.

## Current Code Audit (What Is Solid)
- Dual-trigger logic (crypto reversal + market drop): `crates/polyrust-strategies/src/crypto_arb/base.rs:1730`.
- Trailing stop with time-decay and floor: `crates/polyrust-strategies/src/crypto_arb/base.rs:1756`.
- Liquidity-vs-balance rejection classification and cooldown escalation: `crates/polyrust-strategies/src/crypto_arb/base.rs:1927`.
- GTC fallback after liquidity rejection + stale GTC cancellation: `crates/polyrust-strategies/src/crypto_arb/base.rs:2003`, `crates/polyrust-strategies/src/crypto_arb/tailend.rs:719`.

## Critical Gaps (High Priority)
1. **Post-entry fail-fast is effectively disabled with defaults**
- `min_sell_delay_secs` check runs before post-entry logic: `crates/polyrust-strategies/src/crypto_arb/tailend.rs:773`.
- Post-entry window is 10s by default: `crates/polyrust-strategies/src/crypto_arb/config.rs:90`.
- Sell delay is 15s by default: `crates/polyrust-strategies/src/crypto_arb/config.rs:116`.
- Net effect: with defaults, the post-entry condition cannot fire.

2. **Stop-loss decisions do not enforce freshness on crypto input in `check_stop_loss`**
- Current logic reads latest crypto price without a staleness guard in the stop-loss path: `crates/polyrust-strategies/src/crypto_arb/base.rs:1731`.

3. **Full-size FOK stop-loss can repeatedly fail in thin books**
- `check_stop_loss` submits full position size (`pos.size`) for FOK/GTC: `crates/polyrust-strategies/src/crypto_arb/base.rs:1823`.
- This creates avoidable rejection loops when top-of-book depth is insufficient.

4. **Trailing default parameters are internally contradictory for time-decay intent**
- Default `trailing_distance = 0.03` and `trailing_min_distance = 0.05`: `crates/polyrust-strategies/src/crypto_arb/config.rs:377` and `crates/polyrust-strategies/src/crypto_arb/config.rs:379`.
- Because floor > base, effective trailing distance is often pinned to floor, reducing time-decay effect.

## External Research Inputs (Primary Sources)
1. **Polymarket order semantics + errors**
- Create Order docs explicitly define FOK/FAK/GTC/GTD and post-only constraints, including FOK rejection behavior and `FOK_ORDER_NOT_FILLED_ERROR`.
- Source: https://docs.polymarket.com/developers/CLOB/orders/create-order

2. **FAK support direction**
- Changelog documents FAK introduction and behavior (partial immediate fill, cancel remainder).
- Source: https://docs.polymarket.com/changelog/changelog

3. **Orderbook microstructure and execution probabilities**
- Cont, Stoikov, Talreja show that short-horizon execution and price-move probabilities are state-dependent on orderbook queues.
- Source (paper PDF): https://www.columbia.edu/~ww2040/orderbook.pdf

4. **OFI and market depth as key short-horizon risk drivers**
- Cont, Kukanov, Stoikov show short-interval price changes are strongly linked to order-flow imbalance and depth.
- Source (abstract): https://academic.oup.com/jfec/article-abstract/12/1/47/816163

5. **Stop-loss can improve return/volatility tradeoff in specific regimes**
- Kaminski & Lo: certain stop policies increase expected return and reduce volatility at longer sampling frequencies.
- Source: https://research.hhs.se/esploro/outputs/journalArticle/When-do-stop-loss-rules-stop-losses/991001480526106056

6. **Crypto-focused evidence for downside risk control**
- Empirical crypto studies show stop-loss overlays can improve Sharpe and reduce downside in volatile regimes.
- Source (JBEF PDF): https://bibliotecadigital.bcb.gob.bo/xmlui/bitstream/handle/123456789/1933/Stop-loss-rules-and-momentum-payoffs-in_2023_Journal-of-Behavioral-and-Exper.pdf?sequence=49&trk=public_post_comment-text
- Source (FRL open access): https://www.sciencedirect.com/science/article/pii/S1544612321004116

7. **Event freshness and microstructure drift are explicit in Polymarket feeds**
- CLOB market channel includes event timestamps and tick-size-change events near extreme prices.
- RTDS provides Binance + Chainlink timestamps for crypto prices.
- Sources:
  - https://docs.polymarket.com/developers/CLOB/websocket/market-channel
  - https://docs.polymarket.com/developers/RTDS/RTDS-crypto-prices

8. **Retry storms need bounded control**
- Polymarket throttles requests; retry loops should be bounded and stateful.
- Source: https://docs.polymarket.com/quickstart/introduction/rate-limits

## Bulletproof Stop-Loss Variant (TailEnd SL v3)

### 1) State Machine
Per-token stop-loss state:
- `Healthy`
- `ArmedPostEntry` (signal seen but sell delayed by settlement lock)
- `ExitPending` (order in-flight)
- `DegradedLiquidity` (liquidity failure, fallback ladder active)
- `Cooldown`

State transitions are deterministic and logged with reason codes.

### 2) Trigger Hierarchy (highest priority first)
1. **Hard crash override**
- Immediate risk-off if either:
  - market bid drop from entry >= `hard_drop_abs` OR
  - external price move against side >= `hard_reversal_pct` within `hard_window_ms`.
- Requires only one fresh source (Binance OR Chainlink) plus fresh local book.

2. **External pre-trigger + local confirmation**
- Pre-trigger on adverse external momentum / OFI proxy.
- Confirm with local CLOB deterioration (spread widening, bid depth collapse, or queue imbalance flip) before firing normal exit.

3. **Dual-trigger (existing, retained)**
- Keep crypto reversal + market drop logic, but add freshness checks and hysteresis (2 consecutive ticks).

4. **Trailing stop (existing, improved)**
- Arm only after minimum favorable excursion.
- Use coherent floor/base relationship (floor <= base).

5. **Post-entry fail-fast**
- If adverse move appears before tokens are sellable, arm deferred exit.
- Execute when `min_sell_delay_secs` elapses only if condition still valid.

### 3) Execution Ladder
1. **Preferred:** `FAK` stop-loss sell at aggressive but bounded price.
2. If FAK unavailable in your stack: **depth-capped FOK** (sell only executable size at top levels).
3. Residual size -> short-lived GTC with tight expiry/replace cycle.
4. If repeated liquidity rejection: reduce clip size geometrically and continue until below min tradable size, then mark dust and stop retrying.

### 4) Freshness and Data Guards
Before any stop decision, require:
- `orderbook_age_ms <= sl_max_book_age_ms`
- `external_age_ms <= sl_max_external_age_ms`
- If both external feeds available, dispersion <= `sl_max_cross_source_dispersion_bps`; otherwise degrade confidence.

### 5) Parameterization (practical defaults for TailEnd)
Suggested starting values (live-paper first):
- `hard_drop_abs = 0.08`
- `hard_reversal_pct = 0.006`
- `hard_window_ms = 2000`
- `sl_max_book_age_ms = 1200`
- `sl_max_external_age_ms = 1500`
- `dual_trigger_consecutive_ticks = 2`
- `trailing_distance = 0.03`
- `trailing_min_distance = 0.015` (must be <= trailing_distance)
- `post_entry_window_secs = 20`
- `min_sell_delay_secs = 10`
- `gtc_stop_loss_max_age_secs = 3`

## Implementation Delta (Repo-Level)
1. `crates/polyrust-strategies/src/crypto_arb/config.rs`
- Add stop-loss freshness and hard-stop fields.
- Add config validation for contradictory windows and trailing floor/base.

2. `crates/polyrust-strategies/src/crypto_arb/types.rs`
- Add stop-loss state enum + reason codes.

3. `crates/polyrust-strategies/src/crypto_arb/base.rs`
- Add SL v3 evaluator with trigger hierarchy and freshness checks.
- Add depth-capped sizing helper for exit clips.

4. `crates/polyrust-strategies/src/crypto_arb/tailend.rs`
- Replace linear stop check with state-machine transitions.
- Fix post-entry deferred execution under settlement delay.

5. `crates/polyrust-strategies/src/crypto_arb/tests.rs`
- Add targeted tests for deferred post-entry exit, stale-data suppression, depth-capped partial liquidation, and fallback ladder behavior.

## Validation Plan
1. Unit tests
- Trigger correctness, state transitions, and edge conditions.

2. Deterministic replay / paper-live
- Replay high-volatility windows with synthetic lag and missing ticks.

3. Runtime SLOs
- `p95 time_to_first_exit_action`
- `% exits blocked by stale data`
- `% residual risk after first stop attempt`
- `retry_count_distribution`

4. Safety checks
- No infinite retry loops.
- No exit decisions on stale snapshots.
- No post-entry dead zone when defaults are used.

## Bottom Line
The strongest practical upgrade is **not** one more threshold. It is a **stateful, freshness-aware exit engine** with a fallback execution ladder and explicit degradation behavior under liquidity stress.

This gives the best chance of “bulletproof” behavior in real TailEnd conditions where latency, queue dynamics, and partial fills dominate risk.
