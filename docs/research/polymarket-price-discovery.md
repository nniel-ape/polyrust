# Polymarket Reference Price Discovery

Research on how Polymarket discovers, calculates, and uses reference prices across its prediction market infrastructure.

## 1. Displayed Price (What Users See)

Polymarket's displayed price = **midpoint of best bid and best ask** from the CLOB:

```
Price = (Best Bid + Best Ask) / 2
```

**Fallback rule**: When bid-ask spread exceeds $0.10, the platform shows **last trade price** instead of midpoint. This prevents outlier quotes from distorting the displayed price.

Prices are entirely **peer-to-peer market-driven** — Polymarket does not set prices. They emerge from trader limit orders. When a market launches, opening prices form once cumulative YES and NO share bids reach $1.00.

## 2. Three-Layer Price Architecture

| Layer | Source | Latency | Purpose |
|-------|--------|---------|---------|
| **CLOB WebSocket** | `wss://ws-subscriptions-clob.polymarket.com` | ~100ms | Real-time orderbook, trades, price changes |
| **RTDS WebSocket** | `wss://ws-live-data.polymarket.com` | ~100ms | External crypto spot prices (Binance + Chainlink) |
| **Gamma API** | `https://gamma-api.polymarket.com` | ~1s | Market discovery, metadata, liquidity snapshots |

## 3. RTDS (Real-Time Data Streams) — External Price Feeds

Two parallel crypto price streams, no auth required:

- **Binance source**: Direct exchange prices (`btcusdt`, `ethusdt`, `solusdt`, `xrpusdt`)
- **Chainlink source**: Aggregated oracle prices (`btc/usd`, `eth/usd`) — **higher priority**, used for settlement reference

### Connection Details

- WebSocket endpoint: `wss://ws-live-data.polymarket.com`
- Ping/Pong heartbeat every 5 seconds
- Supports dynamic subscribe/unsubscribe without reconnecting
- Each message includes: trading pair, Unix millisecond timestamp, current price

### Polyrust Implementation (`crates/polyrust-market/src/price_feed.rs`)

- Subscribes to both Chainlink and Binance streams simultaneously
- Chainlink preferred; Binance used as fallback when Chainlink data is stale (>30s)
- Symbol normalization: "BTCUSDT" → "BTC", "ETH/USD" → "ETH"
- Prices cached in `Arc<RwLock<HashMap<String, CachedPrice>>>` with timestamp + source
- Published as `Event::MarketData(MarketDataEvent::ExternalPrice)` events

```rust
ExternalPrice {
    symbol: String,     // e.g., "BTC"
    price: Decimal,     // USD price (e.g., 50000)
    source: String,     // "chainlink" or "binance"
    timestamp: DateTime<Utc>,
}
```

## 4. CLOB Orderbook Price Discovery

### WebSocket Market Channel Events

The CLOB WebSocket API (`wss://ws-subscriptions-clob.polymarket.com/ws/`) broadcasts:

| Event | Description |
|-------|-------------|
| `book` | Full L2 orderbook snapshot on subscribe + after every trade |
| `price_change` | Fires on order place/cancel; shows updated price levels |
| `last_trade_price` | Executed trade details when maker/taker orders match |
| `tick_size_change` | Dynamic tick size adjustments at price boundaries |
| `best_bid_ask` | Best quote updates (optional feature flag) |
| `new_market` | Newly created markets (optional feature flag) |
| `market_resolved` | Resolution announcement with winning outcome (optional feature flag) |

### Polyrust Implementation (`crates/polyrust-market/src/orderbook.rs`)

The `OrderbookManager` maintains live snapshots per token_id:

- `best_bid()` — highest bid price
- `best_ask()` — lowest ask price
- `mid_price()` — `(best_bid + best_ask) / 2` (or single side if other missing)
- `spread()` — `best_ask - best_bid`

Bids sorted descending (best first), asks sorted ascending (best first).

## 5. Reference Price in Crypto Arbitrage Strategy

The `CryptoArbitrageStrategy` (`crates/polyrust-strategies/src/crypto_arb.rs`) defines **reference price = crypto spot price at the moment a 15-minute market is discovered**:

```rust
pub struct MarketWithReference {
    pub market: MarketInfo,
    pub reference_price: Decimal,        // Crypto price when market discovered
    pub reference_approximate: bool,     // True if buffered before price available
    pub discovery_time: DateTime<Utc>,
    pub coin: String,
}
```

### Assignment Flow

1. `DiscoveryFeed` finds new 15-min market via slug-based Gamma API polling
2. Market buffered in `pending_discovery` until external price arrives
3. First `ExternalPrice` event promotes market to `active_markets` with that price as reference
4. Strategy continuously compares current crypto price vs reference to predict Up/Down winner

### Confidence Model

The strategy uses a multi-signal confidence model:

- **Distance** = `|current_price - reference_price| / reference_price` (percentage deviation)
- **Tail-end mode** (<2 min remaining, market >= 90%): confidence = 1.0
- **Late window** (2-5 min): distance-weighted with market probability boost
- **Early window** (>5 min): distance-weighted, lower base confidence

### Three Trading Modes

1. **TailEnd**: <2 min remaining, predicted winner ask >= 0.90 (high certainty)
2. **TwoSided**: Both outcome asks sum < $0.98 (guaranteed profit on one outcome)
3. **Confirmed**: Dynamic confidence model, standard directional trading

## 6. Market Discovery via Gamma API

The `DiscoveryFeed` (`crates/polyrust-market/src/discovery_feed.rs`) discovers new markets:

- **Slug-based lookup**: Constructs deterministic slugs like `btc-updown-15m-{unix_timestamp}`
- **Timestamp alignment**: Rounded to 15-minute boundaries
- **Fallback order**: Current window → next window → previous window
- **Polling interval**: Every 30 seconds (configurable)
- **Supported coins**: BTC, ETH, SOL, XRP

Returns `MarketInfo` with `condition_id`, `clob_token_ids`, `end_date`, `accepting_orders`, `neg_risk`.

## 7. Oracle Integration for Settlement

### Chainlink Data Streams + Automation (live since Sept 2025)

- Low-latency, timestamped oracle reports for price-based market resolution
- Chainlink Automation triggers on-chain settlement automatically
- Near-instantaneous resolution vs hours for subjective markets
- **Scope**: ~20% of markets (price-focused)

### UMA Optimistic Oracle

- Handles ~80% of markets (subjective/event-based outcomes)
- Human dispute resolution with economic incentives
- Longer settlement time but supports arbitrary resolution criteria

## 8. Dynamic Tick Sizes

| Price Range | Tick Size | Precision |
|-------------|-----------|-----------|
| [0.04, 0.96] | 0.01 | 1 cent |
| < 0.04 or > 0.96 | 0.001 | 0.1 cent |

Finer granularity at price extremes maintains market efficiency when outcomes are near-certain. A `tick_size_change` WebSocket event broadcasts when boundaries are crossed.

## 9. Maker Rebates Program (Liquidity Incentives)

Taker-only fees collected on 15-minute crypto markets, redistributed daily to makers:

- **Fee curve**: Highest at 50% probability (~3.15%), decreases toward 0% and 100%
- **Reward formula**: Considers participation, two-sided depth, and spread vs mid-market
- **Effect**: Incentivizes tight spreads, improving price discovery quality
- **Recent changes** (Jan 2026): 20% of collected taker fees redistributed daily to makers

## 10. Mid-Market Calculation Examples

### Example 1: Tight Spread
- Best Bid: $0.45, Best Ask: $0.55
- Spread: $0.10 (at threshold)
- Displayed price: ($0.45 + $0.55) / 2 = **$0.50**

### Example 2: Wide Spread (Fallback)
- Best Bid: $0.30, Best Ask: $0.60
- Spread: $0.30 (exceeds $0.10 threshold)
- Last trade: $0.42
- Displayed price: **$0.42** (last trade price)

### Example 3: One-Sided Book
- Best Bid: $0.65, No asks
- Displayed price: **$0.65** (only available side)

## 11. Shared State Architecture

`MarketDataState` (`crates/polyrust-core/src/context.rs`) provides thread-safe access:

```rust
pub struct MarketDataState {
    pub orderbooks: HashMap<TokenId, OrderbookSnapshot>,
    pub markets: HashMap<MarketId, MarketInfo>,
    pub external_prices: HashMap<String, Decimal>,  // "BTC" → 50000
}
```

Updated by the engine's context-update task:
- `ExternalPrice` events → `external_prices`
- `MarketDiscovered` events → `markets`
- `OrderbookUpdate` events → `orderbooks`

## 12. Paper Trading Price Matching

`PaperBackend` (`crates/polyrust-execution/src/paper.rs`) simulates fills using orderbook data:

- **Buy orders**: Fill if order price >= best ask
- **Sell orders**: Fill if order price <= best bid
- Supports partial fills based on available liquidity at each level
- Weighted average entry price tracking

## Key Files Reference

| File | Role |
|------|------|
| `crates/polyrust-market/src/price_feed.rs` | RTDS Chainlink + Binance price streams |
| `crates/polyrust-market/src/clob_feed.rs` | CLOB WebSocket orderbook subscription |
| `crates/polyrust-market/src/orderbook.rs` | OrderbookManager + mid_price/spread calculations |
| `crates/polyrust-market/src/discovery_feed.rs` | Slug-based market discovery via Gamma API |
| `crates/polyrust-strategies/src/crypto_arb.rs` | Reference price assignment + confidence model |
| `crates/polyrust-core/src/context.rs` | Shared MarketDataState (orderbooks, external_prices, markets) |
| `crates/polyrust-execution/src/paper.rs` | Paper trading fill simulation |

## External References

- [CLOB Introduction](https://docs.polymarket.com/developers/CLOB/introduction)
- [How Prices Are Calculated](https://docs.polymarket.com/polymarket-learn/trading/how-are-prices-calculated)
- [RTDS Overview](https://docs.polymarket.com/developers/RTDS/RTDS-overview)
- [RTDS Crypto Prices](https://docs.polymarket.com/developers/RTDS/RTDS-crypto-prices)
- [WebSocket Market Channel](https://docs.polymarket.com/developers/CLOB/websocket/market-channel)
- [Data Feeds for Market Makers](https://docs.polymarket.com/developers/market-makers/data-feeds)
- [Chainlink Partnership Announcement](https://www.prnewswire.com/news-releases/polymarket-partners-with-chainlink-to-enhance-accuracy-of-prediction-market-resolutions-302555123.html)
- [Maker Rebates Program](https://docs.polymarket.com/polymarket-learn/trading/maker-rebates-program)
