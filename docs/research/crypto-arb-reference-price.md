# Crypto Arbitrage: Reference Price for 15-Minute Markets

Deep dive into how reference prices are captured, stored, and used in the crypto arbitrage strategy for Polymarket's short-duration Up/Down markets.

## 1. What Are 15-Minute Up/Down Markets?

Binary prediction markets on BTC, ETH, SOL, XRP that settle every 15 minutes:

- **"Up" wins** if end-price >= start-price over the 15-minute window
- **"Down" wins** if end-price < start-price
- Settlement via Chainlink Data Streams + Automation (automated on-chain)
- Each outcome is a separate ERC-1155 token priced $0.00–$1.00 (probability)

There is no traditional "strike price" — the reference IS the opening price of the 15-minute window.

## 2. Reference Price = Crypto Spot at Market Discovery

The strategy defines reference price as the **external crypto price at the moment a market is discovered**:

```rust
pub struct MarketWithReference {
    pub market: MarketInfo,
    pub reference_price: Decimal,        // Crypto price when market discovered
    pub reference_approximate: bool,     // True if buffered before price available
    pub discovery_time: DateTime<Utc>,
    pub coin: String,                    // "BTC", "ETH", "SOL", "XRP"
}
```

**Key property**: Reference price is **immutable** — once set, it never changes for that market.

## 3. Reference Price Capture Flow

```
DiscoveryFeed (Gamma API polling every 30s)
    │
    ├─→ MarketDiscovered event
    │
    ▼
Strategy: on_market_discovered()
    │
    ├─→ Extract coin from question ("Bitcoin Up or Down" → "BTC")
    ├─→ Check coin is in config
    │
    ├─ Price available in context? ─── YES ──→ Create MarketWithReference
    │                                           reference_price = current external price
    │                                           Activate immediately
    │
    └─ NO ──→ Buffer in pending_discovery
              Wait for first ExternalPrice event
              Then promote with that price as reference
```

### Source Priority

1. **Chainlink** — primary, preferred for settlement accuracy
2. **Binance** — fallback when Chainlink stale (>30s old)

```rust
// From price_feed.rs
let should_update = c.get(&symbol).is_none_or(|existing| {
    existing.source != "chainlink"
        || (timestamp - existing.timestamp) > chrono::Duration::seconds(30)
});
```

### Buffering When Price Unavailable

If market discovered before any crypto price arrives:

```rust
// Market held in pending_discovery: HashMap<String, MarketInfo>
self.pending_discovery.insert(coin, market.clone());

// Later, on first ExternalPrice for that coin:
if let Some(market) = self.pending_discovery.remove(symbol) {
    let mwr = MarketWithReference {
        reference_price: price,  // First price becomes reference
        ...
    };
    self.active_markets.insert(market.id.clone(), mwr);
}
```

## 4. Prediction: Current Price vs Reference

Simple directional comparison:

```rust
pub fn predict_winner(&self, current_price: Decimal) -> Option<OutcomeSide> {
    if current_price > self.reference_price {
        Some(OutcomeSide::Up)
    } else if current_price < self.reference_price {
        Some(OutcomeSide::Down)
    } else {
        None  // No signal when prices equal
    }
}
```

Example: BTC reference = $50,000
- Current $50,500 → **Up**
- Current $49,800 → **Down**
- Current $50,000 → **None** (no trade)

## 5. Confidence Model — Three Time Regimes

```rust
pub fn get_confidence(
    &self,
    current_price: Decimal,
    market_price: Decimal,      // Best ask of predicted winner
    time_remaining_secs: i64,
) -> Decimal
```

### Distance Metric

```
distance_pct = |current_price - reference_price| / reference_price
```

Example: BTC ref=$50k, current=$51k → distance = 2%

### Regime 1: Tail-End (<120s remaining, market >= 90%)

```
confidence = 1.0 (always maximum)
```

Near resolution with decisive pricing = highest certainty.

### Regime 2: Late Window (120–300s remaining)

```
base = distance_pct × 66
market_boost = 1.0 + (market_price - 0.50) × 0.5
confidence = min(1.0, base × market_boost)
```

Example: distance=2%, market=0.70
- base = 0.02 × 66 = 1.32
- boost = 1.0 + 0.20 × 0.5 = 1.10
- confidence = min(1.0, 1.452) = **1.0**

### Regime 3: Early Window (>300s remaining)

```
confidence = min(1.0, distance_pct × 50)
```

Example: distance=1% → confidence = 0.50
Example: distance=0.5% → confidence = 0.25

### Threshold Summary

| Parameter | Value |
|-----------|-------|
| Tail-end window | < 120s |
| Late window | 120–300s |
| Early window | > 300s |
| Tail-end market threshold | >= 0.90 |
| Distance multiplier (late) | 66× |
| Distance multiplier (early) | 50× |
| Market boost coefficient | 0.5× per 1% from midpoint |
| Confidence cap | 1.0 |
| Min confidence to trade | 0.50 |

## 6. Three Trading Modes

Evaluated in priority order — first match wins.

### Mode 1: TailEnd

**Trigger**: time < 120s AND predicted winner ask >= 0.90

```rust
// Buy predicted outcome at 90¢+, resolves to $1.00
profit_margin = 1.00 - ask_price  // e.g., $0.05 on 95¢ ask
confidence = 1.0                   // Always maximum
```

Reference price role: `predict_winner()` determines which outcome to buy.

### Mode 2: TwoSided

**Trigger**: up_ask + down_ask < $0.98

```rust
// Buy BOTH outcomes — one resolves to $1.00, other to $0.00
profit_margin = 1.00 - (up_ask + down_ask)  // e.g., $0.04 if combined = $0.96
confidence = 1.0
```

Reference price role: Not directly used — market is inefficient regardless of direction. Returns two opportunities (Up + Down) with equal share counts:

```rust
size = position_size / combined_ask  // Equal shares of each outcome
```

### Mode 3: Confirmed

**Trigger**: confidence >= 0.50 AND profit_margin >= threshold

```rust
let min_margin = if time_remaining < 300 {
    config.late_window_margin   // 0.02 (2¢)
} else {
    config.min_profit_margin    // 0.03 (3¢)
};
```

Reference price role: Both `predict_winner()` (direction) and `get_confidence()` (distance from reference) drive the decision.

## 7. Configuration Defaults

```rust
impl Default for ArbitrageConfig {
    fn default() -> Self {
        Self {
            coins: vec!["BTC", "ETH", "SOL", "XRP"],
            position_size: Decimal::new(5, 0),            // $5 per trade
            max_positions: 5,
            min_profit_margin: Decimal::new(3, 2),        // 3¢
            late_window_margin: Decimal::new(2, 2),       // 2¢
            stop_loss_reversal_pct: Decimal::new(5, 3),   // 0.5%
            stop_loss_min_drop: Decimal::new(5, 2),       // 5¢
            scan_interval_secs: 30,
            use_chainlink: true,
        }
    }
}
```

## 8. Edge Cases

| Scenario | Handling |
|----------|----------|
| Reference price = 0 | distance_pct = 0 → confidence = 0 → no trade |
| Current = reference | `predict_winner()` returns None → all modes skip |
| Missing orderbook | Ask = None → mode skipped, tries next |
| Price unavailable at discovery | Buffered in `pending_discovery`, promoted on first price |
| Market expired (time <= 0) | Returns empty vec, no evaluation |
| Chainlink stale (>30s) | Binance price used as fallback |

## 9. Settlement: Chainlink vs UMA

| Aspect | Chainlink | UMA Optimistic Oracle |
|--------|-----------|----------------------|
| Market type | Price-based (~20%) | Subjective/event (~80%) |
| Speed | Minutes (automated) | Hours (dispute window) |
| Mechanism | Data Streams + Automation | Bond + challenge |
| 15-min markets | Primary resolver | Fallback for disputes |

Chainlink Data Streams deliver timestamped prices; Automation triggers on-chain settlement at window close.

## 10. Fee Impact on Strategy

Dynamic taker fees introduced to curb latency arbitrage:

- **At 50% probability**: ~3.15% fee (highest — where arbitrage thrives)
- **Near 0% or 100%**: ~0% fee (lowest — outcomes near-certain)
- **Makers pay $0** — incentivizes limit orders
- **Rebate program**: 20% of taker fees redistributed daily to liquidity providers

Impact: Pure latency arbitrage at 50/50 is unprofitable (3.15% > typical 1-2% edge). Strategy must target:
- Tail-end plays (low fees at extremes)
- Two-sided arbitrage (fee-adjusted combined cost still < $1)
- High-confidence directional bets (large distance from reference)

## 11. Historical Context

Pre-fee era: Bots dominated 15-minute markets. One notable bot turned $313 into $414,000 in one month with 98% win rate, exploiting the 1-2 second lag between Binance spot prices and Chainlink oracle updates on Polymarket.

Current landscape: Maker rebates + taker fees shifted the edge toward market-making strategies rather than pure directional arbitrage.

## Key Files

| File | Role |
|------|------|
| `crates/polyrust-strategies/src/crypto_arb.rs` | Full strategy: reference capture, confidence model, three modes |
| `crates/polyrust-market/src/price_feed.rs` | RTDS Chainlink + Binance price streams |
| `crates/polyrust-market/src/discovery_feed.rs` | Slug-based market discovery, MarketInfo |
| `crates/polyrust-market/src/orderbook.rs` | mid_price, best_bid, best_ask calculations |
| `crates/polyrust-core/src/context.rs` | MarketDataState shared state |

## External References

- [Polymarket RTDS Crypto Prices](https://docs.polymarket.com/developers/RTDS/RTDS-crypto-prices)
- [Polymarket + Chainlink Partnership](https://www.prnewswire.com/news-releases/polymarket-partners-with-chainlink-to-enhance-accuracy-of-prediction-market-resolutions-302555123.html)
- [Dynamic Fees Curb Latency Arbitrage](https://www.financemagnates.com/cryptocurrency/polymarket-introduces-dynamic-fees-to-curb-latency-arbitrage-in-short-term-crypto-markets/)
- [Maker Rebates Program](https://docs.polymarket.com/polymarket-learn/trading/maker-rebates-program)
- [Arbitrage Bots on Polymarket](https://finance.yahoo.com/news/arbitrage-bots-dominate-polymarket-millions-100000888.html)
