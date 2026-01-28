# Crypto Arb Strategy Improvements Research

> Date: 2026-01-28
> Context: Dynamic taker fees introduced on 15-min crypto markets, intensifying bot competition

## Current State

The strategy covers three modes — Tail-End, Two-Sided, Confirmed — with quality-tracked reference pricing (boundary snapshots, historical fallback, current fallback), dual-trigger stop-loss, and SSE dashboard. The landscape has shifted materially since this was designed.

## Threat #1: Dynamic Taker Fees

Polymarket now charges **taker-only fees on 15-minute crypto markets**:

- ~3.15% at 50/50 odds (peak) — kills naive latency arb
- Drops toward 0% as price approaches 0 or 1
- Fees redistributed daily to makers via Maker Rebates Program
- Tail-End mode (buying at 90%+) barely affected (~0% fee)
- Confirmed mode at moderate prices heavily penalized

Fee approximation at price `p`:

```
fee ≈ 2 * p * (1 - p) * 0.0315
```

| Price (p) | Fee per share | Round-trip |
|-----------|--------------|------------|
| 0.50      | ~1.575¢      | ~3.15¢     |
| 0.70      | ~1.32¢       | ~2.64¢     |
| 0.80      | ~1.01¢       | ~2.02¢     |
| 0.90      | ~0.57¢       | ~1.13¢     |
| 0.95      | ~0.30¢       | ~0.60¢     |

Note: exact formula not publicly documented — verify from [exchange-fee-module](https://github.com/Polymarket/exchange-fee-module) contract source.

## Threat #2: Competition

- Top 10 wallets capture 80%+ of arb profits
- One bot turned $313 -> $414K/month on these exact markets
- Bid-ask spreads narrowed from 4.5% (2023) -> 1.2% (2025)
- Estimated $40M extracted by arb bots Apr 2024–Apr 2025
- Only 0.5% of users made >$1,000 profit

## 10 Improvements (Ranked by Impact)

### 1. Become a Maker, Not a Taker

**Impact: Critical** — Eliminates the #1 threat

Current strategy places FOK (Fill-or-Kill) taker orders. Switch to limit orders posted to the book — makers pay $0 fees and earn daily USDC rebates.

Implementation:
- Replace FOK orders with GTC limit orders at `best_ask - 0.01` (buying) or `best_bid + 0.01` (selling)
- Add order management: track open limits, cancel stale orders, handle partial fills
- For Tail-End mode: post limits at 0.90–0.95 range early, get filled as price rises
- Trade-off: execution uncertainty (may not fill), but fee savings are 3%+ at mid-prices

### 2. Fee-Aware Profit Calculation

**Impact: High** — Prevents unprofitable trades

Current `min_profit_margin` (3¢ early, 2¢ late) doesn't account for the dynamic fee curve.

```
real_margin = gross_margin - fee(entry_price) - fee(exit_price_estimate)
```

Current Confirmed mode's 3¢ margin is underwater at mid-range prices after fees.

### 3. Spike Detection / Momentum Triggers

**Impact: High** — New alpha source

Instead of continuous scanning every 30s, detect explosive spot moves in real-time:
- Monitor Binance/Coinbase WebSocket for sudden price jumps (>0.5% in <10s)
- Liquidation cascades / news events create temporary mispricings that exceed the fee threshold
- Key insight: post-fee arb is only profitable on large, fast moves — not gradual drift
- Filter: `|spot_delta| > fee_at_current_price + min_margin` before evaluating

### 4. Cross-Market / Multi-Outcome Arbitrage

**Impact: High** — Untapped in current strategy

Current Two-Sided mode only checks `up_ask + down_ask < 0.98`. Expand to:
- **Cross-window arb**: Same coin, adjacent 15-min windows — structural mispricings
- **Cross-coin correlation**: BTC pumps -> ETH/SOL likely follow. Buy ETH Up before the market catches up
- **Multi-leg arb across platforms**: Opinion mirrors many Polymarket markets with independent pricing

### 5. Adaptive Position Sizing (Kelly Criterion)

**Impact: Medium-High** — Better capital efficiency

Current fixed $5/trade ignores confidence levels. Use fractional Kelly:

```
kelly_fraction = (confidence * payout - (1 - confidence)) / payout
position_size = base_size * kelly_fraction * kelly_multiplier (0.25-0.5)
```

- High confidence (0.9+): size up to $10-20
- Moderate confidence (0.5-0.7): keep at $3-5
- Could 2-3x returns without additional risk

### 6. Maker Market-Making Mode (New 4th Strategy Mode)

**Impact: Medium-High** — Revenue diversification

Provide two-sided liquidity in quiet markets:
- Post buy at `mid - spread/2`, sell at `mid + spread/2`
- Earn spread + maker rebates
- Only in markets where reference price gives informational edge
- Inventory management: skew quotes based on current position

### 7. Batch Order API

**Impact: Medium** — Latency reduction

Polymarket now supports placing up to 15 orders per batch request. Current implementation places orders one at a time. Batch enables:
- Atomic multi-leg trades (Two-Sided mode: both sides in one call)
- Faster order updates for market-making mode
- Reduced API round-trips

### 8. Improved Stop-Loss with Trailing Stops

**Impact: Medium** — Better downside management

Current dual-trigger stop (0.5% reversal AND 5¢ market drop) is conservative but rigid. Add:
- **Trailing stop**: lock in profits as position moves in your favor
- **Time-decay stop**: tighten stops as expiration approaches (not just the 60s cutoff)
- **Volatility-adjusted stops**: widen during high-vol periods, tighten in calm markets

### 9. Historical Performance Tracking & Strategy Selection

**Impact: Medium** — Adapt to regime changes

Track per-mode win rates and P&L over rolling windows:
- If Confirmed mode is losing money post-fees -> auto-disable it
- If Tail-End is printing -> increase position size
- Seasonal patterns: crypto volatility clusters by time-of-day (US open, Asia open)
- Creates a meta-strategy that adapts to the fee regime

### 10. Multi-Source Price Oracle

**Impact: Low-Medium** — Marginal accuracy gains

Current Chainlink + Binance dual-source is good. Consider adding:
- Coinbase (used by many Polymarket MMs as reference)
- Aggregated VWAP across 3+ exchanges for robustness
- Order flow imbalance signals: large Binance market buys -> predict direction before price moves
- Weighted by exchange reliability and latency

## Priority Roadmap

| Phase | Changes | Expected Impact |
|-------|---------|-----------------|
| **P0 (Critical)** | Fee-aware margins (#2), spike detection (#3) | Prevent losses, find new alpha |
| **P1 (High)** | Maker orders (#1), Kelly sizing (#5) | 2-3x capital efficiency |
| **P2 (Medium)** | Batch API (#7), cross-market arb (#4) | New revenue streams |
| **P3 (Future)** | Market-making mode (#6), adaptive selection (#9) | Strategy diversification |

## Key Takeaway

The strategy must shift from taker to maker. The fee regime change fundamentally rewards liquidity provision over liquidity extraction. The reference pricing advantage (boundary snapshots, quality tracking) is a genuine edge — most MMs don't have it. Use that edge to post better-informed limit orders rather than crossing the spread.

## Sources

- [Polymarket Dynamic Fees (Finance Magnates)](https://www.financemagnates.com/cryptocurrency/polymarket-introduces-dynamic-fees-to-curb-latency-arbitrage-in-short-term-crypto-markets/)
- [Polymarket Trading Fees Docs](https://docs.polymarket.com/polymarket-learn/trading/fees)
- [Maker Rebates Program](https://docs.polymarket.com/polymarket-learn/trading/maker-rebates-program)
- [Exchange Fee Module (GitHub)](https://github.com/Polymarket/exchange-fee-module)
- [Arb Bots Dominate Polymarket (Yahoo Finance)](https://finance.yahoo.com/news/arbitrage-bots-dominate-polymarket-millions-100000888.html)
- [Polymarket HFT & AI (QuantVPS)](https://www.quantvps.com/blog/polymarket-hft-traders-use-ai-arbitrage-mispricing)
- [Automated Market Making on Polymarket](https://news.polymarket.com/p/automated-market-making-on-polymarket)
- [Polymarket Taker Fees (Cointelegraph)](https://cointelegraph.com/news/polymarket-quietly-adds-taker-fees-15-minute-crypto-markets)
- [Polymarket Taker Fee Implications (ainvest)](https://www.ainvest.com/news/polymarket-taker-fee-model-implications-liquidity-trading-dynamics-2601/)
- [Polymarket Taker Fees (The Block)](https://www.theblock.co/post/384461/polymarket-adds-taker-fees-to-15-minute-crypto-markets-to-fund-liquidity-rebates)
- [7 Polymarket Arbitrage Strategies (Medium)](https://medium.com/@danielelbisnero0714/7-polymarket-arbitrage-strategies-every-trader-should-know-1a278290272c)
- [Polymarket Market Making Guide (PolyTrack)](https://www.polytrackhq.app/blog/polymarket-market-making-guide)
