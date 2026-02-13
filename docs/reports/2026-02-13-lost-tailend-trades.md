# Debug Report: Two Lost TailEnd Trades (2026-02-13 14:30-14:45 UTC)

## Markets

Both are 15-min Up/Down markets, 9:30-9:45 AM ET (14:30-14:45 UTC):
- `https://polymarket.com/event/btc-updown-15m-1770993000`
- `https://polymarket.com/event/eth-updown-15m-1770993000`

| Market | Coin | Side | Entry Price | Size | USDC Spent | Polymarket Outcome |
|--------|------|------|-------------|------|------------|-------------------|
| `0x89dd96b1...` | BTC | Down | 0.99 | 5.05 | $5.00 | **Up (LOST)** |
| `0x26e8b858...` | ETH | Down | 0.93 | 5.43 | $5.05 | **Up (LOST)** |

**Total loss: ~$10.05**

Both orders placed at 14:43:00 UTC, ~2 minutes before market expiry at 14:45.

---

## Price Timeline (Binance 1m klines)

### BTC
```
14:30 (market start):  67311.85  ← reference price
14:40:                 67440.60  (volatile, ranging 67200-67600)
14:43 (ENTRY):         67159.49  ← -0.23% below ref → bot predicted Down
14:44:                 67331.47  ← back above reference
14:45 (EXPIRY):        67768.77  ← +0.68% above ref → Polymarket: UP

Bot's chainlink at 14:45: 67260.877 (-0.08% vs ref) ← STALE, shows below ref!
```

### ETH
```
14:30 (market start):  1975.68   ← reference price
14:40:                 1979.68
14:43 (ENTRY):         1968.76   ← -0.35% below ref → bot predicted Down
14:44:                 1984.00   ← surged above reference
14:45 (EXPIRY):        1997.09   ← +0.38% above ref → Polymarket: UP

Bot's coinbase at 14:45: 1983.125 (+0.38% vs ref)
```

**Both BTC and ETH rallied sharply in the final 2 minutes, flipping from below-reference to above-reference.**

---

## Root Cause 1: Entries Too Close to Reference

The TailEnd strategy entered when prices were barely below the reference:

| Guard | Threshold | BTC Actual | ETH Actual | Pass? |
|-------|-----------|------------|------------|-------|
| `min_strike_distance_pct` | ≥ 0.12% | 0.23% | 0.35% | YES |
| Dynamic ask threshold | ≥ 0.90 at ~120s | 0.99 | 0.92 | YES |
| All other guards | various | pass | pass | YES |

**Problem**: `min_strike_distance_pct = 0.12%` is far too tight.

- For BTC at $67k: 0.12% = only **$80** margin. A single candle moved +$609.
- For ETH at $1975: 0.12% = only **$2.37** margin. A single candle moved +$16.
- Both positions were entered with < 0.4% distance from reference, with only 2 minutes until expiry.

**Config location**: `crates/polyrust-strategies/src/crypto_arb/config.rs:159`
```rust
min_strike_distance_pct: Decimal::new(12, 4), // 0.0012 = 0.12%
```

---

## Root Cause 2: Stop-Loss Never Triggered

Stop-loss runs on every orderbook update for the position's token (`tailend.rs:705-813`).
Requires **dual trigger** (`base.rs:1785`):
```
(crypto_reversed AND market_dropped) OR trailing_triggered
```

### Critical: Entry uses composite price, stop-loss does NOT

The deployed config has `use_composite_price = true` (confirmed by `composite_stale=121` skip stat).

| Component | Price Source | Code |
|-----------|-------------|------|
| **Entry** | `composite_fair_price()` — weighted: binance-futures 50%, binance-spot 30%, coinbase 20% | `tailend.rs:471-492` |
| **Stop-loss** | `get_latest_price()` — single most-recent from ANY source (including stale chainlink) | `base.rs:1731` |

The entry correctly uses a robust multi-source composite. But the stop-loss falls back to `get_latest_price()` which just returns the last price pushed to `price_history` — potentially a stale chainlink update.

### BTC Down — Stop-Loss Analysis

**`crypto_reversed`**: `get_latest_price("BTC")` may have returned stale chainlink price (67260, -0.08% vs ref) instead of composite/binance (67331+, +0.03-0.68% above ref).

```
If chainlink was latest: reversal = (67260 - 67311) / 67311 = -0.08% → NEGATIVE → no reversal seen
If binance was latest:   reversal = (67331 - 67311) / 67311 = +0.03% → still < 0.50% threshold
If composite was used:   weighted avg of binance+coinbase would show BTC above reference
```

Feed lags from logs: `binance-spot=780ms, chainlink=464ms, coinbase=62ms, binance-futures=780ms`

The stop-loss likely saw BTC as still below or barely above reference — **never reaching the 0.50% reversal threshold**.

**`crypto_reversed` = FALSE** — stale/lagging feed source + high threshold = blind stop-loss.

**`market_dropped`**: `entry_price - bid ≥ 0.05` → needs bid ≤ 0.94. At 0.99 entry with bid likely at 0.97-0.98, the drop was < 0.05. **FALSE.**

**`trailing_triggered`**: Needs `peak_bid ≥ 0.99 + 0.05 = 1.04`. Impossible. **NEVER ARMED.**

### ETH Down — Stop-Loss Analysis

**`crypto_reversed`**: ETH peaked at 1984.00.
```
reversal = (1984.00 - 1975.68) / 1975.68 = 0.42%
threshold: reversal_pct = 0.50%
0.42% < 0.50% → FALSE
```
ETH was **$1.56 short** of the threshold (needed 1985.56, peaked at 1984.00).

**`market_dropped`**: `entry_price - bid ≥ 0.05` → needs bid ≤ 0.88. Bid at entry was 0.91, drop = 0.02. Even if it dropped further, condition 1 was FALSE. **Irrelevant.**

**`trailing_triggered`**: Needs `peak_bid ≥ 0.93 + 0.05 = 0.98`. Bid was falling, never reached 0.98. **NEVER ARMED.**

### Time Constraints

```
Entry:              14:43:00
min_sell_delay:     +15 seconds → stop-loss blocked until 14:43:15
Market expiry:      14:45:00
Protection window:  1 minute 45 seconds only
```

---

## Root Cause 3: Chainlink Boundary Stale for BTC

The bot's `on_market_expired` calculates P&L using `get_settlement_price()`:

```
Bot's chainlink boundary: 67260.877 (below reference 67311)
→ Bot thinks: 67260 < 67311 → BTC went DOWN → Down position WON

Polymarket's oracle: BTC was UP
→ Reality: Down position LOST, tokens worth 0
```

**Impact**: The bot may have incorrectly recorded the BTC trade as a win internally, while the on-chain tokens are worthless. The ClaimMonitor would attempt redemption and either get 0 or fail.

**Config location**: Boundary price capture prefers chainlink (`base.rs:474`):
```rust
source.eq_ignore_ascii_case("chainlink") && !existing.source.eq_ignore_ascii_case("chainlink")
```

---

## The Structural Gap

| Parameter | Value | Effect |
|-----------|-------|--------|
| `min_strike_distance_pct` (entry) | 0.12% | Allows entry when crypto is ~$2-80 from reference |
| `reversal_pct` (stop-loss) | 0.50% | Requires crypto to be ~$10-335 ABOVE reference |
| **Dead zone** | **0.62%** | Crypto can swing 0.61% with zero stop-loss protection |

In these trades, BTC swung 0.91% and ETH swung 0.81% — both within 2 minutes. The stop-loss couldn't react because the thresholds are mismatched.

---

## Proposed Fixes

### Fix 1: Widen `min_strike_distance_pct` (0.12% → 0.50%)

**File**: `config.rs:159`

Matches `reversal_pct` so the stop-loss can always fire before the outcome flips. At $67k BTC, requires $335 distance from reference instead of $80.

**Both trades would have been rejected**: BTC was 0.23% < 0.50%, ETH was 0.35% < 0.50%.

### Fix 2: Lower `reversal_pct` (0.50% → 0.25%)

**File**: `config.rs:374`

Makes the stop-loss trigger sooner. For ETH, 0.42% reversal would have exceeded the 0.25% threshold → stop-loss would have fired.

### Fix 3: Lower `min_drop` (0.05 → 0.02)

**File**: `config.rs:375`

At entry 0.93, triggers when bid drops to 0.91 (was 0.88). Combined with lower reversal_pct, both conditions of the dual trigger become reachable.

### Fix 4: Reduce `min_sell_delay_secs` (15 → 5)

**File**: `config.rs:158`

Gives 10 more seconds of stop-loss coverage. CLOB settlement is typically < 5s.

### Fix 5: Use composite price for stop-loss reversal check

**File**: `base.rs:1731` (`check_stop_loss`)

Replace `get_latest_price()` with `composite_fair_price()` — the same multi-source weighted average already used for entry evaluation. This ensures stop-loss sees the same quality of data as the entry decision.

```rust
// BEFORE (base.rs:1731):
let current_crypto = self.get_latest_price(&pos.coin).await;

// AFTER:
let current_crypto = if self.config.tailend.use_composite_price {
    self.composite_fair_price(
        &pos.coin, ctx,
        self.config.tailend.max_source_stale_secs,
        1,  // min_sources=1 for stop-loss (more lenient than entry)
        Decimal::MAX,  // no dispersion limit for stop-loss
    ).await.map(|r| r.price)
} else {
    self.get_latest_price(&pos.coin).await
};
```

Note: `check_stop_loss` doesn't currently take `ctx` — will need to add `StrategyContext` parameter or store a reference in `CryptoArbBase`.

---

## DB Observations

- **14 total trades**, 13 unique markets
- **12 markets**: Buy only, no Sell — winning = redeemed by ClaimMonitor, losing = expire worthless
- **1 market** (`0x5a7ad...`): Buy + Sell — only trade where stop-loss actually executed
- **Orders table empty** — orders not persisted
- **Events table empty** — audit trail not written
- **Fill rate: 50%** — half of GTC orders cancelled as stale before filling

---

## Verification Plan

1. `cargo test --workspace` — existing tests pass with new defaults
2. `cargo test -p polyrust-strategies -- stop_loss` — stop-loss tests
3. Monitor next session for "TailEnd skip: crypto too close to strike" (proof filter works)
4. Check that the feed source used in `get_latest_price` is the freshest available
