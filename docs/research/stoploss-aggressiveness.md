# Stop-Loss Aggressiveness Research

> Date: 2026-02-15
> Context: Position lifecycle state machine implemented; calibrating SL parameters via backtest sweep

## Motivation

The tail-end strategy predicts market outcomes with 99.6% accuracy (4,132/4,149 correct), yet the realized win rate is only 93.6% (3,884/4,149 winning trades). The 6% gap — 265 losing trades — is almost entirely caused by **premature stop-loss exits**. The strategy correctly predicted the direction but exited the position before settlement.

A backtest sweep of the 4-level stop-loss trigger hierarchy revealed that only 2 of ~10 SL parameters actually fire:

| Parameter | Fires in Backtest? | Notes |
|-----------|--------------------|-------|
| `hard_drop_abs` | Yes | Absolute bid drop from entry (any time) |
| `post_entry_exit_drop` | Yes | Bid drop within post-entry window |
| `hard_reversal_pct` | No | Never triggers — price reversals too small in 15-min markets |
| `reversal_pct` | No | Dead — part of dual-trigger (never fires) |
| `min_drop` | No | Dead — dual-trigger minimum drop |
| `trailing_distance` | No | Dead — trailing stop never arms at high entry prices |
| `trailing_arm_distance` | No | Dead — trailing arm unreachable |
| `trailing_min_distance` | No | Dead — trailing minimum unreachable |
| `dual_trigger_consecutive` | No | Dead — dual-trigger condition never met |

This study investigates: **How aggressive should the two active SL parameters be?**

## Method

### Round 1: Coarse Grid (25 combos)

Swept `hard_drop_abs` and `post_entry_exit_drop` across {0.03, 0.05, 0.10, 0.50, 1.00} (5x5 grid). Timing params held at defaults (`sell_delay=8`, `exit_window=60`). No economic metrics.

**Result**: Every config with the same `min(hard_drop, exit_drop)` produced identical PnL. Appeared symmetric.

Data: `sweep_results/2026-02-15_14-00-18/`

### Round 2: Deep Grid with Timing Cross-Params (72 combos)

Asymmetric grid to test independence:
- `hard_drop_abs`: {0.03, 0.50, 1.00}
- `post_entry_exit_drop`: {0.03, 0.05, 0.10, 0.20, 0.50, 1.00}
- `min_sell_delay_secs`: {0, 8}
- `post_entry_window_secs`: {30, 60}

Added economic metrics: `premature_exit_cost` (PnL lost to false-positive SL exits) and `correct_stop_savings` (losses avoided by true-positive SL exits).

Data: `sweep_results/2026-02-15_14-34-57/`

### Round 3: Massive Fine-Grain + Reentry Validation (648 combos → 72 unique)

Fine-grain sweep of the Pareto sweet spot with shorter exit windows and reentry params:
- `hard_drop_abs`: {0.30, 0.50, 0.80}
- `post_entry_exit_drop`: {0.06, 0.08, 0.10, 0.12, 0.15, 0.20, 0.35, 0.50}
- `post_entry_window_secs`: {20, 30, 45}
- `min_sell_delay_secs`: {4}
- `recovery_max_set_cost`: {1.00, 1.02, 1.05}
- `reentry_cooldown_secs`: {0, 4, 8}

648 total combos, but reentry params produced identical results (`reentry_count=0` across all), collapsing to 72 unique configs. This confirmed reentry is dead in backtest (immediate fills = no FOK rejections = no recovery path).

Data: `sweep_results/2026-02-15_16-56-33/`

## Key Findings

### 1. The Two SL Parameters Are NOT Symmetric

Round 1 suggested symmetry, but Round 2 overturned this. Swapping `hard_drop_abs` and `exit_drop` values produces different results:

| Swap Pair | Config A PnL | Config B PnL | Difference |
|-----------|-------------|-------------|------------|
| hd=0.50, ed=1.0 vs hd=1.0, ed=0.50 | $361.62 | $339.20 | **$22.43** |
| hd=0.50, ed=0.03 vs hd=0.03, ed=0.50 | $310.08 | $296.21 | $13.87 |
| hd=1.0, ed=0.03 vs hd=0.03, ed=1.0 | $311.81 | $296.21 | $15.60 |

*All at sell_delay=0, exit_window=30*

**Why?** `hard_drop_abs` is a global trigger (fires at any time), while `post_entry_exit_drop` is gated by `post_entry_window_secs` (only fires within N seconds of entry). A tight `hard_drop=0.03` fires aggressively on any 3¢ drop at any point, but a tight `exit_drop=0.03` only fires during the post-entry window. So `hard_drop` being loose matters more — it's the less constrained trigger.

Mean PnL difference across 24 swap pairs: **$8.83**. Maximum: **$22.43**.

### 2. Effective Threshold Curve

Grouping by `eff_threshold = min(hard_drop, exit_drop)` at `exit_window=20` (Round 3 optimal):

| Eff. Threshold | PnL | Sharpe | Win Rate | SL Exits | Premature | Correct | Net SL |
|---------------|------|--------|----------|----------|-----------|---------|--------|
| 0.06 | $338.73 | 0.2072 | 97.1% | 157 | 141 | 16 | -$7.30 |
| 0.08 | $351.39 | 0.2138 | 97.8% | 131 | 115 | 16 | +$5.35 |
| 0.10 | $348.48 | 0.2020 | 98.0% | 122 | 106 | 16 | +$2.45 |
| 0.12 | $349.95 | 0.2023 | 98.1% | 118 | 102 | 16 | +$3.91 |
| 0.15 | $355.17 | 0.2002 | 98.3% | 109 | 93 | 16 | +$9.14 |
| 0.20 | $347.64 | 0.1865 | 98.4% | 107 | 91 | 16 | +$1.61 |
| 0.35 | $353.47 | 0.1744 | 98.6% | 96 | 81 | 15 | +$7.51 |
| 0.50 | $359.43 | 0.1774 | 98.7% | 93 | 78 | 15 | +$13.46 |

**Key observations:**
- With `exit_window=20`, SL becomes net positive at **threshold 0.08+** (vs 0.50 at ew=60)
- PnL peaks at threshold 0.15 ($355) or 0.50 ($359) depending on specific combo
- Sharpe peaks in the 0.08-0.12 range — a much higher threshold than Round 2's 0.03
- The sweet spot is 0.08-0.15: high PnL ($348-$368), strong Sharpe (0.20-0.23), SL net positive
- At threshold 0.06, SL is barely net negative (-$7.30) — much better than Round 2's -$50 at 0.03

### 3. Economic Analysis

Net SL value = `correct_stop_savings - premature_exit_cost`.

**Round 2** (at `sell_delay=8, exit_window=60` — worst case):

| Eff. Threshold | Premature Exits | Prem. Cost | Correct Stops | Stop Savings | **Net Value** | PnL |
|---------------|----------------|-----------|--------------|-------------|-----------|------|
| 0.03 | 289 | $195.00 | 17 | $144.86 | **-$50.13** | $295.64 |
| 0.05 | 219 | $180.62 | 17 | $144.70 | **-$35.91** | $310.25 |
| 0.10 | 160 | $178.58 | 17 | $135.59 | **-$42.99** | $303.18 |
| 0.20 | 115 | $148.48 | 17 | $113.93 | **-$34.56** | $311.61 |
| 0.50 | 81 | $71.69 | 17 | $87.15 | **+$15.46** | $361.62 |
| 1.00 | 74 | $29.24 | 3 | $28.28 | **-$0.96** | $344.81 |

**Round 3** (at `exit_window=20` — best case):

| Eff. Threshold | Premature Exits | Prem. Cost | Correct Stops | Stop Savings | **Net Value** | PnL |
|---------------|----------------|-----------|--------------|-------------|-----------|------|
| 0.06 | 141 | $120.53 | 16 | $113.24 | **-$7.30** | $338.73 |
| 0.08 | 115 | $107.88 | 16 | $113.24 | **+$5.35** | $351.39 |
| 0.10 | 106 | $101.99 | 16 | $104.44 | **+$2.45** | $348.48 |
| 0.12 | 102 | $100.31 | 16 | $104.23 | **+$3.91** | $349.95 |
| 0.15 | 93 | $93.20 | 16 | $102.34 | **+$9.14** | $355.17 |
| 0.20 | 91 | $89.58 | 16 | $91.19 | **+$1.61** | $347.64 |
| 0.50 | 78 | $54.41 | 15 | $67.87 | **+$13.46** | $359.43 |

**Key shift**: Shorter exit window (20s vs 60s) moves the net-positive crossover from 0.50 down to **0.08**. With `exit_window=20`, SL is net positive across nearly all thresholds.

### 4. Timing Interactions

#### sell_delay: 0 strictly better than 8

Averaged across all SL thresholds (Round 2):

| sell_delay | Mean PnL | Mean Sharpe | Mean Exits |
|-----------|---------|------------|-----------|
| 0 | $319.25 | 0.2226 | 215 |
| 8 | $318.57 | 0.2138 | 208 |

`sell_delay=0` wins on both PnL (+$0.68) and Sharpe (+0.0088). The 8-second delay doesn't help — when SL correctly fires, faster exit is better; when SL incorrectly fires, the loss is small either way since the position was going to win.

#### exit_window: 20s dominates (Round 3 finding)

Round 3 tested {20, 30, 45} and found a clear monotonic relationship:

| exit_window | Mean PnL | Mean Sharpe | Mean Exits | Mean Win Rate |
|------------|---------|------------|-----------|--------------|
| 20 | $348.06 | 0.1960 | 119 | 98.1% |
| 30 | $336.80 | 0.1994 | 133 | 97.7% |
| 45 | $322.30 | 0.1979 | 149 | 97.4% |

Shorter window = fewer false-positive exits = higher PnL (+$25 from 45→20). Sharpe is nearly flat across windows (0.196-0.199), so the PnL gain comes without meaningful Sharpe cost. **20s is the clear winner** — it constrains the exit_drop trigger to only the first 20 seconds after entry, when genuine bad entries are most visible.

#### hard_drop=0.50 confirmed best (Round 3)

Averaged across all exit_drop and exit_window values:

| hard_drop | Mean PnL | Mean Sharpe |
|----------|---------|------------|
| 0.30 | $325.88 | 0.2027 |
| 0.50 | $346.82 | 0.2136 |
| 0.80 | $334.45 | 0.1769 |

`hard_drop=0.50` wins on both PnL and Sharpe. 0.30 is too tight (too many false positives), 0.80 is too loose (misses real crashes, and when it does fire at extreme drops, the losses are larger).

#### Reentry params: confirmed dead in backtest (Round 3)

`recovery_max_set_cost` ({1.00, 1.02, 1.05}) and `reentry_cooldown_secs` ({0, 4, 8}) produced **identical results** across all 648 combos. `reentry_count=0` everywhere. No FOK rejections in backtest = no recovery path = reentry params are inert.

### 5. Pareto Front

**Round 3 Pareto front** (5 non-dominated configs across all 3 rounds):

| # | hard_drop | exit_drop | exit_window | PnL | Sharpe | Win Rate | Exits | Net SL |
|---|----------|----------|------------|------|--------|----------|-------|--------|
| 1 | 0.50 | 0.06 | 45 | $314.27 | 0.2397 | 95.8% | 212 | -$31.89 |
| 2 | 0.50 | 0.12 | 30 | $351.99 | 0.2371 | 97.8% | 133 | +$5.82 |
| 3 | 0.50 | 0.08 | 20 | $362.24 | 0.2288 | 97.9% | 128 | +$16.07 |
| 4 | 0.50 | 0.12 | 20 | $366.80 | 0.2249 | 98.2% | 114 | +$20.63 |
| 5 | 0.50 | 0.15 | 20 | $367.81 | 0.2166 | 98.4% | 106 | +$21.65 |

The Pareto front shifted significantly from Round 2: **3 of 5 configs use `exit_window=20`**, and all use `hard_drop=0.50`. The new sweet spot is `exit_drop=0.08-0.15` with `exit_window=20` — configs that were impossible to discover without testing sub-30s windows.

**Comparison with Round 2 Pareto:**
- Round 2 best balanced: $344.76 PnL, Sharpe 0.2317 (hd=0.50, ed=0.10, ew=30)
- Round 3 best balanced: $366.80 PnL, Sharpe 0.2249 (hd=0.50, ed=0.12, ew=20)
- Improvement: **+$22 PnL (+6.4%)** with only -0.007 Sharpe penalty

## Recommendations

### For Maximum PnL: Pareto #5

```toml
[arbitrage.stop_loss]
hard_drop_abs = 0.50

[arbitrage.tailend]
post_entry_exit_drop = 0.15
min_sell_delay_secs = 4
post_entry_window_secs = 20
```

Yields $367.81, 98.4% win rate, 106 SL exits (89 premature). SL is net positive (+$21.65). Best absolute PnL across all 3 rounds.

### For Maximum Sharpe: Pareto #1

```toml
[arbitrage.stop_loss]
hard_drop_abs = 0.50

[arbitrage.tailend]
post_entry_exit_drop = 0.06
min_sell_delay_secs = 4
post_entry_window_secs = 45
```

Highest Sharpe (0.2397) with reasonable PnL ($314.27). SL is net negative (-$31.89) but variance is well controlled.

### For Balanced (Recommended): Pareto #4

```toml
[arbitrage.stop_loss]
hard_drop_abs = 0.50

[arbitrage.tailend]
post_entry_exit_drop = 0.12
min_sell_delay_secs = 4
post_entry_window_secs = 20
```

Best risk-adjusted return: $366.80 PnL, Sharpe 0.2249, 98.2% win rate. SL is solidly net positive (+$20.63). Tight enough exit_drop catches genuine bad entries in the first 20 seconds, while loose hard_drop avoids later premature exits. **+$22 PnL improvement over Round 2's balanced pick** with minimal Sharpe cost.

### Fine-grain exit_drop at optimal combo (hd=0.50, ew=20)

| exit_drop | PnL | Sharpe | Win Rate | Net SL |
|----------|------|--------|----------|--------|
| 0.06 | $348.60 | 0.2207 | 97.2% | +$2.44 |
| 0.08 | $362.24 | 0.2288 | 97.9% | +$16.07 |
| 0.10 | $364.24 | 0.2233 | 98.1% | +$18.08 |
| **0.12** | **$366.80** | **0.2249** | **98.2%** | **+$20.63** |
| 0.15 | $367.81 | 0.2166 | 98.4% | +$21.65 |
| 0.20 | $361.45 | 0.2023 | 98.5% | +$15.29 |
| 0.35 | $361.17 | 0.1950 | 98.6% | +$15.00 |
| 0.50 | $361.62 | 0.1932 | 98.6% | +$15.46 |

The 0.08-0.15 band is the sweet spot. Below 0.08, too many false positives erode PnL. Above 0.20, the exit_drop trigger is so loose it's effectively disabled, and PnL plateaus at ~$361 (same as no exit_drop).

## Dead Code Candidates (Backtest Only)

The following SL parameters never fire during backtest on 15-minute markets. They may still provide value in live trading (different price dynamics, real orderbook depth, latency effects) and should be documented as **dead in backtest** rather than removed:

**Trigger params (never fire):**
- `hard_reversal_pct` — price reversals too small in 15-min window
- `reversal_pct` — dual-trigger reversal condition never met
- `min_drop` — dual-trigger minimum drop never reached
- `trailing_distance` — trailing stop never arms (entry prices too high, can't gain enough)
- `trailing_arm_distance` — trailing arm threshold unreachable
- `trailing_min_distance` — trailing minimum unreachable
- `dual_trigger_consecutive` — consecutive adverse ticks condition never met

**Reentry params (no path to trigger — confirmed Round 3):**
- `recovery_max_set_cost` — identical results at {1.00, 1.02, 1.05}
- `reentry_cooldown_secs` — identical results at {0, 4, 8}
- Root cause: immediate fills in backtest → no FOK rejections → RecoveryProbe path never entered

## Next Steps

### Short-term: Config Update (DONE)
- ~~Switch `sell_delay` from 8 to 0~~ → set to 4 (sweep default)
- ~~Set `hard_drop_abs=0.50`~~ → applied
- ~~Set `exit_drop=0.12, exit_window=20`~~ → applied (Round 3 balanced winner)

### Future Sweeps
1. **Ultra-fine exit_window**: Test {10, 12, 15, 18, 20, 22, 25} to find if sub-20s is even better, or if 20s is already optimal
2. **Entry-price stratification**: Split backtest by entry price band (0.90-0.95, 0.95-0.98, 0.98+) — SL may behave differently at different confidence levels
3. **Multi-objective ranking**: Weighted `0.5*Sharpe + 0.5*normalized_PnL` instead of single-metric sort
4. **Market-duration sensitivity**: Test whether findings hold on non-15-min markets
5. **Live vs backtest divergence**: Compare live reentry behavior to backtest predictions — reentry params may matter in live where FOK rejections occur

## Analysis Tools

- Sweep analysis script: `analysis/stoploss_research/analyze_sweep.py`
  - Usage: `uv run --with pandas --with numpy python analysis/stoploss_research/analyze_sweep.py [sweep_dir_name]`
  - Reads from `sweep_results/<dir>/results.csv`, defaults to latest sweep
  - Produces: symmetry test, threshold curve, timing interaction, economic analysis, Pareto front

## Sweep History

| Round | Date | Combos | Key Finding | Data |
|-------|------|--------|-------------|------|
| 1 | 2026-02-15 | 25 | Appeared symmetric (overturned in R2) | `sweep_results/2026-02-15_14-00-18/` |
| 2 | 2026-02-15 | 72 | NOT symmetric, Pareto front, sell_delay=0 better | `sweep_results/2026-02-15_14-34-57/` |
| 3 | 2026-02-15 | 648 (72 unique) | exit_window=20 dominates, exit_drop=0.12 optimal, reentry dead | `sweep_results/2026-02-15_16-56-33/` |
