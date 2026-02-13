# TailEnd Stop-Loss Debug Report — 2026-02-13

## Trade Summary

| Field | Value |
|-------|-------|
| Market | ETH Up/Down 15m (`0x5a7ad0c...`) |
| Entry | 5.49 Up at 92¢ (GTC aggressive, 1 tick above 91¢ ask) |
| Exit | 5.48 Up at 92¢ (FOK stop-loss), actual CLOB fill ~93¢ |
| Reported PnL | **-$0.054** (wrong — see PnL bugs below) |
| Estimated real PnL | **~+$0.06** |
| Market outcome | **Up won** (settlement 2053.52 > reference 2049.63) |
| Verdict | **Should have held** — resolution at $1.00 would have netted ~+$0.44 |

---

## Why It Sold

**The trailing stop triggered because `trailing_min_distance` (0.05) > `trailing_distance` (0.03), making `time_decay` completely inert.**

### Config values (`config.rs:377-379`):
```
trailing_distance:     0.03   # base distance
trailing_min_distance: 0.05   # floor (widened from 0.01)
time_decay:            true   # enabled but INERT
```

### Effective distance calculation (`base.rs:1762-1778`):
```
eff = max(trailing_distance × decay_factor, trailing_min_distance)
    = max(0.03 × 44/900, 0.05)
    = max(0.0015, 0.05)
    = 0.05  ← floor always wins because floor > base
```

Since `0.03 × any_factor ≤ 0.03 < 0.05`, the floor always dominates. Time decay has **zero effect** on effective distance — it's pinned at 0.05 regardless of time remaining.

### Trigger sequence:
1. Peak bid reached **0.97**
2. Current bid dropped to **0.92**
3. `drop_from_peak = 0.97 - 0.92 = 0.05`
4. `0.05 >= effective_distance (0.05)` → **triggered**
5. FOK sell at current_bid = 0.92, filled at ~0.93
6. 44 seconds later: market resolved **Up** at $1.00

### Why the sell was wrong:
- With 44s left and Up at 92% probability, expected value of holding ≈ $0.92/share
- Expected value of selling at 0.92 minus taker fee ≈ $0.915/share
- The stop protected against downside that didn't materialize, at the cost of the entire upside
- The `min_remaining_secs` config exists (`base.rs:1710`) and was previously hardcoded to 60, but was set to 0 ("always active")

---

## PnL Bug #1: Charges taker fee on GTC maker entry

**Location**: `tailend.rs:949-951`

```rust
let pnl = (exit_price - pos.entry_price) * size
    - (pos.estimated_fee * size)   // ← charges taker fee on a MAKER entry
    - (exit_fee * size);
```

`estimated_fee` is computed at `tailend.rs:261` as `taker_fee(ask_price, rate)` during opportunity evaluation. But TailEnd entries use **GTC orders** (`tailend.rs:647`), which are maker orders with **0% fee**.

**Impact**: `taker_fee(0.91, 0.0315) = 0.00516/share × 5.48 = $0.028` phantom charge.

---

## PnL Bug #2: Uses trigger bid, not actual fill price

**Location**: `tailend.rs:941`

```rust
let exit_price = sl_info.exit_price;  // ← bid at trigger time (0.92)
```

The `on_order_filled` handler receives actual fill `price` from CLOB as a parameter, but ignores it for FOK stop-loss fills. It uses `sl_info.exit_price` (the bid when the stop triggered) instead of the actual FOK fill price.

Dashboard shows "Sold at 93¢" (from CLOB Filled event), but PnL uses 92¢ (trigger bid).

**Impact**: `(0.93 - 0.92) × 5.48 = $0.055` undercount in exit proceeds.

---

## PnL Breakdown

| Component | Computed (wrong) | Correct |
|-----------|-----------------|---------|
| Entry price | 0.92 | 0.92 (or 0.91 if CLOB gave price improvement) |
| Exit price | 0.92 (trigger bid) | 0.93 (actual FOK fill) |
| Entry fee | 0.00516/share (taker) | 0 (GTC = maker, 0% fee) |
| Exit fee | 0.00464/share | 0.00410/share (at 0.93) |
| **Reported PnL** | **-$0.054** | **~+$0.06** |

---

## Dust Retry Loop (already fixed)

After the FOK partial fill (5.48 of 5.49), 0.01 residual triggered a retry loop:
- 0.01 clamped to 0.0008 (on-chain balance) → API rejected "invalid amounts"
- Retried twice with escalating cooldowns (5s, 15s) before market expired

**Status**: Fixed by commit `861127e` (three-layer dust defense). Logs are from before deployment.

---

## Recommended Fixes

1. **Set `min_remaining_secs = 45`** in config — suppress stop-loss in final 45s (feature already exists at `base.rs:1710`, just needs non-zero config)
2. **Fix floor/base inversion** — set `trailing_min_distance < trailing_distance` so time_decay actually works (e.g., `trailing_distance=0.05, trailing_min_distance=0.01`)
3. **Fix PnL entry fee** — don't charge `estimated_fee` for GTC entries (add `entry_order_type` to position, zero out fee for maker)
4. **Fix PnL exit price** — use actual fill `price` from Filled event instead of `sl_info.exit_price` for FOK stop-loss fills
