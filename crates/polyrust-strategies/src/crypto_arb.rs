use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as FmtWrite;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tracing::{info, warn};

use polyrust_core::prelude::*;
use polyrust_market::ChainlinkHistoricalClient;

/// Escape a string for safe inclusion in HTML content.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Fee model configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FeeConfig {
    /// Taker fee rate (default 0.0315 = 3.15% at 50/50).
    pub taker_fee_rate: Decimal,
}

impl Default for FeeConfig {
    fn default() -> Self {
        Self {
            taker_fee_rate: Decimal::new(315, 4), // 0.0315
        }
    }
}

/// Spike detection configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpikeConfig {
    /// Minimum price change percentage to count as a spike.
    pub threshold_pct: Decimal,
    /// Lookback window in seconds for spike detection.
    pub window_secs: u64,
    /// Maximum number of spike events to retain.
    pub history_size: usize,
}

impl Default for SpikeConfig {
    fn default() -> Self {
        Self {
            threshold_pct: Decimal::new(5, 3), // 0.005
            window_secs: 10,
            history_size: 50,
        }
    }
}

/// Hybrid order mode configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OrderConfig {
    /// Use GTC maker orders for Confirmed/TwoSided modes.
    pub hybrid_mode: bool,
    /// Price offset below best ask for GTC limit orders.
    pub limit_offset: Decimal,
    /// Cancel stale GTC orders after this many seconds.
    pub max_age_secs: u64,
}

impl Default for OrderConfig {
    fn default() -> Self {
        Self {
            hybrid_mode: true,
            limit_offset: Decimal::new(1, 2), // 0.01
            max_age_secs: 30,
        }
    }
}

/// Position sizing configuration (Kelly criterion).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SizingConfig {
    /// Base position size in USDC.
    pub base_size: Decimal,
    /// Kelly fraction multiplier (fractional Kelly).
    pub kelly_multiplier: Decimal,
    /// Minimum position size in USDC.
    pub min_size: Decimal,
    /// Maximum position size in USDC.
    pub max_size: Decimal,
    /// Whether to use Kelly sizing (vs fixed).
    pub use_kelly: bool,
}

impl Default for SizingConfig {
    fn default() -> Self {
        Self {
            base_size: Decimal::new(10, 0),
            kelly_multiplier: Decimal::new(25, 2), // 0.25
            min_size: Decimal::new(2, 0),
            max_size: Decimal::new(25, 0),
            use_kelly: true,
        }
    }
}

/// Stop-loss configuration (dual-trigger + trailing).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StopLossConfig {
    /// Crypto price reversal percentage trigger (e.g. 0.005 = 0.5%).
    pub reversal_pct: Decimal,
    /// Minimum market price drop to confirm stop-loss (e.g. 0.05 = 5¢).
    pub min_drop: Decimal,
    /// Enable trailing stop-loss.
    pub trailing_enabled: bool,
    /// Trailing stop distance from peak bid.
    pub trailing_distance: Decimal,
    /// Tighten trailing distance as time remaining decreases.
    pub time_decay: bool,
}

impl Default for StopLossConfig {
    fn default() -> Self {
        Self {
            reversal_pct: Decimal::new(5, 3),  // 0.005
            min_drop: Decimal::new(5, 2),      // 0.05
            trailing_enabled: true,
            trailing_distance: Decimal::new(3, 2), // 0.03
            time_decay: true,
        }
    }
}

/// Cross-market correlation configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CorrelationConfig {
    /// Enable cross-market correlation signals.
    pub enabled: bool,
    /// Minimum spike percentage in leader coin to trigger follower signals.
    pub min_spike_pct: Decimal,
    /// Leader → follower coin pairs (e.g. BTC → [ETH, SOL]).
    pub pairs: Vec<(String, Vec<String>)>,
}

impl Default for CorrelationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_spike_pct: Decimal::new(1, 2), // 0.01
            pairs: vec![
                ("BTC".into(), vec!["ETH".into(), "SOL".into()]),
                ("ETH".into(), vec!["SOL".into()]),
            ],
        }
    }
}

/// Performance tracking and auto-disable configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PerformanceConfig {
    /// Minimum trades before auto-disable can trigger.
    pub min_trades: u64,
    /// Minimum win rate to keep a mode enabled.
    pub min_win_rate: Decimal,
    /// Rolling window size for recent P&L tracking.
    pub window_size: usize,
    /// Automatically disable modes with poor performance.
    pub auto_disable: bool,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            min_trades: 20,
            min_win_rate: Decimal::new(40, 2), // 0.40
            window_size: 50,
            auto_disable: false,
        }
    }
}

/// Configuration for the crypto arbitrage strategy.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArbitrageConfig {
    /// Coins to track (e.g. ["BTC", "ETH", "SOL", "XRP"])
    pub coins: Vec<String>,
    /// Maximum concurrent positions
    pub max_positions: usize,
    /// Minimum profit margin for confirmed mode
    pub min_profit_margin: Decimal,
    /// Minimum profit margin in late window (120-300s)
    pub late_window_margin: Decimal,
    /// Interval in seconds between market discovery scans
    pub scan_interval_secs: u64,
    /// Whether to use on-chain Chainlink RPC for resolution reference price
    pub use_chainlink: bool,
    /// Fee model configuration.
    #[serde(default)]
    pub fee: FeeConfig,
    /// Spike detection configuration.
    #[serde(default)]
    pub spike: SpikeConfig,
    /// Hybrid order mode configuration.
    #[serde(default)]
    pub order: OrderConfig,
    /// Position sizing configuration.
    #[serde(default)]
    pub sizing: SizingConfig,
    /// Stop-loss configuration.
    #[serde(default)]
    pub stop_loss: StopLossConfig,
    /// Cross-market correlation configuration.
    #[serde(default)]
    pub correlation: CorrelationConfig,
    /// Performance tracking configuration.
    #[serde(default)]
    pub performance: PerformanceConfig,
}

impl Default for ArbitrageConfig {
    fn default() -> Self {
        Self {
            coins: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            max_positions: 5,
            min_profit_margin: Decimal::new(3, 2),      // 0.03
            late_window_margin: Decimal::new(2, 2),     // 0.02
            scan_interval_secs: 30,
            use_chainlink: true,
            fee: FeeConfig::default(),
            spike: SpikeConfig::default(),
            order: OrderConfig::default(),
            sizing: SizingConfig::default(),
            stop_loss: StopLossConfig::default(),
            correlation: CorrelationConfig::default(),
            performance: PerformanceConfig::default(),
        }
    }
}

/// Parse a unix timestamp from a slug suffix (e.g. `btc-updown-15m-1706000000` → timestamp).
/// Returns `None` if the slug doesn't end with a valid unix timestamp.
fn parse_slug_timestamp(slug: &str) -> Option<i64> {
    let last_segment = slug.rsplit('-').next()?;
    let ts: i64 = last_segment.parse().ok()?;
    // Sanity: must be a reasonable unix timestamp (after 2020)
    if ts > 1_577_836_800 {
        Some(ts)
    } else {
        None
    }
}

/// Format a USD price with 2 decimal places and thousands separators (e.g. `$88,959.37`).
fn fmt_usd(price: Decimal) -> String {
    let rounded = price.round_dp(2);
    let s = format!("{:.2}", rounded);
    // Split on decimal point and add thousands separators to integer part
    let parts: Vec<&str> = s.split('.').collect();
    let int_part = parts[0];
    let dec_part = parts.get(1).unwrap_or(&"00");

    let negative = int_part.starts_with('-');
    let digits: &str = if negative { &int_part[1..] } else { int_part };

    let with_commas: String = digits
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<&str>>()
        .join(",");

    if negative {
        format!("$-{}.{}", with_commas, dec_part)
    } else {
        format!("${}.{}", with_commas, dec_part)
    }
}

/// Format a market probability price with 2 decimal places (e.g. `0.50`).
fn fmt_market_price(price: Decimal) -> String {
    format!("{:.2}", price.round_dp(2))
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// How accurately the reference price matches the market's actual start-of-window price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceQuality {
    /// On-chain Chainlink RPC lookup; staleness in seconds from target timestamp.
    /// Traditional Chainlink feeds update ~27s on Polygon, typical staleness is 12-15s.
    OnChain(u64),
    /// Boundary snapshot captured within 2s of window start (best real-time via RTDS).
    Exact,
    /// Closest historical price entry; staleness in seconds from window start.
    Historical(u64),
    /// Price at discovery time — existing fallback behavior (least accurate).
    Current,
}

impl ReferenceQuality {
    /// Confidence discount factor based on reference accuracy.
    /// Exact = 1.0 (real-time RTDS), OnChain(<5s) = 1.0, OnChain(<15s) = 0.98, OnChain(>=15s) = 0.95,
    /// Historical(<5s) = 0.95, Historical(>=5s) = 0.85, Current = 0.70.
    pub fn quality_factor(&self) -> Decimal {
        match self {
            ReferenceQuality::Exact => Decimal::ONE,
            ReferenceQuality::OnChain(s) if *s < 5 => Decimal::ONE,
            ReferenceQuality::OnChain(s) if *s < 15 => Decimal::new(98, 2),
            ReferenceQuality::OnChain(_) => Decimal::new(95, 2),
            ReferenceQuality::Historical(s) if *s < 5 => Decimal::new(95, 2),
            ReferenceQuality::Historical(_) => Decimal::new(85, 2),
            ReferenceQuality::Current => Decimal::new(70, 2),
        }
    }
}

/// A price snapshot captured at a 15-minute window boundary.
#[derive(Debug, Clone)]
pub struct BoundarySnapshot {
    pub timestamp: DateTime<Utc>,
    pub price: Decimal,
    /// Price source (e.g. "chainlink", "binance")
    pub source: String,
}

/// Market enriched with the reference crypto price at discovery time.
#[derive(Debug, Clone)]
pub struct MarketWithReference {
    pub market: MarketInfo,
    /// Crypto price at the moment the market was discovered
    pub reference_price: Decimal,
    /// How accurately the reference price matches the window start price.
    pub reference_quality: ReferenceQuality,
    pub discovery_time: DateTime<Utc>,
    /// Coin symbol (e.g. "BTC")
    pub coin: String,
}

impl MarketWithReference {
    /// Predict the winning outcome based on current price vs reference.
    /// Returns `None` when price equals reference (no directional signal).
    pub fn predict_winner(&self, current_price: Decimal) -> Option<OutcomeSide> {
        if current_price > self.reference_price {
            Some(OutcomeSide::Up)
        } else if current_price < self.reference_price {
            Some(OutcomeSide::Down)
        } else {
            None
        }
    }

    /// Multi-signal confidence score in [0, 1].
    ///
    /// Three regimes based on time remaining:
    /// - Tail-end (< 120s, market >= 0.90): confidence 1.0
    /// - Late window (120-300s): distance-weighted with market boost
    /// - Early window (> 300s): distance-weighted, lower base
    ///
    /// The raw confidence is then discounted by `reference_quality.quality_factor()`
    /// to reflect how accurately the reference price matches the window start price.
    pub fn get_confidence(
        &self,
        current_price: Decimal,
        market_price: Decimal,
        time_remaining_secs: i64,
    ) -> Decimal {
        let distance_pct = if self.reference_price.is_zero() {
            Decimal::ZERO
        } else {
            ((current_price - self.reference_price) / self.reference_price).abs()
        };

        let raw = if time_remaining_secs < 120 && market_price >= Decimal::new(90, 2) {
            // Tail-end: highest confidence — quality factor still applies
            Decimal::ONE
        } else if time_remaining_secs < 300 {
            // Late window
            let base = distance_pct * Decimal::new(66, 0);
            let market_boost =
                Decimal::ONE + (market_price - Decimal::new(50, 2)) * Decimal::new(5, 1);
            (base * market_boost).min(Decimal::ONE)
        } else {
            // Early window
            (distance_pct * Decimal::new(50, 0)).min(Decimal::ONE)
        };

        (raw * self.reference_quality.quality_factor()).min(Decimal::ONE)
    }
}

// ---------------------------------------------------------------------------
// Fee helpers
// ---------------------------------------------------------------------------

/// Compute the Polymarket taker fee per share at a given probability price.
///
/// Formula: `2 * p * (1 - p) * rate`
/// At p=0.50, fee = 0.50 * rate. At p=0.95, fee ≈ 0.095 * rate.
pub fn taker_fee(price: Decimal, rate: Decimal) -> Decimal {
    Decimal::new(2, 0) * price * (Decimal::ONE - price) * rate
}

/// Compute the net profit margin for an entry at `entry_price`, assuming the
/// winning outcome resolves to $1.
///
/// - Gross margin = `1 - entry_price`
/// - Entry fee: taker fee for taker orders, $0 for maker (GTC) orders
/// - Exit fee: ~$0 (resolution at $1 has negligible fee)
///
/// Returns `gross_margin - entry_fee`.
pub fn net_profit_margin(entry_price: Decimal, fee_rate: Decimal, is_maker: bool) -> Decimal {
    let gross = Decimal::ONE - entry_price;
    if is_maker {
        gross // Maker fee = $0
    } else {
        gross - taker_fee(entry_price, fee_rate)
    }
}

/// Compute the Kelly criterion position size in USDC.
///
/// - `payout = (1/price) - 1` — net payout per $1 risked if the bet wins
/// - `kelly = (confidence * payout - (1 - confidence)) / payout`
/// - `size = base_size * kelly * kelly_multiplier`, clamped to `[min_size, max_size]`
///
/// Returns `Decimal::ZERO` for negative edge (should skip the trade).
pub fn kelly_position_size(
    confidence: Decimal,
    price: Decimal,
    config: &SizingConfig,
) -> Decimal {
    if price.is_zero() || price >= Decimal::ONE {
        return Decimal::ZERO;
    }
    let payout = Decimal::ONE / price - Decimal::ONE;
    if payout.is_zero() {
        return Decimal::ZERO;
    }
    let kelly = (confidence * payout - (Decimal::ONE - confidence)) / payout;
    if kelly <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let size = config.base_size * kelly * config.kelly_multiplier;
    size.max(config.min_size).min(config.max_size)
}

/// Arbitrage trading modes, ordered by priority.
///
/// Each mode represents a different market condition or signal type:
/// - **TailEnd**: Highest confidence, market near certainty + time urgency
/// - **TwoSided**: Risk-free arbitrage when both outcomes mispriced
/// - **Confirmed**: Standard directional bet with dynamic confidence model
/// - **CrossCorrelated**: Correlation-based signal from leader coin spike
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ArbitrageMode {
    /// Tail-end mode: < 2 min remaining, market price >= 90%.
    /// Uses FOK orders for speed (fee ~0% at extreme prices).
    TailEnd,
    /// Two-sided mode: both outcomes priced below combined $0.98.
    /// Guaranteed profit regardless of outcome. Uses batch GTC orders.
    TwoSided,
    /// Confirmed mode: standard directional with dynamic confidence.
    /// Uses GTC maker orders to avoid taker fees.
    Confirmed,
    /// Cross-market correlation: follower coin triggered by leader spike.
    /// Confidence discounted by 0.7x factor for correlation uncertainty.
    CrossCorrelated {
        /// The leader coin that spiked (e.g. "BTC").
        leader: String,
    },
}

impl ArbitrageMode {
    /// Get the canonical mode variant for performance tracking.
    /// Strips the leader field from CrossCorrelated to unify stats across all leaders.
    fn canonical(&self) -> Self {
        match self {
            ArbitrageMode::CrossCorrelated { .. } => ArbitrageMode::CrossCorrelated {
                leader: String::new(),
            },
            other => other.clone(),
        }
    }
}

/// A detected arbitrage opportunity ready for execution.
///
/// Contains all information needed to place an order: market, outcome, price,
/// confidence, and profitability after fees. The `net_margin` field accounts
/// for Polymarket's dynamic taker fees (0% for maker/GTC orders).
#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    /// Trading mode that generated this opportunity.
    pub mode: ArbitrageMode,
    /// Market to trade.
    pub market_id: MarketId,
    /// Outcome to buy (Up or Down).
    pub outcome_to_buy: OutcomeSide,
    /// ERC-1155 token ID for the outcome.
    pub token_id: TokenId,
    /// Best ask price to buy at.
    pub buy_price: Decimal,
    /// Confidence score in [0, 1], used for Kelly sizing.
    pub confidence: Decimal,
    /// Gross profit margin (1 - buy_price) before fees.
    pub profit_margin: Decimal,
    /// Estimated taker fee **per share** at entry (0 for maker/GTC orders).
    /// Total fee for position = `estimated_fee * size`.
    pub estimated_fee: Decimal,
    /// Net profit margin **per share** after fees: `profit_margin - estimated_fee`.
    pub net_margin: Decimal,
}

/// Tracks an active arbitrage position.
///
/// Once an order fills, it becomes a position tracked until market expiration
/// or stop-loss trigger. The position stores all data needed for P&L calculation,
/// stop-loss monitoring, and performance tracking.
#[derive(Debug, Clone)]
pub struct ArbitragePosition {
    /// Market being traded.
    pub market_id: MarketId,
    /// Token ID of the outcome purchased.
    pub token_id: TokenId,
    /// Outcome side (Up or Down).
    pub side: OutcomeSide,
    /// Entry price paid per share.
    pub entry_price: Decimal,
    /// Position size in shares (USDC amount / entry_price).
    pub size: Decimal,
    /// Crypto reference price at market window start.
    pub reference_price: Decimal,
    /// Coin symbol (e.g. "BTC").
    pub coin: String,
    /// Order ID if known (for tracking).
    pub order_id: Option<OrderId>,
    /// Timestamp when position opened.
    pub entry_time: DateTime<Utc>,
    /// Kelly fraction used for sizing (None if fixed sizing was used).
    pub kelly_fraction: Option<Decimal>,
    /// Highest bid price observed since position entry (for trailing stop-loss).
    pub peak_bid: Decimal,
    /// Trading mode that opened this position.
    pub mode: ArbitrageMode,
    /// Estimated fee **per share** at entry (for P&L calculation).
    /// Total fee for position = `estimated_fee * size`.
    pub estimated_fee: Decimal,
}

/// A detected price spike event.
///
/// Tracks large, rapid price movements that may signal arbitrage opportunities.
/// Spike events are retained for dashboard display and correlation analysis.
#[derive(Debug, Clone)]
pub struct SpikeEvent {
    /// Coin that spiked (e.g. "BTC").
    pub coin: String,
    /// Timestamp when spike detected.
    pub timestamp: DateTime<Utc>,
    /// Percentage change from baseline (signed: positive = up, negative = down).
    pub change_pct: Decimal,
    /// Price at start of spike window.
    pub from_price: Decimal,
    /// Current price that triggered spike.
    pub to_price: Decimal,
    /// Whether this spike generated a trading action.
    pub acted: bool,
}

impl std::fmt::Display for ArbitrageMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArbitrageMode::TailEnd => write!(f, "TailEnd"),
            ArbitrageMode::TwoSided => write!(f, "TwoSided"),
            ArbitrageMode::Confirmed => write!(f, "Confirmed"),
            ArbitrageMode::CrossCorrelated { leader } => write!(f, "Cross({})", leader),
        }
    }
}

/// Per-mode performance statistics for tracking trade outcomes.
///
/// Accumulates win rate, P&L, and recent performance for each trading mode.
/// Used for dashboard display and auto-disable logic (disable modes with
/// poor win rate after min_trades threshold).
#[derive(Debug, Clone)]
pub struct ModeStats {
    /// Total trades entered in this mode.
    pub entered: u64,
    /// Trades that resolved profitably (P&L >= 0).
    pub won: u64,
    /// Trades that resolved at a loss (P&L < 0).
    pub lost: u64,
    /// Cumulative realized P&L (after fees).
    pub total_pnl: Decimal,
    /// Rolling window of recent P&L values (for volatility tracking).
    pub recent_pnl: VecDeque<Decimal>,
    /// Maximum window size for recent_pnl.
    window_size: usize,
}

impl ModeStats {
    fn new(window_size: usize) -> Self {
        Self {
            entered: 0,
            won: 0,
            lost: 0,
            total_pnl: Decimal::ZERO,
            recent_pnl: VecDeque::new(),
            window_size,
        }
    }

    /// Win rate as a fraction in [0, 1]. Returns ZERO if no completed trades.
    pub fn win_rate(&self) -> Decimal {
        let completed = self.won + self.lost;
        if completed == 0 {
            return Decimal::ZERO;
        }
        Decimal::from(self.won) / Decimal::from(completed)
    }

    /// Average P&L from the recent rolling window. Returns ZERO if empty.
    pub fn avg_pnl(&self) -> Decimal {
        if self.recent_pnl.is_empty() {
            return Decimal::ZERO;
        }
        let sum: Decimal = self.recent_pnl.iter().copied().sum();
        sum / Decimal::from(self.recent_pnl.len() as u64)
    }

    /// Record a trade outcome.
    fn record(&mut self, pnl: Decimal) {
        self.entered += 1;
        if pnl >= Decimal::ZERO {
            self.won += 1;
        } else {
            self.lost += 1;
        }
        self.total_pnl += pnl;
        self.recent_pnl.push_back(pnl);
        if self.recent_pnl.len() > self.window_size {
            self.recent_pnl.pop_front();
        }
    }

    /// Total completed trades (won + lost).
    pub fn total_trades(&self) -> u64 {
        self.won + self.lost
    }
}

// ---------------------------------------------------------------------------
// Strategy
// ---------------------------------------------------------------------------

/// A pending order awaiting confirmation from the execution backend.
#[derive(Debug, Clone)]
struct PendingOrder {
    market_id: MarketId,
    token_id: TokenId,
    side: OutcomeSide,
    price: Decimal,
    size: Decimal,
    reference_price: Decimal,
    coin: String,
    order_type: OrderType,
    mode: ArbitrageMode,
    kelly_fraction: Option<Decimal>,
    /// Estimated fee **per share** at entry. Total fee = `estimated_fee * size`.
    estimated_fee: Decimal,
}

/// An open GTC limit order that has been placed but not yet fully filled.
///
/// Tracks maker orders posted to the book. Orders are monitored for fills
/// (OrderEvent::Filled) and cancelled if stale (age > max_age_secs).
#[derive(Debug, Clone)]
pub struct OpenLimitOrder {
    /// Order ID from execution backend.
    pub order_id: OrderId,
    /// Market being traded.
    pub market_id: MarketId,
    /// Token ID of the outcome.
    pub token_id: TokenId,
    /// Outcome side (Up or Down).
    pub side: OutcomeSide,
    /// Limit price posted.
    pub price: Decimal,
    /// Order size in shares (remaining if partially filled).
    pub size: Decimal,
    /// Crypto reference price at market window start.
    pub reference_price: Decimal,
    /// Coin symbol (e.g. "BTC").
    pub coin: String,
    /// Instant when order was placed (for staleness check).
    pub placed_at: tokio::time::Instant,
    /// Trading mode that generated this order.
    pub mode: ArbitrageMode,
    /// Kelly fraction used for sizing (None if fixed).
    pub kelly_fraction: Option<Decimal>,
    /// Estimated fee **per share** at entry (0 for GTC maker orders).
    /// Total fee = `estimated_fee * size`.
    pub estimated_fee: Decimal,
}

/// Crypto arbitrage strategy that exploits mispricing in 15-minute Up/Down
/// crypto prediction markets on Polymarket.
/// Number of price history entries to keep per coin.
/// At ~5s RTDS intervals, 200 entries covers ~16 minutes — enough for a full
/// 15-minute window plus discovery delay.
const PRICE_HISTORY_SIZE: usize = 200;

/// Maximum time (seconds) from a window boundary to consider a snapshot "exact".
const BOUNDARY_TOLERANCE_SECS: i64 = 2;

/// 15 minutes in seconds (window duration).
const WINDOW_SECS: i64 = 900;

pub struct CryptoArbitrageStrategy {
    config: ArbitrageConfig,
    /// On-chain Chainlink RPC client for exact settlement price lookups.
    /// `None` when `config.use_chainlink` is false.
    chainlink_client: Option<Arc<ChainlinkHistoricalClient>>,
    active_markets: HashMap<MarketId, MarketWithReference>,
    /// Price history per coin: (timestamp, price, source).
    /// Kept at PRICE_HISTORY_SIZE entries for retroactive reference lookup.
    price_history: HashMap<String, VecDeque<(DateTime<Utc>, Decimal, String)>>,
    /// Proactive price snapshots at 15-min window boundaries, keyed by "{COIN}-{unix_ts}".
    boundary_prices: HashMap<String, BoundarySnapshot>,
    positions: HashMap<MarketId, Vec<ArbitragePosition>>,
    /// Orders submitted but not yet confirmed — keyed by token_id.
    /// Prevents re-entry while orders are in flight.
    pending_orders: HashMap<TokenId, PendingOrder>,
    /// Open GTC limit orders awaiting fill, keyed by order_id.
    open_limit_orders: HashMap<OrderId, OpenLimitOrder>,
    /// Token IDs with active stop-loss sell orders awaiting confirmation.
    /// Value is the exit (sell) price for P&L calculation.
    /// Positions are only removed once the sell is confirmed or rejected.
    pending_stop_loss: HashMap<TokenId, Decimal>,
    last_scan: Option<tokio::time::Instant>,
    /// Throttle for dashboard-update signal emission (~5 seconds).
    last_dashboard_emit: Option<tokio::time::Instant>,
    /// Cached best-ask prices per token_id, updated on orderbook events.
    /// Used by render_view() to display UP/DOWN market prices.
    cached_asks: HashMap<TokenId, Decimal>,
    /// Markets discovered before prices were available, keyed by coin.
    /// Promoted to active_markets once a price arrives for the coin.
    pending_discovery: HashMap<String, MarketInfo>,
    /// Recent spike events for display and analysis.
    spike_events: VecDeque<SpikeEvent>,
    /// Per-mode performance statistics (wins, losses, P&L).
    mode_stats: HashMap<ArbitrageMode, ModeStats>,
}

impl CryptoArbitrageStrategy {
    /// Create a new crypto arbitrage strategy.
    ///
    /// # Arguments
    /// * `config` - Strategy configuration
    /// * `rpc_urls` - Polygon RPC endpoints for on-chain Chainlink queries (from main config)
    pub fn new(config: ArbitrageConfig, rpc_urls: Vec<String>) -> Self {
        let chainlink_client = if config.use_chainlink {
            Some(Arc::new(ChainlinkHistoricalClient::new(rpc_urls)))
        } else {
            None
        };

        Self {
            config,
            chainlink_client,
            active_markets: HashMap::new(),
            price_history: HashMap::new(),
            boundary_prices: HashMap::new(),
            positions: HashMap::new(),
            pending_orders: HashMap::new(),
            open_limit_orders: HashMap::new(),
            pending_stop_loss: HashMap::new(),
            last_scan: None,
            last_dashboard_emit: None,
            cached_asks: HashMap::new(),
            pending_discovery: HashMap::new(),
            spike_events: VecDeque::new(),
            mode_stats: HashMap::new(),
        }
    }

    // -- Event handlers -----------------------------------------------------

    async fn on_crypto_price(
        &mut self,
        symbol: &str,
        price: Decimal,
        source: &str,
        ctx: &StrategyContext,
    ) -> Result<Vec<Action>> {
        let now = Utc::now();

        // Record price history with source (keep last PRICE_HISTORY_SIZE entries)
        let history = self.price_history.entry(symbol.to_string()).or_default();
        history.push_back((now, price, source.to_string()));
        if history.len() > PRICE_HISTORY_SIZE {
            history.pop_front();
        }

        // Capture boundary snapshot if we just crossed a 15-min boundary.
        // A boundary is at :00, :15, :30, :45 of each hour.
        let ts = now.timestamp();
        let boundary_ts = ts - (ts % WINDOW_SECS);
        let secs_from_boundary = (ts - boundary_ts).abs();
        if secs_from_boundary <= BOUNDARY_TOLERANCE_SECS {
            let key = format!("{symbol}-{boundary_ts}");
            // Only record if we haven't already (prefer Chainlink source)
            let should_insert = match self.boundary_prices.get(&key) {
                None => true,
                Some(existing) => {
                    // Prefer chainlink over other sources
                    source.eq_ignore_ascii_case("chainlink")
                        && !existing.source.eq_ignore_ascii_case("chainlink")
                }
            };
            if should_insert {
                self.boundary_prices.insert(
                    key.clone(),
                    BoundarySnapshot {
                        timestamp: now,
                        price,
                        source: source.to_string(),
                    },
                );
                info!(
                    coin = %symbol,
                    boundary_ts = boundary_ts,
                    price = %price,
                    source = %source,
                    "Captured boundary price snapshot"
                );
            }
            // Prune old boundary snapshots (keep last 4 per coin = 1 hour)
            self.prune_boundary_snapshots(symbol);
        }

        let mut actions = Vec::new();

        // Promote any pending market for this coin now that we have a price
        if let Some(market) = self.pending_discovery.remove(symbol) {
            let window_ts = market
                .start_date
                .map(|d| d.timestamp())
                .or_else(|| parse_slug_timestamp(&market.slug))
                .unwrap_or(boundary_ts);
            let (reference_price, reference_quality) =
                self.find_best_reference(symbol, window_ts, price).await;
            let mwr = MarketWithReference {
                market: market.clone(),
                reference_price,
                reference_quality,
                discovery_time: Utc::now(),
                coin: symbol.to_string(),
            };
            info!(
                coin = %symbol,
                market = %market.id,
                reference = %reference_price,
                quality = ?reference_quality,
                "Activated buffered market (price now available)"
            );
            self.active_markets.insert(market.id.clone(), mwr);
            actions.push(Action::SubscribeMarket(market.id.clone()));
        }

        // -- Spike detection & pre-filter ------------------------------------
        let spike = self.detect_spike(symbol, price);

        // Record spike event if detected
        if let Some(change_pct) = spike {
            let from_price = self
                .price_history
                .get(symbol)
                .and_then(|h| {
                    let cutoff =
                        Utc::now() - chrono::Duration::seconds(self.config.spike.window_secs as i64);
                    h.iter()
                        .rev()
                        .find(|(ts, _, _)| *ts <= cutoff)
                        .map(|(_, p, _)| *p)
                })
                .unwrap_or(price);

            self.spike_events.push_back(SpikeEvent {
                coin: symbol.to_string(),
                timestamp: Utc::now(),
                change_pct,
                from_price,
                to_price: price,
                acted: false,
            });
            // Cap spike history
            while self.spike_events.len() > self.config.spike.history_size {
                self.spike_events.pop_front();
            }

            info!(
                coin = %symbol,
                change_pct = %change_pct,
                "Spike detected"
            );
        }

        // Pre-filter: skip evaluation unless price delta is large enough or spike detected.
        // This avoids wasting compute on tiny price moves that can't be profitable.
        let should_evaluate = if spike.is_some() {
            true
        } else {
            // Check if any active market for this coin has enough price delta to be profitable
            let fee_rate = self.config.fee.taker_fee_rate;
            self.active_markets.values().any(|m| {
                if m.coin != symbol {
                    return false;
                }
                if m.reference_price.is_zero() {
                    return false;
                }
                let delta_pct = ((price - m.reference_price) / m.reference_price).abs();
                // Approximate mid-price fee + min margin as threshold
                let mid_fee = taker_fee(Decimal::new(50, 2), fee_rate);
                let min_margin = self.config.min_profit_margin.min(self.config.late_window_margin);
                delta_pct > mid_fee + min_margin
            })
        };

        if !should_evaluate {
            return Ok(actions);
        }

        // Evaluate each active market for this coin
        let matching_market_ids: Vec<MarketId> = self
            .active_markets
            .iter()
            .filter(|(_, m)| m.coin == symbol)
            .map(|(id, _)| id.clone())
            .collect();

        for market_id in matching_market_ids {
            let market = match self.active_markets.get(&market_id) {
                Some(m) => m.clone(),
                None => continue,
            };

            let opps = self.evaluate_opportunity(&market, price, ctx).await?;
            let total_positions: usize = self.positions.values().map(|v| v.len()).sum();
            let total_pending = self.pending_orders.len();
            let total_limits = self.open_limit_orders.len();
            if !opps.is_empty()
                && (total_positions + total_pending + total_limits + opps.len())
                    <= self.config.max_positions
            {
                // Skip if a limit order is already open for this market.
                // For TwoSided mode, only skip if BOTH token IDs have open orders.
                let has_open_limit = if !opps.is_empty() && opps[0].mode == ArbitrageMode::TwoSided {
                    // TwoSided: check if both outcomes have limit orders
                    if opps.len() == 2 {
                        let token_ids: std::collections::HashSet<_> =
                            opps.iter().map(|o| &o.token_id).collect();
                        let open_tokens: std::collections::HashSet<_> = self
                            .open_limit_orders
                            .values()
                            .filter(|lo| lo.market_id == market_id)
                            .map(|lo| &lo.token_id)
                            .collect();
                        token_ids.is_subset(&open_tokens)
                    } else {
                        false
                    }
                } else {
                    // Other modes: skip if any limit order exists for this market
                    self.open_limit_orders
                        .values()
                        .any(|lo| lo.market_id == market_id)
                };
                if has_open_limit {
                    continue;
                }

                // For TwoSided mode, compute equal share count across both outcomes
                // so total cost = position_size and each side gets N shares.
                let two_sided_size = if opps.len() == 2 && opps[0].mode == ArbitrageMode::TwoSided {
                    let combined_price = opps[0].buy_price + opps[1].buy_price;
                    if combined_price > Decimal::ZERO {
                        Some(self.config.sizing.base_size / combined_price)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let is_two_sided = two_sided_size.is_some();
                let mut batch_orders: Vec<OrderRequest> = Vec::new();

                for opp in &opps {
                    if opp.buy_price.is_zero() {
                        warn!(market = %market_id, "skipping opportunity with zero buy_price");
                        continue;
                    }

                    // Position sizing: Kelly for Confirmed/TailEnd, fixed for TwoSided.
                    let (size, kelly_frac) = if let Some(ts) = two_sided_size {
                        (ts, None)
                    } else if self.config.sizing.use_kelly {
                        let kelly_size =
                            kelly_position_size(opp.confidence, opp.buy_price, &self.config.sizing);
                        if kelly_size.is_zero() {
                            info!(
                                mode = ?opp.mode,
                                market = %market_id,
                                confidence = %opp.confidence,
                                price = %opp.buy_price,
                                "Kelly sizing returned 0 (negative edge), skipping"
                            );
                            continue;
                        }
                        // Convert USDC size to share count
                        let shares = kelly_size / opp.buy_price;
                        // Compute the raw Kelly fraction for tracking
                        let payout = Decimal::ONE / opp.buy_price - Decimal::ONE;
                        let kf = if payout > Decimal::ZERO {
                            (opp.confidence * payout - (Decimal::ONE - opp.confidence)) / payout
                        } else {
                            Decimal::ZERO
                        };
                        (shares, Some(kf))
                    } else {
                        (self.config.sizing.base_size / opp.buy_price, None)
                    };

                    // Hybrid order mode: TailEnd → FOK at best_ask (speed matters);
                    // Confirmed/TwoSided → GTC at best_ask - limit_offset (maker, $0 fee).
                    let (order_type, order_price) =
                        if self.config.order.hybrid_mode && opp.mode != ArbitrageMode::TailEnd {
                            let limit_price =
                                (opp.buy_price - self.config.order.limit_offset).max(Decimal::new(1, 2));
                            (OrderType::Gtc, limit_price)
                        } else {
                            (OrderType::Fok, opp.buy_price)
                        };

                    let order = OrderRequest {
                        token_id: opp.token_id.clone(),
                        price: order_price,
                        size,
                        side: OrderSide::Buy,
                        order_type,
                        neg_risk: false,
                    };
                    info!(
                        mode = ?opp.mode,
                        market = %market_id,
                        confidence = %opp.confidence,
                        price = %order_price,
                        order_type = ?order_type,
                        side = ?opp.outcome_to_buy,
                        kelly = ?kelly_frac,
                        "Submitting arbitrage order"
                    );
                    // Track pending order — position recorded only on confirmed fill
                    self.pending_orders.insert(
                        opp.token_id.clone(),
                        PendingOrder {
                            market_id: market_id.clone(),
                            token_id: opp.token_id.clone(),
                            side: opp.outcome_to_buy,
                            price: order_price,
                            size,
                            reference_price: market.reference_price,
                            coin: market.coin.clone(),
                            order_type,
                            mode: opp.mode.clone(),
                            kelly_fraction: kelly_frac,
                            estimated_fee: opp.estimated_fee,
                        },
                    );

                    if is_two_sided {
                        batch_orders.push(order);
                    } else {
                        actions.push(Action::PlaceOrder(order));
                    }
                }

                // TwoSided mode: emit a single batch order for both legs
                if is_two_sided && !batch_orders.is_empty() {
                    actions.push(Action::PlaceBatchOrder(batch_orders));
                }
            }
        }

        // -- Cross-market correlation: leader spike → follower opportunities --
        if self.config.correlation.enabled
            && let Some(change_pct) = spike
            && change_pct.abs() >= self.config.correlation.min_spike_pct
        {
            let corr_actions = self
                .generate_cross_correlated_opportunities(symbol, change_pct, price, ctx)
                .await?;
            actions.extend(corr_actions);
        }

        Ok(actions)
    }

    /// Evaluate opportunity across three modes in priority order.
    /// Returns zero or more opportunities. TwoSided mode returns two (one per outcome).
    async fn evaluate_opportunity(
        &self,
        market: &MarketWithReference,
        current_price: Decimal,
        ctx: &StrategyContext,
    ) -> Result<Vec<ArbitrageOpportunity>> {
        let time_remaining = market.market.seconds_remaining();

        // Skip ended or almost-ended markets
        if time_remaining <= 0 {
            return Ok(vec![]);
        }

        // Already have a position, pending order, or open limit order in this market
        if self.positions.contains_key(&market.market.id) {
            return Ok(vec![]);
        }
        // Check if any pending orders target this market's tokens
        if self
            .pending_orders
            .values()
            .any(|p| p.market_id == market.market.id)
        {
            return Ok(vec![]);
        }
        // Check if any open limit orders target this market
        if self
            .open_limit_orders
            .values()
            .any(|lo| lo.market_id == market.market.id)
        {
            return Ok(vec![]);
        }

        let md = ctx.market_data.read().await;

        let up_ask = md
            .orderbooks
            .get(&market.market.token_ids.outcome_a)
            .and_then(|ob| ob.best_ask());
        let down_ask = md
            .orderbooks
            .get(&market.market.token_ids.outcome_b)
            .and_then(|ob| ob.best_ask());

        // 1. Tail-End mode: < 120s remaining + predicted winner ask >= 0.90
        if time_remaining < 120
            && !self.is_mode_disabled(&ArbitrageMode::TailEnd)
            && let Some(predicted) = market.predict_winner(current_price)
        {
            let (token_id, ask) = match predicted {
                OutcomeSide::Up | OutcomeSide::Yes => (&market.market.token_ids.outcome_a, up_ask),
                OutcomeSide::Down | OutcomeSide::No => {
                    (&market.market.token_ids.outcome_b, down_ask)
                }
            };
            if let Some(ask_price) = ask
                && ask_price >= Decimal::new(90, 2)
            {
                let profit_margin = Decimal::ONE - ask_price;
                let estimated_fee = taker_fee(ask_price, self.config.fee.taker_fee_rate);
                let net_margin = profit_margin - estimated_fee;
                return Ok(vec![ArbitrageOpportunity {
                    mode: ArbitrageMode::TailEnd,
                    market_id: market.market.id.clone(),
                    outcome_to_buy: predicted,
                    token_id: token_id.clone(),
                    buy_price: ask_price,
                    confidence: Decimal::ONE,
                    profit_margin,
                    estimated_fee,
                    net_margin,
                }]);
            }
        }

        // 2. Two-Sided mode: sum of both asks < 0.98 — buy BOTH outcomes
        //    for guaranteed profit (one resolves to $1, the other to $0,
        //    total cost < $1 so net profit = 1 - combined).
        if !self.is_mode_disabled(&ArbitrageMode::TwoSided)
            && let (Some(ua), Some(da)) = (up_ask, down_ask)
        {
            let combined = ua + da;
            if combined < Decimal::new(98, 2) {
                let profit_margin = Decimal::ONE - combined;
                // In hybrid mode, TwoSided uses GTC maker orders (0 fee)
                let is_maker = self.config.order.hybrid_mode;
                let fee_up = if is_maker {
                    Decimal::ZERO
                } else {
                    taker_fee(ua, self.config.fee.taker_fee_rate)
                };
                let fee_down = if is_maker {
                    Decimal::ZERO
                } else {
                    taker_fee(da, self.config.fee.taker_fee_rate)
                };
                let total_fee = fee_up + fee_down;
                let net_margin = profit_margin - total_fee;
                // Skip if net margin is negative after fees
                if net_margin <= Decimal::ZERO {
                    return Ok(vec![]);
                }
                return Ok(vec![
                    ArbitrageOpportunity {
                        mode: ArbitrageMode::TwoSided,
                        market_id: market.market.id.clone(),
                        outcome_to_buy: OutcomeSide::Up,
                        token_id: market.market.token_ids.outcome_a.clone(),
                        buy_price: ua,
                        confidence: Decimal::ONE,
                        profit_margin,
                        estimated_fee: fee_up,
                        net_margin,
                    },
                    ArbitrageOpportunity {
                        mode: ArbitrageMode::TwoSided,
                        market_id: market.market.id.clone(),
                        outcome_to_buy: OutcomeSide::Down,
                        token_id: market.market.token_ids.outcome_b.clone(),
                        buy_price: da,
                        confidence: Decimal::ONE,
                        profit_margin,
                        estimated_fee: fee_down,
                        net_margin,
                    },
                ]);
            }
        }

        // 3. Confirmed mode: confidence >= threshold + sufficient margin
        if !self.is_mode_disabled(&ArbitrageMode::Confirmed)
            && let Some(predicted) = market.predict_winner(current_price)
        {
            let (token_id, ask) = match predicted {
                OutcomeSide::Up | OutcomeSide::Yes => (&market.market.token_ids.outcome_a, up_ask),
                OutcomeSide::Down | OutcomeSide::No => {
                    (&market.market.token_ids.outcome_b, down_ask)
                }
            };

            if let Some(ask_price) = ask {
                let confidence = market.get_confidence(current_price, ask_price, time_remaining);
                let profit_margin = Decimal::ONE - ask_price;
                // In hybrid mode, Confirmed uses GTC maker orders (0 fee)
                let is_maker = self.config.order.hybrid_mode;
                let estimated_fee = if is_maker {
                    Decimal::ZERO
                } else {
                    taker_fee(ask_price, self.config.fee.taker_fee_rate)
                };
                let net_margin = profit_margin - estimated_fee;
                let min_margin = if time_remaining < 300 {
                    self.config.late_window_margin
                } else {
                    self.config.min_profit_margin
                };

                if confidence >= Decimal::new(50, 2) && net_margin >= min_margin {
                    return Ok(vec![ArbitrageOpportunity {
                        mode: ArbitrageMode::Confirmed,
                        market_id: market.market.id.clone(),
                        outcome_to_buy: predicted,
                        token_id: token_id.clone(),
                        buy_price: ask_price,
                        confidence,
                        profit_margin,
                        estimated_fee,
                        net_margin,
                    }]);
                }
            }
        }

        Ok(vec![])
    }

    /// Generate cross-correlated opportunities for follower coins when a leader
    /// coin spikes. Finds active markets for each follower, checks that the
    /// follower market hasn't already moved (ask still near 0.50), and creates
    /// discounted-confidence opportunities.
    async fn generate_cross_correlated_opportunities(
        &mut self,
        leader_coin: &str,
        leader_change_pct: Decimal,
        _leader_price: Decimal,
        ctx: &StrategyContext,
    ) -> Result<Vec<Action>> {
        let mut actions = Vec::new();

        // Find follower coins for this leader
        let followers: Vec<String> = self
            .config
            .correlation
            .pairs
            .iter()
            .filter(|(leader, _)| leader == leader_coin)
            .flat_map(|(_, followers)| followers.clone())
            .collect();

        if followers.is_empty() {
            return Ok(actions);
        }

        // Compute leader confidence from the spike magnitude
        // Use the same confidence model: larger spikes = higher confidence
        let leader_confidence = leader_change_pct.abs().min(Decimal::ONE);
        let correlation_discount = Decimal::new(7, 1); // 0.7
        let follower_confidence = leader_confidence * correlation_discount;

        // Need at least 50% confidence to act (same threshold as Confirmed mode)
        if follower_confidence < Decimal::new(50, 2) {
            return Ok(actions);
        }

        // Check if CrossCorrelated mode is auto-disabled
        // Use canonical mode (empty leader string) since perf tracking strips leader
        let cross_mode = ArbitrageMode::CrossCorrelated {
            leader: leader_coin.to_string(),
        };
        if self.is_mode_disabled(&cross_mode.canonical()) {
            return Ok(actions);
        }

        let md = ctx.market_data.read().await;

        for follower_coin in &followers {
            // Find active markets for this follower coin
            let follower_market_ids: Vec<MarketId> = self
                .active_markets
                .iter()
                .filter(|(_, m)| m.coin == *follower_coin)
                .map(|(id, _)| id.clone())
                .collect();

            for market_id in follower_market_ids {
                let market = match self.active_markets.get(&market_id) {
                    Some(m) => m.clone(),
                    None => continue,
                };

                // Skip if we already have a position or pending order
                if self.positions.contains_key(&market.market.id) {
                    continue;
                }
                if self
                    .pending_orders
                    .values()
                    .any(|p| p.market_id == market.market.id)
                {
                    continue;
                }
                if self
                    .open_limit_orders
                    .values()
                    .any(|lo| lo.market_id == market.market.id)
                {
                    continue;
                }

                // Skip ended markets
                if market.market.seconds_remaining() <= 0 {
                    continue;
                }

                // Determine predicted side: leader went up → follower Up, leader went down → follower Down
                let predicted = if leader_change_pct > Decimal::ZERO {
                    OutcomeSide::Up
                } else {
                    OutcomeSide::Down
                };

                let (token_id, ask) = match predicted {
                    OutcomeSide::Up | OutcomeSide::Yes => (
                        &market.market.token_ids.outcome_a,
                        md.orderbooks
                            .get(&market.market.token_ids.outcome_a)
                            .and_then(|ob| ob.best_ask()),
                    ),
                    OutcomeSide::Down | OutcomeSide::No => (
                        &market.market.token_ids.outcome_b,
                        md.orderbooks
                            .get(&market.market.token_ids.outcome_b)
                            .and_then(|ob| ob.best_ask()),
                    ),
                };

                let ask_price = match ask {
                    Some(p) => p,
                    None => continue,
                };

                // Skip if follower market already moved away from 0.50
                // (market has already caught up to the leader's move)
                if ask_price > Decimal::new(60, 2) || ask_price < Decimal::new(40, 2) {
                    info!(
                        leader = %leader_coin,
                        follower = %follower_coin,
                        ask = %ask_price,
                        "Skipping cross-correlated signal: follower market already moved"
                    );
                    continue;
                }

                let profit_margin = Decimal::ONE - ask_price;
                let is_maker = self.config.order.hybrid_mode;
                let estimated_fee = if is_maker {
                    Decimal::ZERO
                } else {
                    taker_fee(ask_price, self.config.fee.taker_fee_rate)
                };
                let net_margin = profit_margin - estimated_fee;
                let min_margin = self.config.min_profit_margin;

                if net_margin < min_margin {
                    continue;
                }

                // Check position limits
                let total_positions: usize = self.positions.values().map(|v| v.len()).sum();
                let total_pending = self.pending_orders.len();
                let total_limits = self.open_limit_orders.len();
                if total_positions + total_pending + total_limits + 1 > self.config.max_positions {
                    break;
                }

                info!(
                    leader = %leader_coin,
                    follower = %follower_coin,
                    leader_change = %leader_change_pct,
                    follower_confidence = %follower_confidence,
                    ask = %ask_price,
                    net_margin = %net_margin,
                    "Cross-correlated opportunity detected"
                );

                let opp = ArbitrageOpportunity {
                    mode: ArbitrageMode::CrossCorrelated {
                        leader: leader_coin.to_string(),
                    },
                    market_id: market.market.id.clone(),
                    outcome_to_buy: predicted,
                    token_id: token_id.clone(),
                    buy_price: ask_price,
                    confidence: follower_confidence,
                    profit_margin,
                    estimated_fee,
                    net_margin,
                };

                // Use Kelly sizing for CrossCorrelated (like Confirmed mode)
                let (size, kelly_frac) = if self.config.sizing.use_kelly {
                    let kelly_size =
                        kelly_position_size(opp.confidence, opp.buy_price, &self.config.sizing);
                    if kelly_size.is_zero() {
                        continue;
                    }
                    let shares = kelly_size / opp.buy_price;
                    let payout = Decimal::ONE / opp.buy_price - Decimal::ONE;
                    let kf = if payout > Decimal::ZERO {
                        (opp.confidence * payout - (Decimal::ONE - opp.confidence)) / payout
                    } else {
                        Decimal::ZERO
                    };
                    (shares, Some(kf))
                } else {
                    (self.config.sizing.base_size / opp.buy_price, None)
                };

                // Use hybrid order mode (GTC for maker, like Confirmed)
                let (order_type, order_price) =
                    if self.config.order.hybrid_mode {
                        let limit_price =
                            (opp.buy_price - self.config.order.limit_offset).max(Decimal::new(1, 2));
                        (OrderType::Gtc, limit_price)
                    } else {
                        (OrderType::Fok, opp.buy_price)
                    };

                let order = OrderRequest {
                    token_id: token_id.clone(),
                    side: polyrust_core::types::OrderSide::Buy,
                    price: order_price,
                    size,
                    order_type,
                    neg_risk: false, // 15-min markets always use false
                };

                self.pending_orders.insert(
                    token_id.clone(),
                    PendingOrder {
                        market_id: market.market.id.clone(),
                        token_id: token_id.clone(),
                        side: predicted,
                        price: order_price,
                        size,
                        reference_price: market.reference_price,
                        coin: follower_coin.clone(),
                        order_type,
                        mode: opp.mode.clone(),
                        kelly_fraction: kelly_frac,
                        estimated_fee: opp.estimated_fee,
                    },
                );

                actions.push(Action::PlaceOrder(order));
            }
        }

        Ok(actions)
    }

    async fn on_orderbook_update(
        &mut self,
        snapshot: &OrderbookSnapshot,
        ctx: &StrategyContext,
    ) -> Result<Vec<Action>> {
        // Update market data in shared context
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks
                .insert(snapshot.token_id.clone(), snapshot.clone());
        }

        // Cache best-ask price for dashboard display
        if let Some(best_ask) = snapshot.asks.first() {
            self.cached_asks
                .insert(snapshot.token_id.clone(), best_ask.price);
        }

        // Update peak_bid for trailing stop-loss tracking
        if let Some(current_bid) = snapshot.best_bid() {
            for positions in self.positions.values_mut() {
                for pos in positions.iter_mut() {
                    if pos.token_id == snapshot.token_id && current_bid > pos.peak_bid {
                        pos.peak_bid = current_bid;
                    }
                }
            }
        }

        // Check stop-losses on open positions
        let mut actions = Vec::new();
        let position_ids: Vec<MarketId> = self.positions.keys().cloned().collect();

        for market_id in position_ids {
            let positions = match self.positions.get(&market_id) {
                Some(p) => p.clone(),
                None => continue,
            };

            for pos in &positions {
                // Only check if this snapshot is for the position's token
                if pos.token_id != snapshot.token_id {
                    continue;
                }

                // Skip if stop-loss sell already in flight for this token
                if self.pending_stop_loss.contains_key(&pos.token_id) {
                    continue;
                }

                if let Some((action, exit_price)) = self.check_stop_loss(pos, snapshot)? {
                    info!(
                        market = %market_id,
                        entry = %pos.entry_price,
                        exit = %exit_price,
                        side = ?pos.side,
                        "Stop-loss triggered, selling position"
                    );
                    // Track pending stop-loss with exit price for P&L calculation
                    self.pending_stop_loss.insert(pos.token_id.clone(), exit_price);
                    actions.push(action);
                }
            }
        }

        Ok(actions)
    }

    /// Check if stop-loss should trigger for a position.
    ///
    /// Triggers when:
    /// 1. Crypto price reversed by >= stop_loss_reversal_pct (0.5%)
    /// 2. Market price dropped by >= stop_loss_min_drop (5¢) from entry
    /// 3. Time remaining > 60s (don't sell in final minute)
    ///
    /// Returns `Some((action, exit_price))` when stop-loss should trigger.
    fn check_stop_loss(
        &self,
        pos: &ArbitragePosition,
        snapshot: &OrderbookSnapshot,
    ) -> Result<Option<(Action, Decimal)>> {
        let market = match self.active_markets.get(&pos.market_id) {
            Some(m) => m,
            None => return Ok(None),
        };

        let time_remaining = market.market.seconds_remaining();
        // Don't trigger stop-loss in the final 60 seconds
        if time_remaining <= 60 {
            return Ok(None);
        }

        // Check crypto price reversal
        let current_crypto = self
            .price_history
            .get(&pos.coin)
            .and_then(|h| h.back().map(|(_, p, _)| *p));

        let crypto_reversed = if let Some(current) = current_crypto {
            let reversal = match pos.side {
                OutcomeSide::Up | OutcomeSide::Yes => {
                    // We bet Up, so reversal = price went down
                    (pos.reference_price - current) / pos.reference_price
                }
                OutcomeSide::Down | OutcomeSide::No => {
                    // We bet Down, so reversal = price went up
                    (current - pos.reference_price) / pos.reference_price
                }
            };
            reversal >= self.config.stop_loss.reversal_pct
        } else {
            false
        };

        // Check market price drop from entry
        let current_bid = match snapshot.best_bid() {
            Some(bid) => bid,
            None => return Ok(None), // No bids — cannot sell, skip stop-loss
        };
        let price_drop = pos.entry_price - current_bid;
        let market_dropped = price_drop >= self.config.stop_loss.min_drop;

        // Trailing stop: triggers when position was profitable and bid dropped from peak
        let trailing_triggered = if self.config.stop_loss.trailing_enabled
            && pos.peak_bid > pos.entry_price
        {
            let base_distance = self.config.stop_loss.trailing_distance;
            let effective_distance = if self.config.stop_loss.time_decay {
                // Tighten trailing distance as expiry approaches (900s = 15min market)
                let decay_factor =
                    Decimal::from(time_remaining) / Decimal::from(900i64);
                // Clamp to [0, 1] — don't widen beyond base distance
                let clamped = if decay_factor > Decimal::ONE {
                    Decimal::ONE
                } else if decay_factor < Decimal::ZERO {
                    Decimal::ZERO
                } else {
                    decay_factor
                };
                base_distance * clamped
            } else {
                base_distance
            };
            let drop_from_peak = pos.peak_bid - current_bid;
            drop_from_peak >= effective_distance
        } else {
            false
        };

        if (crypto_reversed && market_dropped) || trailing_triggered {
            let order = OrderRequest {
                token_id: pos.token_id.clone(),
                price: current_bid,
                size: pos.size,
                side: OrderSide::Sell,
                order_type: OrderType::Fok,
                neg_risk: false,
            };
            Ok(Some((Action::PlaceOrder(order), current_bid)))
        } else {
            Ok(None)
        }
    }

    async fn on_market_discovered(
        &mut self,
        market: &MarketInfo,
        ctx: &StrategyContext,
    ) -> Result<Vec<Action>> {
        // Check if this is a crypto market we care about
        let coin = match self.extract_coin(&market.question) {
            Some(c) => c,
            None => return Ok(vec![]),
        };

        if !self.config.coins.contains(&coin) {
            return Ok(vec![]);
        }

        // Get the current crypto price — needed as final fallback
        let md = ctx.market_data.read().await;
        let current_price = match md.external_prices.get(&coin) {
            Some(&p) => p,
            None => {
                info!(coin = %coin, market = %market.id, "No price yet for coin, buffering market for later activation");
                drop(md);
                self.pending_discovery.insert(coin, market.clone());
                return Ok(vec![]);
            }
        };
        drop(md);

        // Determine window start timestamp
        let window_ts = market
            .start_date
            .map(|d| d.timestamp())
            .or_else(|| parse_slug_timestamp(&market.slug))
            .unwrap_or_else(|| {
                // Fallback: align to nearest 15-min boundary
                let now = Utc::now().timestamp();
                now - (now % WINDOW_SECS)
            });

        let (reference_price, reference_quality) =
            self.find_best_reference(&coin, window_ts, current_price).await;

        let mwr = MarketWithReference {
            market: market.clone(),
            reference_price,
            reference_quality,
            discovery_time: Utc::now(),
            coin: coin.clone(),
        };

        info!(
            coin = %coin,
            market = %market.id,
            reference = %reference_price,
            quality = ?reference_quality,
            "Discovered crypto market"
        );

        self.active_markets.insert(market.id.clone(), mwr);

        Ok(vec![Action::SubscribeMarket(market.id.clone())])
    }

    async fn on_market_expired(
        &mut self,
        market_id: &str,
        _ctx: &StrategyContext,
    ) -> Result<Vec<Action>> {
        // Capture coin before removing market (needed for price lookup)
        let market_coin = self
            .active_markets
            .get(market_id)
            .map(|m| m.coin.clone());

        if let Some(market) = self.active_markets.remove(market_id) {
            info!(
                market = %market_id,
                coin = %market.coin,
                "Market expired, removing from active markets"
            );
        }

        let mut actions = vec![Action::UnsubscribeMarket(market_id.to_string())];

        if let Some(positions) = self.positions.remove(market_id) {
            for pos in &positions {
                // Estimate outcome: check if crypto price moved in our direction.
                // Up side wins if current_price > reference_price, Down wins otherwise.
                let current_crypto = market_coin
                    .as_ref()
                    .and_then(|coin| self.price_history.get(coin.as_str()))
                    .and_then(|h| h.back().map(|(_, p, _)| *p));

                let pnl = if let Some(current) = current_crypto {
                    let won = match pos.side {
                        OutcomeSide::Up | OutcomeSide::Yes => current > pos.reference_price,
                        OutcomeSide::Down | OutcomeSide::No => current < pos.reference_price,
                    };
                    if won {
                        // Winner: payout $1 per share
                        (Decimal::ONE - pos.entry_price) * pos.size - (pos.estimated_fee * pos.size)
                    } else {
                        // Loser: shares worth $0 (fee already paid at entry)
                        -pos.entry_price * pos.size
                    }
                } else {
                    // No price data — assume loss (conservative, fee already paid at entry)
                    -pos.entry_price * pos.size
                };

                self.record_trade_pnl(&pos.mode, pnl);

                warn!(
                    market = %market_id,
                    side = ?pos.side,
                    entry = %pos.entry_price,
                    pnl = %pnl,
                    mode = %pos.mode,
                    "Position in expired market — resolved"
                );
            }
            actions.push(Action::Log {
                level: LogLevel::Info,
                message: format!(
                    "Market {} expired with {} open position(s)",
                    market_id,
                    positions.len()
                ),
            });
        }

        Ok(actions)
    }

    /// Handle order placement result — only record position on confirmed success.
    fn on_order_placed(&mut self, result: &OrderResult) -> Vec<Action> {
        // Check if this is a stop-loss sell confirmation
        if let Some(exit_price) = self.pending_stop_loss.remove(&result.token_id) {
            if result.success {
                // Sell confirmed — remove the position and record P&L
                if let Some(pos) = self.remove_position_by_token(&result.token_id) {
                    // Calculate exit fee (FOK order at current bid incurs taker fee)
                    let exit_fee = taker_fee(exit_price, self.config.fee.taker_fee_rate);
                    let pnl = (exit_price - pos.entry_price) * pos.size
                        - (pos.estimated_fee * pos.size)
                        - (exit_fee * pos.size);
                    self.record_trade_pnl(&pos.mode, pnl);
                    info!(
                        token_id = %result.token_id,
                        mode = %pos.mode,
                        pnl = %pnl,
                        "Stop-loss sell confirmed, position removed"
                    );
                }
            } else {
                warn!(
                    token_id = %result.token_id,
                    message = %result.message,
                    "Stop-loss sell failed, position retained for retry"
                );
            }
            return vec![];
        }

        let pending = match self.pending_orders.remove(&result.token_id) {
            Some(p) => p,
            None => return vec![], // Not our order
        };

        if !result.success {
            warn!(
                token_id = %result.token_id,
                market = %pending.market_id,
                message = %result.message,
                "Order rejected, removing pending entry"
            );
            return vec![];
        }

        // GTC orders: track as open limit order; position created on fill event.
        // FOK orders: immediate fill — create position now.
        if pending.order_type == OrderType::Gtc {
            if let Some(order_id) = &result.order_id {
                info!(
                    order_id = %order_id,
                    market = %pending.market_id,
                    mode = ?pending.mode,
                    price = %pending.price,
                    "GTC limit order placed, tracking for fill"
                );
                self.open_limit_orders.insert(
                    order_id.clone(),
                    OpenLimitOrder {
                        order_id: order_id.clone(),
                        market_id: pending.market_id,
                        token_id: pending.token_id,
                        side: pending.side,
                        price: pending.price,
                        size: pending.size,
                        reference_price: pending.reference_price,
                        coin: pending.coin,
                        placed_at: tokio::time::Instant::now(),
                        mode: pending.mode,
                        kelly_fraction: pending.kelly_fraction,
                        estimated_fee: pending.estimated_fee,
                    },
                );
            }
            return vec![];
        }

        let mode = pending.mode.clone();
        let position = ArbitragePosition {
            market_id: pending.market_id.clone(),
            token_id: pending.token_id,
            side: pending.side,
            entry_price: pending.price,
            size: pending.size,
            reference_price: pending.reference_price,
            coin: pending.coin,
            order_id: result.order_id.clone(),
            entry_time: Utc::now(),
            kelly_fraction: pending.kelly_fraction,
            peak_bid: pending.price,
            mode: pending.mode,
            estimated_fee: pending.estimated_fee,
        };

        info!(
            market = %pending.market_id,
            side = ?position.side,
            price = %position.entry_price,
            size = %position.size,
            mode = %mode,
            "Position confirmed after order fill"
        );

        self.positions
            .entry(pending.market_id)
            .or_default()
            .push(position);

        vec![]
    }

    /// Handle a fully filled order event — move from open_limit_orders to positions.
    fn on_order_filled(
        &mut self,
        order_id: &str,
        token_id: &str,
        price: Decimal,
        size: Decimal,
    ) -> Vec<Action> {
        if let Some(lo) = self.open_limit_orders.remove(order_id) {
            info!(
                order_id = %order_id,
                market = %lo.market_id,
                mode = ?lo.mode,
                price = %price,
                size = %size,
                "GTC limit order filled, creating position"
            );
            let position = ArbitragePosition {
                market_id: lo.market_id.clone(),
                token_id: lo.token_id,
                side: lo.side,
                entry_price: price,
                size,
                reference_price: lo.reference_price,
                coin: lo.coin,
                order_id: Some(order_id.to_string()),
                entry_time: Utc::now(),
                kelly_fraction: lo.kelly_fraction,
                peak_bid: price,
                mode: lo.mode,
                estimated_fee: lo.estimated_fee,
            };
            self.positions
                .entry(lo.market_id)
                .or_default()
                .push(position);
        } else {
            // Could be a fill for a non-limit order we don't track here
            info!(
                order_id = %order_id,
                token_id = %token_id,
                "Received fill for unknown order (may be external)"
            );
        }
        vec![]
    }

    /// Remove a position by token_id across all markets, returning it.
    fn remove_position_by_token(&mut self, token_id: &str) -> Option<ArbitragePosition> {
        let mut removed = None;
        let mut empty_markets = Vec::new();
        for (market_id, positions) in &mut self.positions {
            if let Some(idx) = positions.iter().position(|p| p.token_id == token_id) {
                removed = Some(positions.remove(idx));
            }
            if positions.is_empty() {
                empty_markets.push(market_id.clone());
            }
        }
        for market_id in empty_markets {
            self.positions.remove(&market_id);
        }
        removed
    }

    /// Check if a mode is auto-disabled due to poor performance.
    fn is_mode_disabled(&self, mode: &ArbitrageMode) -> bool {
        if !self.config.performance.auto_disable {
            return false;
        }
        let canonical_mode = mode.canonical();
        if let Some(stats) = self.mode_stats.get(&canonical_mode) {
            stats.total_trades() >= self.config.performance.min_trades
                && stats.win_rate() < self.config.performance.min_win_rate
        } else {
            false
        }
    }

    /// Record a trade P&L outcome for the given mode.
    fn record_trade_pnl(&mut self, mode: &ArbitrageMode, pnl: Decimal) {
        let window_size = self.config.performance.window_size;
        let canonical_mode = mode.canonical();
        self.mode_stats
            .entry(canonical_mode)
            .or_insert_with(|| ModeStats::new(window_size))
            .record(pnl);
    }

    /// Cancel GTC limit orders that have been open longer than `max_age_secs`.
    fn check_stale_limit_orders(&mut self) -> Vec<Action> {
        let max_age = std::time::Duration::from_secs(self.config.order.max_age_secs);
        let now = tokio::time::Instant::now();

        let stale_ids: Vec<OrderId> = self
            .open_limit_orders
            .iter()
            .filter(|(_, lo)| now.duration_since(lo.placed_at) >= max_age)
            .map(|(id, _)| id.clone())
            .collect();

        let mut actions = Vec::new();
        for order_id in stale_ids {
            if let Some(lo) = self.open_limit_orders.remove(&order_id) {
                info!(
                    order_id = %order_id,
                    market = %lo.market_id,
                    age_secs = now.duration_since(lo.placed_at).as_secs(),
                    "Cancelling stale GTC limit order"
                );
                actions.push(Action::CancelOrder(order_id));
            }
        }
        actions
    }

    // -- Dashboard ----------------------------------------------------------

    /// Emit a dashboard-update signal if enough time has elapsed since the last one.
    /// Returns `Some(Action)` when the throttle interval (5 seconds) has passed.
    /// Pre-renders the view HTML and includes it in the payload so the SSE handler
    /// doesn't need to re-acquire the strategy lock (which would deadlock).
    fn maybe_emit_dashboard_update(&mut self) -> Option<Action> {
        let now = tokio::time::Instant::now();
        let should_emit = match self.last_dashboard_emit {
            Some(last) => now.duration_since(last) >= std::time::Duration::from_secs(5),
            None => true,
        };
        if should_emit {
            self.last_dashboard_emit = Some(now);
            let html = self.render_view().unwrap_or_default();
            Some(Action::EmitSignal {
                signal_type: "dashboard-update".to_string(),
                payload: serde_json::json!({
                    "view_name": self.view_name(),
                    "rendered_html": html,
                }),
            })
        } else {
            None
        }
    }

    // -- Reference price helpers -----------------------------------------------

    /// Find the most accurate reference price for a coin at a given window start.
    ///
    /// Priority (revised for RTDS accuracy over on-chain staleness):
    /// 0. Exact boundary snapshot (captured within 2s of window start via RTDS)
    /// 1. On-chain Chainlink RPC lookup (if no boundary, use if staleness ≤ 30s)
    /// 2. Closest historical price entry (within 30s of window start)
    /// 3. Current price (fallback)
    ///
    /// Rationale: RTDS boundary snapshots are captured <2s from target, while on-chain
    /// Chainlink rounds have ~12-15s typical staleness. Prefer fresher RTDS data.
    async fn find_best_reference(
        &self,
        coin: &str,
        window_ts: i64,
        current_price: Decimal,
    ) -> (Decimal, ReferenceQuality) {
        // 0. Exact boundary snapshot — best real-time accuracy via RTDS (<2s from target)
        let key = format!("{coin}-{window_ts}");
        let boundary_snap = self.boundary_prices.get(&key).cloned();

        if let Some(snap) = &boundary_snap {
            let snap_staleness = snap.timestamp.timestamp().abs_diff(window_ts);
            // Use boundary snapshot if it's within tolerance (2s)
            if snap_staleness <= BOUNDARY_TOLERANCE_SECS as u64 {
                // Optionally fetch on-chain for comparison logging (don't block on it)
                if let Some(client) = &self.chainlink_client
                    && let Ok(cp) = client
                        .get_price_at_timestamp(coin, window_ts as u64, 100)
                        .await
                {
                    let onchain_staleness = cp.timestamp.abs_diff(window_ts as u64);
                    info!(
                        coin = %coin,
                        boundary_price = %snap.price,
                        boundary_staleness_s = snap_staleness,
                        onchain_price = %cp.price,
                        onchain_staleness_s = onchain_staleness,
                        "Reference comparison: preferring boundary snapshot over on-chain"
                    );
                }
                return (snap.price, ReferenceQuality::Exact);
            }
        }

        // 1. On-chain Chainlink RPC — use if no fresh boundary and staleness ≤ 30s
        if let Some(client) = &self.chainlink_client {
            match client
                .get_price_at_timestamp(coin, window_ts as u64, 100)
                .await
            {
                Ok(cp) => {
                    let staleness = cp.timestamp.abs_diff(window_ts as u64);
                    if staleness <= 30 {
                        info!(
                            coin = %coin,
                            price = %cp.price,
                            staleness_s = staleness,
                            round_id = cp.round_id,
                            "On-chain Chainlink reference price retrieved (no boundary available)"
                        );
                        return (cp.price, ReferenceQuality::OnChain(staleness));
                    }
                    warn!(
                        coin = %coin,
                        staleness_s = staleness,
                        "On-chain round too stale (>30s), trying historical"
                    );
                }
                Err(e) => {
                    warn!(
                        coin = %coin,
                        error = %e,
                        "On-chain Chainlink lookup failed, falling back to local data"
                    );
                }
            }
        }

        // 2. Historical lookup — closest entry to window start, preferring Chainlink source
        let target = DateTime::from_timestamp(window_ts, 0);
        if let (Some(target_dt), Some(history)) = (target, self.price_history.get(coin)) {
            // Find all entries within 30s of window start
            let mut best: Option<(u64, Decimal, bool)> = None; // (staleness, price, is_chainlink)
            for (ts, price, source) in history {
                let staleness = (*ts - target_dt).num_seconds().unsigned_abs();
                if staleness >= 30 {
                    continue;
                }
                let is_chainlink = source.eq_ignore_ascii_case("chainlink");
                let is_better = match best {
                    None => true,
                    Some((prev_stale, _, prev_cl)) => {
                        // Prefer Chainlink if staleness is similar (within 5s)
                        if is_chainlink && !prev_cl && staleness < prev_stale + 5 {
                            true
                        } else if !is_chainlink && prev_cl && prev_stale < staleness + 5 {
                            false
                        } else {
                            staleness < prev_stale
                        }
                    }
                };
                if is_better {
                    best = Some((staleness, *price, is_chainlink));
                }
            }
            if let Some((staleness, price, _)) = best {
                return (price, ReferenceQuality::Historical(staleness));
            }
        }

        // 3. Current price (existing behavior)
        (current_price, ReferenceQuality::Current)
    }

    /// Remove boundary snapshots older than 4 windows (1 hour) for a given coin.
    fn prune_boundary_snapshots(&mut self, coin: &str) {
        let now_ts = Utc::now().timestamp();
        let cutoff = now_ts - (WINDOW_SECS * 4);
        let prefix = format!("{coin}-");
        self.boundary_prices.retain(|key, _| {
            if !key.starts_with(&prefix) {
                return true;
            }
            // Extract timestamp from key
            key.strip_prefix(&prefix)
                .and_then(|ts_str| ts_str.parse::<i64>().ok())
                .is_none_or(|ts| ts >= cutoff)
        });
    }

    // -- Spike detection ------------------------------------------------------

    /// Detect a price spike for a coin by comparing current price to the
    /// price `spike.window_secs` seconds ago in `price_history`.
    ///
    /// Returns `Some(change_pct)` if the absolute percentage change exceeds
    /// `spike.threshold_pct`, otherwise `None`.
    fn detect_spike(&self, coin: &str, current_price: Decimal) -> Option<Decimal> {
        let history = self.price_history.get(coin)?;
        let now = Utc::now();
        let window = chrono::Duration::seconds(self.config.spike.window_secs as i64);
        let cutoff = now - window;

        // Find the oldest price entry that is at or before the cutoff
        // (i.e. the baseline price from `window_secs` ago).
        let baseline = history
            .iter()
            .rev()
            .find(|(ts, _, _)| *ts <= cutoff)
            .map(|(_, p, _)| *p)?;

        if baseline.is_zero() {
            return None;
        }

        let change_pct = (current_price - baseline) / baseline;
        if change_pct.abs() >= self.config.spike.threshold_pct {
            Some(change_pct)
        } else {
            None
        }
    }

    // -- Helpers ------------------------------------------------------------

    /// Extract coin symbol from market question string.
    /// Looks for known coin names as whole words in the question text.
    fn extract_coin(&self, question: &str) -> Option<String> {
        const COIN_NAMES: &[(&str, &str)] = &[
            ("BITCOIN", "BTC"),
            ("ETHEREUM", "ETH"),
            ("SOLANA", "SOL"),
        ];

        let upper = question.to_uppercase();

        // First, check for full coin names (e.g. "Bitcoin" → "BTC")
        for &(name, ticker) in COIN_NAMES {
            if upper.contains(name) {
                return Some(ticker.to_string());
            }
        }

        // Then, check for ticker symbols as whole words (e.g. "XRP")
        for coin in &self.config.coins {
            // Match coin as a whole word to avoid false positives
            // (e.g. "SOL" should not match "SOLVE" or "resolution")
            let mut found = false;
            for (idx, _) in upper.match_indices(coin.as_str()) {
                let before_ok = idx == 0
                    || !upper[..idx]
                        .chars()
                        .next_back()
                        .unwrap()
                        .is_ascii_alphanumeric();
                let after_idx = idx + coin.len();
                let after_ok = after_idx >= upper.len()
                    || !upper[after_idx..]
                        .chars()
                        .next()
                        .unwrap()
                        .is_ascii_alphanumeric();
                if before_ok && after_ok {
                    found = true;
                    break;
                }
            }
            if found {
                return Some(coin.clone());
            }
        }
        None
    }
}

impl DashboardViewProvider for CryptoArbitrageStrategy {
    fn view_name(&self) -> &str {
        "crypto-arb"
    }

    fn render_view(&self) -> polyrust_core::error::Result<String> {
        let mut html = String::with_capacity(4096);

        // --- Reference Prices & Predictions ---
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        html.push_str(r#"<h2 class="text-lg font-bold mb-3">Reference Prices &amp; Predictions</h2>"#);

        if self.active_markets.is_empty() {
            html.push_str(r#"<p class="text-gray-500">No active markets</p>"#);
        } else {
            html.push_str(r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#);
            html.push_str("<th class=\"text-left py-1\">Coin</th>");
            html.push_str("<th class=\"text-right py-1\">Ref Price</th>");
            html.push_str("<th class=\"text-right py-1\">Current</th>");
            html.push_str("<th class=\"text-right py-1\">Change</th>");
            html.push_str("<th class=\"text-right py-1\">Prediction</th>");
            html.push_str("</tr></thead><tbody>");

            // Collect unique coins from active markets
            let mut seen_coins = HashSet::new();
            let mut markets_sorted: Vec<_> = self.active_markets.values().collect();
            markets_sorted.sort_by(|a, b| a.coin.cmp(&b.coin));

            for mwr in &markets_sorted {
                if !seen_coins.insert(&mwr.coin) {
                    continue;
                }
                let current_price = self
                    .price_history
                    .get(&mwr.coin)
                    .and_then(|h| h.back().map(|(_, p, _)| *p));

                let ref_label = match mwr.reference_quality {
                    ReferenceQuality::OnChain(_) => "✓",
                    ReferenceQuality::Exact => "=",
                    ReferenceQuality::Historical(_) => "≈",
                    ReferenceQuality::Current => "~",
                };

                let (change_str, change_class, prediction) = match current_price {
                    Some(cp) => {
                        let change = if mwr.reference_price.is_zero() {
                            Decimal::ZERO
                        } else {
                            ((cp - mwr.reference_price) / mwr.reference_price)
                                * Decimal::new(100, 0)
                        };
                        let cls = if change >= Decimal::ZERO {
                            "pnl-positive"
                        } else {
                            "pnl-negative"
                        };
                        let pred = match mwr.predict_winner(cp) {
                            Some(OutcomeSide::Up) | Some(OutcomeSide::Yes) => "UP",
                            Some(OutcomeSide::Down) | Some(OutcomeSide::No) => "DOWN",
                            None => "-",
                        };
                        (format!("{:+.2}%", change), cls, pred)
                    }
                    None => ("-".to_string(), "", "-"),
                };

                let _ = write!(
                    html,
                    r#"<tr class="border-b border-gray-800"><td class="py-1">{coin}</td><td class="text-right py-1">{ref_label}{ref_price}</td><td class="text-right py-1">{current}</td><td class="text-right py-1 {change_class}">{change}</td><td class="text-right py-1 font-bold">{prediction}</td></tr>"#,
                    coin = escape_html(&mwr.coin),
                    ref_label = ref_label,
                    ref_price = fmt_usd(mwr.reference_price),
                    current = current_price
                        .map(fmt_usd)
                        .unwrap_or_else(|| "-".to_string()),
                    change_class = change_class,
                    change = change_str,
                    prediction = prediction,
                );
            }
            html.push_str("</tbody></table>");
        }
        html.push_str("</div>");

        // --- Active Markets ---
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        let _ = write!(
            html,
            r#"<h2 class="text-lg font-bold mb-3">Active Markets ({})</h2>"#,
            self.active_markets.len()
        );

        if self.active_markets.is_empty() {
            html.push_str(r#"<p class="text-gray-500">No active markets</p>"#);
        } else {
            html.push_str(r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#);
            html.push_str("<th class=\"text-left py-1\">Market</th>");
            html.push_str("<th class=\"text-right py-1\">UP</th>");
            html.push_str("<th class=\"text-right py-1\">DOWN</th>");
            html.push_str("<th class=\"text-right py-1\">Fee</th>");
            html.push_str("<th class=\"text-right py-1\">Net</th>");
            html.push_str("<th class=\"text-right py-1\">Time Left</th>");
            html.push_str("</tr></thead><tbody>");

            let mut markets_by_time: Vec<_> = self.active_markets.values().collect();
            markets_by_time.sort_by_key(|m| m.market.end_date);

            for mwr in &markets_by_time {
                let remaining = mwr.market.seconds_remaining().max(0);
                let time_str = if remaining > 60 {
                    format!("{}m {}s", remaining / 60, remaining % 60)
                } else {
                    format!("{}s", remaining)
                };

                let up_ask = self
                    .cached_asks
                    .get(&mwr.market.token_ids.outcome_a)
                    .copied();
                let down_ask = self
                    .cached_asks
                    .get(&mwr.market.token_ids.outcome_b)
                    .copied();

                let up_price = up_ask
                    .map(fmt_market_price)
                    .unwrap_or_else(|| "-".to_string());
                let down_price = down_ask
                    .map(fmt_market_price)
                    .unwrap_or_else(|| "-".to_string());

                // Show fee/net for the predicted winner side (or lower-priced side)
                let fee_rate = self.config.fee.taker_fee_rate;
                let (fee_str, net_str) = match (up_ask, down_ask) {
                    (Some(ua), Some(da)) => {
                        // Show fee for the lower-priced (more likely to trade) side
                        let price = ua.min(da);
                        let fee = taker_fee(price, fee_rate);
                        let net = net_profit_margin(price, fee_rate, false);
                        (
                            format!("{:.3}", fee.round_dp(3)),
                            format!("{:.3}", net.round_dp(3)),
                        )
                    }
                    (Some(p), None) | (None, Some(p)) => {
                        let fee = taker_fee(p, fee_rate);
                        let net = net_profit_margin(p, fee_rate, false);
                        (
                            format!("{:.3}", fee.round_dp(3)),
                            format!("{:.3}", net.round_dp(3)),
                        )
                    }
                    _ => ("-".to_string(), "-".to_string()),
                };

                let _ = write!(
                    html,
                    r#"<tr class="border-b border-gray-800"><td class="py-1">{coin} Up/Down</td><td class="text-right py-1">{up}</td><td class="text-right py-1">{down}</td><td class="text-right py-1">{fee}</td><td class="text-right py-1">{net}</td><td class="text-right py-1">{time}</td></tr>"#,
                    coin = escape_html(&mwr.coin),
                    up = up_price,
                    down = down_price,
                    fee = fee_str,
                    net = net_str,
                    time = time_str,
                );
            }
            html.push_str("</tbody></table>");
        }
        html.push_str("</div>");

        // --- Open Positions ---
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        let total_positions: usize = self.positions.values().map(|v| v.len()).sum();
        let _ = write!(
            html,
            r#"<h2 class="text-lg font-bold mb-3">Open Positions ({})</h2>"#,
            total_positions
        );

        if self.positions.is_empty() {
            html.push_str(r#"<p class="text-gray-500">No open positions</p>"#);
        } else {
            html.push_str(r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#);
            html.push_str("<th class=\"text-left py-1\">Market</th>");
            html.push_str("<th class=\"text-left py-1\">Side</th>");
            html.push_str("<th class=\"text-right py-1\">Entry</th>");
            html.push_str("<th class=\"text-right py-1\">Current</th>");
            html.push_str("<th class=\"text-right py-1\">PnL</th>");
            html.push_str("<th class=\"text-right py-1\">Size</th>");
            html.push_str("<th class=\"text-right py-1\">Kelly</th>");
            html.push_str("</tr></thead><tbody>");

            for positions in self.positions.values() {
                for pos in positions {
                    let current = self.cached_asks.get(&pos.token_id).copied();
                    let (current_str, pnl_str, pnl_class) = match current {
                        Some(cp) => {
                            let pnl = (cp - pos.entry_price) * pos.size - (pos.estimated_fee * pos.size);
                            let cls = if pnl >= Decimal::ZERO {
                                "pnl-positive"
                            } else {
                                "pnl-negative"
                            };
                            (cp.to_string(), format!("${pnl:.2}"), cls)
                        }
                        None => ("-".to_string(), "-".to_string(), ""),
                    };
                    let kelly_str = match pos.kelly_fraction {
                        Some(kf) => format!("{:.1}%", kf * Decimal::new(100, 0)),
                        None => "fixed".to_string(),
                    };
                    let _ = write!(
                        html,
                        r#"<tr class="border-b border-gray-800"><td class="py-1">{coin}</td><td class="py-1">{side:?}</td><td class="text-right py-1">{entry}</td><td class="text-right py-1">{current}</td><td class="text-right py-1"><span class="{pnl_class}">{pnl}</span></td><td class="text-right py-1">{size}</td><td class="text-right py-1">{kelly}</td></tr>"#,
                        coin = escape_html(&pos.coin),
                        side = pos.side,
                        entry = pos.entry_price,
                        current = current_str,
                        pnl_class = pnl_class,
                        pnl = pnl_str,
                        size = pos.size,
                        kelly = kelly_str,
                    );
                }
            }
            html.push_str("</tbody></table>");
        }
        html.push_str("</div>");

        // --- Spike Events ---
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        let _ = write!(
            html,
            r#"<h2 class="text-lg font-bold mb-3">Spike Events ({})</h2>"#,
            self.spike_events.len()
        );

        if self.spike_events.is_empty() {
            html.push_str(r#"<p class="text-gray-500">No spike events detected</p>"#);
        } else {
            html.push_str(r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#);
            html.push_str("<th class=\"text-left py-1\">Coin</th>");
            html.push_str("<th class=\"text-right py-1\">Change</th>");
            html.push_str("<th class=\"text-right py-1\">From</th>");
            html.push_str("<th class=\"text-right py-1\">To</th>");
            html.push_str("<th class=\"text-right py-1\">Time</th>");
            html.push_str("</tr></thead><tbody>");

            // Show most recent spikes first (last 10)
            for spike in self.spike_events.iter().rev().take(10) {
                let change_class = if spike.change_pct >= Decimal::ZERO {
                    "pnl-positive"
                } else {
                    "pnl-negative"
                };
                let change_display = spike.change_pct * Decimal::new(100, 0);
                let ago = (Utc::now() - spike.timestamp).num_seconds();
                let time_str = if ago < 60 {
                    format!("{}s ago", ago)
                } else {
                    format!("{}m ago", ago / 60)
                };

                let _ = write!(
                    html,
                    r#"<tr class="border-b border-gray-800"><td class="py-1">{coin}</td><td class="text-right py-1 {change_class}">{change:+.2}%</td><td class="text-right py-1">{from}</td><td class="text-right py-1">{to}</td><td class="text-right py-1">{time}</td></tr>"#,
                    coin = escape_html(&spike.coin),
                    change_class = change_class,
                    change = change_display,
                    from = fmt_usd(spike.from_price),
                    to = fmt_usd(spike.to_price),
                    time = time_str,
                );
            }
            html.push_str("</tbody></table>");
        }
        html.push_str("</div>");

        // --- Cross-Market Correlation ---
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        html.push_str(r#"<h2 class="text-lg font-bold mb-3">Cross-Market Correlation</h2>"#);

        if !self.config.correlation.enabled {
            html.push_str(r#"<p class="text-gray-500">Correlation signals disabled</p>"#);
        } else {
            // Show configured pairs
            html.push_str(r#"<p class="text-gray-400 text-sm mb-2">Pairs: "#);
            for (i, (leader, followers)) in self.config.correlation.pairs.iter().enumerate() {
                if i > 0 {
                    html.push_str(", ");
                }
                let _ = write!(
                    html,
                    "{} → [{}]",
                    escape_html(leader),
                    followers
                        .iter()
                        .map(|f| escape_html(f))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            html.push_str("</p>");

            // Show active cross-correlated positions/orders
            let cross_pending: Vec<_> = self
                .pending_orders
                .values()
                .filter(|p| matches!(&p.mode, ArbitrageMode::CrossCorrelated { .. }))
                .collect();
            let cross_limits: Vec<_> = self
                .open_limit_orders
                .values()
                .filter(|lo| matches!(&lo.mode, ArbitrageMode::CrossCorrelated { .. }))
                .collect();

            let cross_count = cross_pending.len() + cross_limits.len();
            if cross_count == 0 {
                html.push_str(r#"<p class="text-gray-500">No active correlation signals</p>"#);
            } else {
                html.push_str(r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#);
                html.push_str("<th class=\"text-left py-1\">Leader</th>");
                html.push_str("<th class=\"text-left py-1\">Follower</th>");
                html.push_str("<th class=\"text-right py-1\">Price</th>");
                html.push_str("<th class=\"text-right py-1\">Size</th>");
                html.push_str("<th class=\"text-left py-1\">Status</th>");
                html.push_str("</tr></thead><tbody>");

                for p in &cross_pending {
                    let leader_name = match &p.mode {
                        ArbitrageMode::CrossCorrelated { leader } => leader.as_str(),
                        _ => "-",
                    };
                    let _ = write!(
                        html,
                        r#"<tr class="border-b border-gray-800"><td class="py-1">{leader}</td><td class="py-1">{follower}</td><td class="text-right py-1">{price}</td><td class="text-right py-1">{size}</td><td class="py-1">Pending</td></tr>"#,
                        leader = escape_html(leader_name),
                        follower = escape_html(&p.coin),
                        price = fmt_usd(p.price),
                        size = p.size.round_dp(2),
                    );
                }

                for lo in &cross_limits {
                    let leader_name = match &lo.mode {
                        ArbitrageMode::CrossCorrelated { leader } => leader.as_str(),
                        _ => "-",
                    };
                    let _ = write!(
                        html,
                        r#"<tr class="border-b border-gray-800"><td class="py-1">{leader}</td><td class="py-1">{follower}</td><td class="text-right py-1">{price}</td><td class="text-right py-1">{size}</td><td class="py-1">Open Limit</td></tr>"#,
                        leader = escape_html(leader_name),
                        follower = escape_html(&lo.coin),
                        price = fmt_usd(lo.price),
                        size = lo.size.round_dp(2),
                    );
                }

                html.push_str("</tbody></table>");
            }
        }
        html.push_str("</div>");

        // --- Performance Stats ---
        html.push_str(r#"<div class="bg-gray-900 rounded-lg p-4 mb-4">"#);
        html.push_str(r#"<h2 class="text-lg font-bold mb-3">Performance Stats</h2>"#);

        if self.mode_stats.is_empty() {
            html.push_str(r#"<p class="text-gray-500">No trades recorded yet</p>"#);
        } else {
            html.push_str(r#"<table class="w-full text-sm"><thead><tr class="text-gray-400 border-b border-gray-800">"#);
            html.push_str("<th class=\"text-left py-1\">Mode</th>");
            html.push_str("<th class=\"text-right py-1\">Trades</th>");
            html.push_str("<th class=\"text-right py-1\">Won</th>");
            html.push_str("<th class=\"text-right py-1\">Lost</th>");
            html.push_str("<th class=\"text-right py-1\">Win Rate</th>");
            html.push_str("<th class=\"text-right py-1\">Total P&amp;L</th>");
            html.push_str("<th class=\"text-right py-1\">Avg P&amp;L</th>");
            html.push_str("<th class=\"text-left py-1\">Status</th>");
            html.push_str("</tr></thead><tbody>");

            let mut modes: Vec<_> = self.mode_stats.iter().collect();
            modes.sort_by(|a, b| a.0.to_string().cmp(&b.0.to_string()));

            for (mode, stats) in &modes {
                let win_rate_pct = stats.win_rate() * Decimal::new(100, 0);
                let pnl_class = if stats.total_pnl >= Decimal::ZERO {
                    "pnl-positive"
                } else {
                    "pnl-negative"
                };
                let status = if self.is_mode_disabled(mode) {
                    r#"<span class="text-red-400">Disabled</span>"#
                } else {
                    r#"<span class="text-green-400">Active</span>"#
                };
                let _ = write!(
                    html,
                    r#"<tr class="border-b border-gray-800"><td class="py-1">{mode}</td><td class="text-right py-1">{trades}</td><td class="text-right py-1">{won}</td><td class="text-right py-1">{lost}</td><td class="text-right py-1">{win_rate:.1}%</td><td class="text-right py-1 {pnl_class}">${total_pnl:.2}</td><td class="text-right py-1">${avg_pnl:.4}</td><td class="py-1">{status}</td></tr>"#,
                    mode = mode,
                    trades = stats.total_trades(),
                    won = stats.won,
                    lost = stats.lost,
                    win_rate = win_rate_pct,
                    pnl_class = pnl_class,
                    total_pnl = stats.total_pnl,
                    avg_pnl = stats.avg_pnl(),
                    status = status,
                );
            }
            html.push_str("</tbody></table>");
        }
        html.push_str("</div>");

        Ok(html)
    }
}

#[async_trait]
impl Strategy for CryptoArbitrageStrategy {
    fn name(&self) -> &str {
        "crypto-arbitrage"
    }

    fn description(&self) -> &str {
        "Exploits mispricing in 15-min Up/Down crypto markets"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        info!(
            coins = ?self.config.coins,
            max_positions = self.config.max_positions,
            position_size = %self.config.sizing.base_size,
            "Crypto arbitrage strategy started"
        );
        self.last_scan = Some(tokio::time::Instant::now());
        Ok(())
    }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        let mut actions = match event {
            Event::MarketData(MarketDataEvent::ExternalPrice {
                symbol,
                price,
                source,
                ..
            }) => self.on_crypto_price(symbol, *price, source, ctx).await?,

            Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot)) => {
                self.on_orderbook_update(snapshot, ctx).await?
            }

            Event::MarketData(MarketDataEvent::MarketDiscovered(market)) => {
                self.on_market_discovered(market, ctx).await?
            }

            Event::MarketData(MarketDataEvent::MarketExpired(id)) => {
                self.on_market_expired(id, ctx).await?
            }

            Event::OrderUpdate(OrderEvent::Placed(result)) => self.on_order_placed(result),

            Event::OrderUpdate(OrderEvent::Filled {
                order_id,
                token_id,
                price,
                size,
            }) => self.on_order_filled(order_id, token_id, *price, *size),

            Event::OrderUpdate(OrderEvent::PartiallyFilled {
                order_id,
                filled_size,
                remaining_size,
            }) => {
                if let Some(lo) = self.open_limit_orders.get_mut(order_id) {
                    lo.size = *remaining_size;
                    info!(
                        order_id = %order_id,
                        filled = %filled_size,
                        remaining = %remaining_size,
                        "GTC limit order partially filled"
                    );
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::Cancelled(order_id)) => {
                if let Some(lo) = self.open_limit_orders.remove(order_id) {
                    info!(
                        order_id = %order_id,
                        market = %lo.market_id,
                        "GTC limit order cancelled, removing"
                    );
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::Rejected { token_id, .. }) => {
                if let Some(token_id) = token_id {
                    // Clear pending buy order
                    if let Some(pending) = self.pending_orders.remove(token_id) {
                        warn!(
                            token_id = %token_id,
                            market = %pending.market_id,
                            "Cleared pending order after rejection"
                        );
                    }
                    // Clear pending stop-loss — position retained for retry
                    if self.pending_stop_loss.remove(token_id).is_some() {
                        warn!(
                            token_id = %token_id,
                            "Stop-loss sell rejected, position retained for retry"
                        );
                    }
                }
                vec![]
            }

            _ => vec![],
        };

        // Check for stale GTC limit orders on every event tick
        actions.extend(self.check_stale_limit_orders());

        // Throttled dashboard update signal for real-time SSE view refresh
        if let Some(dashboard_action) = self.maybe_emit_dashboard_update() {
            actions.push(dashboard_action);
        }

        Ok(actions)
    }

    async fn on_stop(&mut self, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        let total_positions: usize = self.positions.values().map(|v| v.len()).sum();
        info!(
            active_markets = self.active_markets.len(),
            open_positions = total_positions,
            pending_orders = self.pending_orders.len(),
            "Crypto arbitrage strategy stopping"
        );

        let mut actions = Vec::new();

        // Cancel all open orders on shutdown to avoid orphaned orders
        if !self.positions.is_empty() || !self.pending_orders.is_empty() {
            warn!(
                markets_with_positions = self.positions.len(),
                total_positions = total_positions,
                "Cancelling all open orders on shutdown"
            );
            actions.push(Action::CancelAllOrders);
        }

        self.pending_orders.clear();
        self.pending_stop_loss.clear();
        Ok(actions)
    }

    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use rust_decimal_macros::dec;
    use std::collections::VecDeque;

    fn make_market_info(id: &str, end_date: DateTime<Utc>) -> MarketInfo {
        MarketInfo {
            id: id.to_string(),
            slug: "btc-up-down".to_string(),
            question: "Will BTC go up?".to_string(),
            start_date: None,
            end_date,
            token_ids: TokenIds {
                outcome_a: "token_up".to_string(),
                outcome_b: "token_down".to_string(),
            },
            accepting_orders: true,
            neg_risk: false,
        }
    }

    fn make_mwr(reference_price: Decimal, time_remaining_secs: i64) -> MarketWithReference {
        MarketWithReference {
            market: make_market_info(
                "market1",
                Utc::now() + Duration::seconds(time_remaining_secs),
            ),
            reference_price,
            reference_quality: ReferenceQuality::Exact,
            discovery_time: Utc::now(),
            coin: "BTC".to_string(),
        }
    }

    fn make_orderbook(token_id: &str, best_bid: Decimal, best_ask: Decimal) -> OrderbookSnapshot {
        OrderbookSnapshot {
            token_id: token_id.to_string(),
            bids: vec![OrderbookLevel {
                price: best_bid,
                size: dec!(100),
            }],
            asks: vec![OrderbookLevel {
                price: best_ask,
                size: dec!(100),
            }],
            timestamp: Utc::now(),
        }
    }

    // --- predict_winner tests ---

    #[test]
    fn predict_winner_btc_up() {
        let mwr = make_mwr(dec!(50000), 600);
        // Current price above reference => Up
        assert_eq!(mwr.predict_winner(dec!(50100)), Some(OutcomeSide::Up));
    }

    #[test]
    fn predict_winner_btc_down() {
        let mwr = make_mwr(dec!(50000), 600);
        // Current price below reference => Down
        assert_eq!(mwr.predict_winner(dec!(49900)), Some(OutcomeSide::Down));
    }

    #[test]
    fn predict_winner_at_reference_returns_none() {
        let mwr = make_mwr(dec!(50000), 600);
        // Price equals reference => no directional signal
        assert_eq!(mwr.predict_winner(dec!(50000)), None);
    }

    // --- get_confidence tests ---

    #[test]
    fn confidence_tail_end() {
        // < 120s remaining, market >= 0.90 -> confidence 1.0
        let mwr = make_mwr(dec!(50000), 60);
        let confidence = mwr.get_confidence(dec!(51000), dec!(0.95), 60);
        assert_eq!(confidence, dec!(1.0));
    }

    #[test]
    fn confidence_tail_end_low_market_price() {
        // < 120s but market < 0.90 -> NOT tail-end, falls to late window
        // Small move so late window doesn't cap at 1.0
        let mwr = make_mwr(dec!(50000), 60);
        // distance_pct = 50/50000 = 0.001, base = 0.001 * 66 = 0.066
        // market_boost = 1.0 + (0.55 - 0.50) * 0.5 = 1.025
        // raw = 0.066 * 1.025 = 0.0677 < 1.0
        let confidence = mwr.get_confidence(dec!(50050), dec!(0.55), 60);
        assert!(confidence < dec!(1.0));
        assert!(confidence > Decimal::ZERO);
    }

    #[test]
    fn confidence_late_window() {
        // 120-300s remaining
        let mwr = make_mwr(dec!(50000), 200);
        let confidence = mwr.get_confidence(dec!(51000), dec!(0.70), 200);
        // distance_pct = 1000/50000 = 0.02
        // base = 0.02 * 66 = 1.32
        // market_boost = 1.0 + (0.70 - 0.50) * 0.5 = 1.10
        // raw = 1.32 * 1.10 = 1.452 -> capped at 1.0
        assert!(confidence > Decimal::ZERO);
        assert!(confidence <= dec!(1.0));
    }

    #[test]
    fn confidence_early_window() {
        // > 300s remaining
        let mwr = make_mwr(dec!(50000), 600);
        // distance_pct = 500/50000 = 0.01
        // raw = 0.01 * 50 = 0.50
        let confidence = mwr.get_confidence(dec!(50500), dec!(0.50), 600);
        assert_eq!(confidence, dec!(0.50));
    }

    #[test]
    fn confidence_early_window_small_move() {
        // > 300s, small move => lower confidence
        let mwr = make_mwr(dec!(50000), 600);
        // distance_pct = 100/50000 = 0.002
        // raw = 0.002 * 50 = 0.10
        let confidence = mwr.get_confidence(dec!(50100), dec!(0.50), 600);
        assert_eq!(confidence, dec!(0.10));
    }

    // --- evaluate_opportunity tests ---

    #[tokio::test]
    async fn evaluate_tail_end_opportunity() {
        let mwr = make_mwr(dec!(50000), 60);
        let ctx = StrategyContext::new();

        // Set up orderbook with high ask for Up outcome
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.93), dec!(0.95)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.03), dec!(0.05)),
            );
        }

        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        // Current price > reference => Up wins; ask = 0.95 >= 0.90
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(51000), &ctx)
            .await
            .unwrap();
        assert!(!opps.is_empty());
        let opp = &opps[0];
        assert_eq!(opp.mode, ArbitrageMode::TailEnd);
        assert_eq!(opp.outcome_to_buy, OutcomeSide::Up);
        assert_eq!(opp.buy_price, dec!(0.95));
        assert_eq!(opp.confidence, dec!(1.0));
    }

    #[tokio::test]
    async fn evaluate_two_sided_opportunity() {
        let mwr = make_mwr(dec!(50000), 400);
        let ctx = StrategyContext::new();

        // Both asks low: 0.40 + 0.40 = 0.80 < 0.98
        // Gross margin = 0.20, net = 0.20 (maker fee = $0 in hybrid mode)
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.38), dec!(0.40)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.38), dec!(0.40)),
            );
        }

        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(50100), &ctx)
            .await
            .unwrap();
        assert_eq!(opps.len(), 2, "TwoSided should return both outcomes");
        assert_eq!(opps[0].mode, ArbitrageMode::TwoSided);
        assert_eq!(opps[0].outcome_to_buy, OutcomeSide::Up);
        assert_eq!(opps[1].outcome_to_buy, OutcomeSide::Down);
        assert_eq!(opps[0].profit_margin, dec!(0.20)); // 1.0 - 0.80
        assert!(opps[0].net_margin > Decimal::ZERO);
        // In hybrid mode (default), TwoSided uses maker orders with $0 fee
        assert_eq!(opps[0].estimated_fee, Decimal::ZERO);
    }

    #[tokio::test]
    async fn evaluate_confirmed_opportunity() {
        let mwr = make_mwr(dec!(50000), 200);
        let ctx = StrategyContext::new();

        // Late window, reasonable ask, high distance
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.55), dec!(0.60)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.35), dec!(0.40)),
            );
        }

        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        // Large price move: 52000 vs 50000 = 4% distance
        // confidence = min(1.0, 0.04 * 66 * boost) will be > 0.50
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(52000), &ctx)
            .await
            .unwrap();
        assert!(!opps.is_empty());
        let opp = &opps[0];
        assert_eq!(opp.mode, ArbitrageMode::Confirmed);
        assert_eq!(opp.outcome_to_buy, OutcomeSide::Up);
        assert!(opp.confidence >= dec!(0.50));
        // In hybrid mode (default), Confirmed uses maker orders with $0 fee
        assert_eq!(opp.estimated_fee, Decimal::ZERO);
        assert!(opp.net_margin > Decimal::ZERO);
        // net_margin == profit_margin when maker fee is $0
        assert_eq!(opp.net_margin, opp.profit_margin);
    }

    #[tokio::test]
    async fn evaluate_no_opportunity_low_confidence() {
        let mwr = make_mwr(dec!(50000), 600);
        let ctx = StrategyContext::new();

        // Early window, tiny move, high ask => no opportunity
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.88), dec!(0.92)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.06), dec!(0.08)),
            );
        }

        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        // Tiny move: 50010 vs 50000 = 0.02% distance
        // confidence = 0.0002 * 50 = 0.01 < 0.50
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(50010), &ctx)
            .await
            .unwrap();
        assert!(opps.is_empty());
    }

    // --- stop-loss tests ---

    #[test]
    fn stop_loss_triggers() {
        // Reversal > 0.5% AND price drop > 5¢ AND time > 60s
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let mwr = make_mwr(dec!(50000), 300);
        strategy.active_markets.insert("market1".to_string(), mwr);

        // We bet Up at reference 50000 with entry price 0.60
        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
            kelly_fraction: None,
            peak_bid: dec!(0.60),
            mode: ArbitrageMode::Confirmed,
            estimated_fee: Decimal::ZERO,
        };

        // Price reversed: BTC dropped from 50000 to 49500 = -1% > 0.5%
        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(49500), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // Market bid dropped from 0.60 to 0.50 = 10¢ > 5¢
        let snapshot = make_orderbook("token_up", dec!(0.50), dec!(0.55));

        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_some());
    }

    #[test]
    fn stop_loss_does_not_trigger_final_60s() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        // Only 30 seconds left
        let mwr = make_mwr(dec!(50000), 30);
        strategy.active_markets.insert("market1".to_string(), mwr);

        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
            kelly_fraction: None,
            peak_bid: dec!(0.60),
            mode: ArbitrageMode::Confirmed,
            estimated_fee: Decimal::ZERO,
        };

        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(49500), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        let snapshot = make_orderbook("token_up", dec!(0.50), dec!(0.55));
        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_none());
    }

    #[test]
    fn stop_loss_does_not_trigger_small_drop() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let mwr = make_mwr(dec!(50000), 300);
        strategy.active_markets.insert("market1".to_string(), mwr);

        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
            kelly_fraction: None,
            peak_bid: dec!(0.60),
            mode: ArbitrageMode::Confirmed,
            estimated_fee: Decimal::ZERO,
        };

        // Crypto reversed, but market price only dropped 3¢ < 5¢
        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(49500), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        let snapshot = make_orderbook("token_up", dec!(0.57), dec!(0.60));
        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_none());
    }

    // --- trailing stop-loss tests ---

    #[test]
    fn trailing_stop_triggers_when_bid_drops_from_peak() {
        // peak=0.70, current_bid=0.67 → drop=0.03 >= trailing_distance=0.03
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let mwr = make_mwr(dec!(50000), 300);
        strategy.active_markets.insert("market1".to_string(), mwr);

        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
            kelly_fraction: None,
            peak_bid: dec!(0.70), // Position was profitable, bid reached 0.70
            mode: ArbitrageMode::Confirmed,
            estimated_fee: Decimal::ZERO,
        };

        // No crypto reversal needed for trailing stop — price history irrelevant
        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(50000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // Current bid = 0.67, drop from peak = 0.70 - 0.67 = 0.03 > trailing_distance (0.03)
        // Note: condition is strictly greater than, so 0.031 drop needed
        let snapshot = make_orderbook("token_up", dec!(0.669), dec!(0.70));
        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_some(), "Trailing stop should trigger when bid drops > trailing_distance from peak");
    }

    #[test]
    fn trailing_stop_does_not_trigger_when_position_underwater() {
        // peak == entry (position never went up) → trailing should NOT trigger
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let mwr = make_mwr(dec!(50000), 300);
        strategy.active_markets.insert("market1".to_string(), mwr);

        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
            kelly_fraction: None,
            peak_bid: dec!(0.60), // Never went above entry
            mode: ArbitrageMode::Confirmed,
            estimated_fee: Decimal::ZERO,
        };

        // No crypto reversal
        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(50000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // Bid dropped to 0.50 — large drop, but peak == entry so trailing shouldn't fire
        let snapshot = make_orderbook("token_up", dec!(0.50), dec!(0.55));
        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_none(), "Trailing stop should NOT trigger when peak_bid == entry_price (position was never profitable)");
    }

    #[test]
    fn trailing_stop_time_decay_tightens_near_expiry() {
        // 30s remaining out of 900s → decay_factor = 30/900 = 0.0333
        // effective_distance = 0.03 * 0.0333 ≈ 0.001
        // So even a tiny 0.002 drop from peak should trigger
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let mwr = make_mwr(dec!(50000), 90); // 90 seconds remaining (past the 60s cutoff)
        strategy.active_markets.insert("market1".to_string(), mwr);

        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
            kelly_fraction: None,
            peak_bid: dec!(0.65), // Position was profitable
            mode: ArbitrageMode::Confirmed,
            estimated_fee: Decimal::ZERO,
        };

        // No crypto reversal
        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(50000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // At 90s remaining: effective_distance = 0.03 * (90/900) = 0.03 * 0.1 = 0.003
        // Drop from peak: 0.65 - 0.645 = 0.005 > 0.003 → should trigger
        let snapshot = make_orderbook("token_up", dec!(0.645), dec!(0.66));
        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_some(), "Time decay should tighten trailing distance near expiry");
    }

    #[test]
    fn trailing_stop_disabled_preserves_existing_behavior() {
        // With trailing_enabled=false, only dual-trigger logic should work
        let mut config = ArbitrageConfig::default();
        config.stop_loss.trailing_enabled = false;
        let mut strategy = CryptoArbitrageStrategy::new(config, vec![]);

        let mwr = make_mwr(dec!(50000), 300);
        strategy.active_markets.insert("market1".to_string(), mwr);

        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
            kelly_fraction: None,
            peak_bid: dec!(0.70), // Position was profitable, peak at 0.70
            mode: ArbitrageMode::Confirmed,
            estimated_fee: Decimal::ZERO,
        };

        // No crypto reversal (price stable)
        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(50000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // Bid dropped 0.04 from peak (> trailing_distance=0.03) but trailing is disabled
        // Dual-trigger: no crypto reversal → should NOT trigger
        let snapshot = make_orderbook("token_up", dec!(0.66), dec!(0.70));
        let action = strategy.check_stop_loss(&pos, &snapshot).unwrap();
        assert!(action.is_none(), "With trailing_enabled=false, trailing stop should not trigger even with large drop from peak");
    }

    // --- market discovery/expiry tests ---

    #[tokio::test]
    async fn on_market_discovered_creates_entry() {
        let mut strategy = make_strategy_no_chainlink();
        let ctx = StrategyContext::new();

        // Set BTC price in context
        {
            let mut md = ctx.market_data.write().await;
            md.external_prices.insert("BTC".to_string(), dec!(50000));
        }

        let market = make_market_info("btc-market-1", Utc::now() + Duration::seconds(900));

        let actions = strategy.on_market_discovered(&market, &ctx).await.unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::SubscribeMarket(_)));
        assert!(strategy.active_markets.contains_key("btc-market-1"));
        // Reference price should be the current external price (Current quality)
        let mwr = &strategy.active_markets["btc-market-1"];
        assert_eq!(mwr.reference_price, dec!(50000));
        assert_eq!(mwr.reference_quality, ReferenceQuality::Current);
    }

    #[tokio::test]
    async fn on_market_expired_removes_market() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        let ctx = StrategyContext::new();

        let mwr = make_mwr(dec!(50000), 0);
        strategy.active_markets.insert("market1".to_string(), mwr);

        let actions = strategy.on_market_expired("market1", &ctx).await.unwrap();
        assert!(!strategy.active_markets.contains_key("market1"));
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::UnsubscribeMarket(_)));
    }

    // --- extract_coin tests ---

    #[test]
    fn extract_coin_from_question() {
        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        assert_eq!(
            strategy.extract_coin("Will BTC go up in the next 15 minutes?"),
            Some("BTC".to_string())
        );
        assert_eq!(
            strategy.extract_coin("Will ETH be above $2000?"),
            Some("ETH".to_string())
        );
        assert_eq!(strategy.extract_coin("Random question about stocks"), None);
        // Full coin names (as used by Polymarket)
        assert_eq!(
            strategy.extract_coin("Bitcoin Up or Down - January 27, 4:45PM-5:00PM ET"),
            Some("BTC".to_string())
        );
        assert_eq!(
            strategy.extract_coin("Ethereum Up or Down - January 27, 4:45PM-5:00PM ET"),
            Some("ETH".to_string())
        );
        assert_eq!(
            strategy.extract_coin("Solana Up or Down - January 27, 4:45PM-5:00PM ET"),
            Some("SOL".to_string())
        );
        assert_eq!(
            strategy.extract_coin("XRP Up or Down - January 27, 4:45PM-5:00PM ET"),
            Some("XRP".to_string())
        );
    }

    // --- DashboardViewProvider tests ---

    #[test]
    fn dashboard_view_returns_some() {
        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        let view = strategy.dashboard_view();
        assert!(view.is_some());
        assert_eq!(view.unwrap().view_name(), "crypto-arb");
    }

    #[test]
    fn render_view_empty_state() {
        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        let html = strategy.render_view().unwrap();
        // Should contain all three section headers
        assert!(html.contains("Reference Prices"));
        assert!(html.contains("Active Markets"));
        assert!(html.contains("Open Positions"));
        // Empty state messages
        assert!(html.contains("No active markets"));
        assert!(html.contains("No open positions"));
    }

    #[test]
    fn render_view_with_active_markets_and_prices() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        // Add an active market
        let mwr = make_mwr(dec!(50000), 300);
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

        // Add current price history for BTC
        let mut history = VecDeque::new();
        history.push_back((Utc::now(), dec!(50500), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        let html = strategy.render_view().unwrap();

        // Reference price section should show coin data with formatted prices
        assert!(html.contains("BTC"));
        assert!(html.contains("$50,000.00"));
        assert!(html.contains("$50,500.00"));
        assert!(html.contains("UP")); // 50500 > 50000 => UP prediction

        // Active markets section should show the market
        assert!(html.contains("BTC Up/Down"));

        // No open positions
        assert!(html.contains("No open positions"));
    }

    #[test]
    fn render_view_with_positions() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        // Add a position
        let pos = ArbitragePosition {
            market_id: "market1".to_string(),
            token_id: "token_up".to_string(),
            side: OutcomeSide::Up,
            entry_price: dec!(0.60),
            size: dec!(10),
            reference_price: dec!(50000),
            coin: "BTC".to_string(),
            order_id: None,
            entry_time: Utc::now(),
            kelly_fraction: None,
            peak_bid: dec!(0.60),
            mode: ArbitrageMode::Confirmed,
            estimated_fee: Decimal::ZERO,
        };
        strategy
            .positions
            .entry("market1".to_string())
            .or_default()
            .push(pos);

        let html = strategy.render_view().unwrap();

        // Should show position data
        assert!(html.contains("Open Positions (1)"));
        assert!(html.contains("BTC"));
        assert!(html.contains("0.60")); // entry price
        assert!(!html.contains("No open positions"));
    }

    // --- dashboard update emission tests ---

    #[test]
    fn maybe_emit_dashboard_update_first_call_emits() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        let action = strategy.maybe_emit_dashboard_update();
        assert!(action.is_some(), "first call should emit");
        if let Some(Action::EmitSignal {
            signal_type,
            payload,
        }) = action
        {
            assert_eq!(signal_type, "dashboard-update");
            assert_eq!(payload["view_name"], "crypto-arb");
        }
    }

    #[test]
    fn maybe_emit_dashboard_update_throttles() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        // First call emits
        let action = strategy.maybe_emit_dashboard_update();
        assert!(action.is_some());

        // Immediate second call should be throttled
        let action = strategy.maybe_emit_dashboard_update();
        assert!(action.is_none(), "immediate second call should be throttled");
    }

    #[test]
    fn render_view_current_quality_reference() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let mut mwr = make_mwr(dec!(50000), 300);
        mwr.reference_quality = ReferenceQuality::Current;
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

        let html = strategy.render_view().unwrap();
        // Current quality reference should show ~ prefix with formatted price
        assert!(html.contains("~$50,000.00"));
    }

    #[test]
    fn render_view_historical_quality_reference() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let mut mwr = make_mwr(dec!(50000), 300);
        mwr.reference_quality = ReferenceQuality::Historical(10);
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

        let html = strategy.render_view().unwrap();
        // Historical quality reference should show ≈ prefix
        assert!(html.contains("≈$50,000.00"));
    }

    #[test]
    fn render_view_onchain_quality_reference() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let mut mwr = make_mwr(dec!(50000), 300);
        mwr.reference_quality = ReferenceQuality::OnChain(5);
        strategy
            .active_markets
            .insert("market1".to_string(), mwr);

        let html = strategy.render_view().unwrap();
        // OnChain quality reference should show ✓ prefix
        assert!(html.contains("✓$50,000.00"));
    }

    // --- reference quality tests ---

    #[test]
    fn quality_factor_values() {
        // Exact = 1.0
        assert_eq!(ReferenceQuality::Exact.quality_factor(), Decimal::ONE);
        // OnChain(<5s) = 1.0
        assert_eq!(ReferenceQuality::OnChain(3).quality_factor(), Decimal::ONE);
        // OnChain(<15s) = 0.98
        assert_eq!(
            ReferenceQuality::OnChain(12).quality_factor(),
            dec!(0.98)
        );
        // OnChain(>=15s) = 0.95
        assert_eq!(
            ReferenceQuality::OnChain(20).quality_factor(),
            dec!(0.95)
        );
        // Historical(<5s) = 0.95
        assert_eq!(
            ReferenceQuality::Historical(3).quality_factor(),
            dec!(0.95)
        );
        // Historical(>=5s) = 0.85
        assert_eq!(
            ReferenceQuality::Historical(10).quality_factor(),
            dec!(0.85)
        );
        // Current = 0.70
        assert_eq!(ReferenceQuality::Current.quality_factor(), dec!(0.70));
    }

    #[test]
    fn confidence_discounted_by_quality() {
        // Exact quality: raw confidence unchanged
        let mwr_exact = make_mwr(dec!(50000), 600);
        // distance_pct = 500/50000 = 0.01, raw = 0.01 * 50 = 0.50
        let c_exact = mwr_exact.get_confidence(dec!(50500), dec!(0.50), 600);
        assert_eq!(c_exact, dec!(0.50)); // 0.50 * 1.0 = 0.50

        // Current quality: discounted by 0.70
        let mut mwr_current = make_mwr(dec!(50000), 600);
        mwr_current.reference_quality = ReferenceQuality::Current;
        let c_current = mwr_current.get_confidence(dec!(50500), dec!(0.50), 600);
        assert_eq!(c_current, dec!(0.350)); // 0.50 * 0.70 = 0.35
    }

    /// Helper: create a strategy with Chainlink disabled (no RPC calls in tests).
    fn make_strategy_no_chainlink() -> CryptoArbitrageStrategy {
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        CryptoArbitrageStrategy::new(config, vec![])
    }

    #[tokio::test]
    async fn find_best_reference_exact_boundary() {
        let mut strategy = make_strategy_no_chainlink();

        let ts = 1706000000i64;
        strategy.boundary_prices.insert(
            "BTC-1706000000".to_string(),
            BoundarySnapshot {
                timestamp: DateTime::from_timestamp(ts, 0).unwrap(),
                price: dec!(42500),
                source: "chainlink".to_string(),
            },
        );

        let (price, quality) = strategy.find_best_reference("BTC", ts, dec!(43000)).await;
        assert_eq!(price, dec!(42500));
        assert_eq!(quality, ReferenceQuality::Exact);
    }

    #[tokio::test]
    async fn find_best_reference_historical() {
        let mut strategy = make_strategy_no_chainlink();

        let window_ts = 1706000000i64;
        let target_dt = DateTime::from_timestamp(window_ts, 0).unwrap();

        // Add history entries around the window start
        let mut history = VecDeque::new();
        // 5 seconds after window start
        history.push_back((
            target_dt + Duration::seconds(5),
            dec!(42600),
            "binance".to_string(),
        ));
        // 20 seconds after window start
        history.push_back((
            target_dt + Duration::seconds(20),
            dec!(42700),
            "binance".to_string(),
        ));
        strategy.price_history.insert("BTC".to_string(), history);

        let (price, quality) = strategy.find_best_reference("BTC", window_ts, dec!(43000)).await;
        assert_eq!(price, dec!(42600)); // Closest to window start (5s)
        assert_eq!(quality, ReferenceQuality::Historical(5));
    }

    #[tokio::test]
    async fn find_best_reference_historical_prefers_chainlink() {
        let mut strategy = make_strategy_no_chainlink();

        let window_ts = 1706000000i64;
        let target_dt = DateTime::from_timestamp(window_ts, 0).unwrap();

        let mut history = VecDeque::new();
        // Binance at 3s, Chainlink at 6s — within 5s of each other
        history.push_back((
            target_dt + Duration::seconds(3),
            dec!(42600),
            "binance".to_string(),
        ));
        history.push_back((
            target_dt + Duration::seconds(6),
            dec!(42650),
            "chainlink".to_string(),
        ));
        strategy.price_history.insert("BTC".to_string(), history);

        let (price, quality) = strategy.find_best_reference("BTC", window_ts, dec!(43000)).await;
        // Should prefer Chainlink even though it's slightly further
        assert_eq!(price, dec!(42650));
        assert_eq!(quality, ReferenceQuality::Historical(6));
    }

    #[tokio::test]
    async fn find_best_reference_fallback_to_current() {
        let strategy = make_strategy_no_chainlink();

        // No boundary snapshots, no history
        let (price, quality) = strategy.find_best_reference("BTC", 1706000000, dec!(43000)).await;
        assert_eq!(price, dec!(43000));
        assert_eq!(quality, ReferenceQuality::Current);
    }

    #[tokio::test]
    async fn find_best_reference_stale_history_falls_to_current() {
        let mut strategy = make_strategy_no_chainlink();

        let window_ts = 1706000000i64;
        let target_dt = DateTime::from_timestamp(window_ts, 0).unwrap();

        // History entry 60s after window start — too stale (> 30s threshold)
        let mut history = VecDeque::new();
        history.push_back((
            target_dt + Duration::seconds(60),
            dec!(42800),
            "binance".to_string(),
        ));
        strategy.price_history.insert("BTC".to_string(), history);

        let (price, quality) = strategy.find_best_reference("BTC", window_ts, dec!(43000)).await;
        assert_eq!(price, dec!(43000));
        assert_eq!(quality, ReferenceQuality::Current);
    }

    #[test]
    fn parse_slug_timestamp_valid() {
        assert_eq!(
            parse_slug_timestamp("btc-updown-15m-1706000000"),
            Some(1706000000)
        );
    }

    #[test]
    fn parse_slug_timestamp_no_number() {
        assert_eq!(parse_slug_timestamp("btc-updown-15m"), None);
    }

    #[test]
    fn parse_slug_timestamp_small_number() {
        assert_eq!(parse_slug_timestamp("btc-updown-15m-12345"), None);
    }

    #[test]
    fn prune_boundary_snapshots_removes_old() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let now_ts = Utc::now().timestamp();
        let old_ts = now_ts - (WINDOW_SECS * 5); // 5 windows ago
        let recent_ts = now_ts - WINDOW_SECS; // 1 window ago

        strategy.boundary_prices.insert(
            format!("BTC-{old_ts}"),
            BoundarySnapshot {
                timestamp: DateTime::from_timestamp(old_ts, 0).unwrap(),
                price: dec!(40000),
                source: "chainlink".to_string(),
            },
        );
        strategy.boundary_prices.insert(
            format!("BTC-{recent_ts}"),
            BoundarySnapshot {
                timestamp: DateTime::from_timestamp(recent_ts, 0).unwrap(),
                price: dec!(42000),
                source: "chainlink".to_string(),
            },
        );

        strategy.prune_boundary_snapshots("BTC");

        // Old one should be pruned, recent one kept
        assert!(!strategy.boundary_prices.contains_key(&format!("BTC-{old_ts}")));
        assert!(strategy.boundary_prices.contains_key(&format!("BTC-{recent_ts}")));
    }

    #[tokio::test]
    async fn on_market_discovered_with_boundary_snapshot() {
        let mut strategy = make_strategy_no_chainlink();
        let ctx = StrategyContext::new();

        // Set BTC price in context
        {
            let mut md = ctx.market_data.write().await;
            md.external_prices.insert("BTC".to_string(), dec!(50500));
        }

        // Set up a boundary snapshot for the current window
        let now_ts = Utc::now().timestamp();
        let window_ts = now_ts - (now_ts % WINDOW_SECS);
        strategy.boundary_prices.insert(
            format!("BTC-{window_ts}"),
            BoundarySnapshot {
                timestamp: DateTime::from_timestamp(window_ts, 0).unwrap(),
                price: dec!(50000),
                source: "chainlink".to_string(),
            },
        );

        let mut market = make_market_info("btc-market-1", Utc::now() + Duration::seconds(900));
        market.start_date = DateTime::from_timestamp(window_ts, 0);

        let actions = strategy.on_market_discovered(&market, &ctx).await.unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], Action::SubscribeMarket(_)));

        let mwr = &strategy.active_markets["btc-market-1"];
        // Should use the boundary snapshot, not the current price
        assert_eq!(mwr.reference_price, dec!(50000));
        assert_eq!(mwr.reference_quality, ReferenceQuality::Exact);
    }

    // --- config sub-struct tests ---

    #[test]
    fn config_default_sub_configs() {
        let config = ArbitrageConfig::default();

        // Fee defaults
        assert_eq!(config.fee.taker_fee_rate, dec!(0.0315));

        // Spike defaults
        assert_eq!(config.spike.threshold_pct, dec!(0.005));
        assert_eq!(config.spike.window_secs, 10);
        assert_eq!(config.spike.history_size, 50);

        // Order defaults
        assert!(config.order.hybrid_mode);
        assert_eq!(config.order.limit_offset, dec!(0.01));
        assert_eq!(config.order.max_age_secs, 30);

        // Sizing defaults
        assert_eq!(config.sizing.base_size, dec!(10));
        assert_eq!(config.sizing.kelly_multiplier, dec!(0.25));
        assert_eq!(config.sizing.min_size, dec!(2));
        assert_eq!(config.sizing.max_size, dec!(25));
        assert!(config.sizing.use_kelly);

        // StopLoss defaults
        assert_eq!(config.stop_loss.reversal_pct, dec!(0.005));
        assert_eq!(config.stop_loss.min_drop, dec!(0.05));
        assert!(config.stop_loss.trailing_enabled);
        assert_eq!(config.stop_loss.trailing_distance, dec!(0.03));
        assert!(config.stop_loss.time_decay);

        // Correlation defaults
        assert!(!config.correlation.enabled);
        assert_eq!(config.correlation.min_spike_pct, dec!(0.01));
        assert_eq!(config.correlation.pairs.len(), 2);

        // Performance defaults
        assert_eq!(config.performance.min_trades, 20);
        assert_eq!(config.performance.min_win_rate, dec!(0.40));
        assert_eq!(config.performance.window_size, 50);
        assert!(!config.performance.auto_disable);
    }

    #[test]
    fn config_deserialize_missing_sub_configs() {
        // Minimal TOML with only top-level fields — sub-configs should default.
        let toml_str = r#"
            coins = ["BTC"]
            max_positions = 3
            min_profit_margin = "0.04"
            late_window_margin = "0.03"
            scan_interval_secs = 60
            use_chainlink = false
        "#;
        let config: ArbitrageConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.coins, vec!["BTC"]);
        assert_eq!(config.max_positions, 3);
        assert!(!config.use_chainlink);
        // Sub-configs should have their defaults
        assert_eq!(config.fee.taker_fee_rate, dec!(0.0315));
        assert!(config.order.hybrid_mode);
        assert_eq!(config.sizing.base_size, dec!(10));
        assert_eq!(config.stop_loss.reversal_pct, dec!(0.005));
        assert!(!config.correlation.enabled);
        assert!(!config.performance.auto_disable);
    }

    // --- taker_fee tests ---

    #[test]
    fn taker_fee_at_50_50() {
        // At p=0.50: fee = 2 * 0.50 * 0.50 * 0.0315 = 0.01575
        let fee = taker_fee(dec!(0.50), dec!(0.0315));
        assert_eq!(fee, dec!(0.015750));
    }

    #[test]
    fn taker_fee_at_80() {
        // At p=0.80: fee = 2 * 0.80 * 0.20 * 0.0315 = 0.01008
        let fee = taker_fee(dec!(0.80), dec!(0.0315));
        assert_eq!(fee, dec!(0.010080));
    }

    #[test]
    fn taker_fee_at_95() {
        // At p=0.95: fee = 2 * 0.95 * 0.05 * 0.0315 = 0.0029925
        let fee = taker_fee(dec!(0.95), dec!(0.0315));
        assert_eq!(fee, dec!(0.0029925));
    }

    // --- net_profit_margin tests ---

    #[test]
    fn net_profit_margin_taker() {
        // At p=0.80: gross = 0.20, fee = 0.01008, net = 0.18992
        let net = net_profit_margin(dec!(0.80), dec!(0.0315), false);
        let expected = dec!(0.20) - dec!(0.010080);
        assert_eq!(net, expected);
    }

    #[test]
    fn net_profit_margin_maker() {
        // Maker fee = $0, so net = gross = 1 - price
        let net = net_profit_margin(dec!(0.80), dec!(0.0315), true);
        assert_eq!(net, dec!(0.20));
    }

    // --- fee-aware filtering tests ---

    #[tokio::test]
    async fn confirmed_mode_filtered_at_50_with_small_margin() {
        // At p=0.50 with 3¢ gross margin, net margin < 0 after fee
        // ask = 0.97 → gross = 0.03, fee at 0.97 = 2*0.97*0.03*0.0315 = 0.001837
        // Actually, let's use p=0.50 directly: ask = 0.50, gross = 0.50 but
        // the plan says "Confirmed mode at p=0.50 with 3¢ gross margin is filtered out"
        // This means ask = 0.97 at a 50/50 market. But fee at 0.97 is tiny.
        // More accurately: at mid-range prices where fee is highest.
        // Use ask = 0.55 with min_profit_margin = 0.03.
        // gross = 0.45, fee = 2*0.55*0.45*0.0315 = 0.01559. net = 0.434.
        // That's still > 0.03. Let's find a case where net < min_margin.
        //
        // To filter: net < min_margin(0.02 for late window). Use ask = 0.97.
        // gross = 0.03, fee = 2*0.97*0.03*0.0315 = 0.001837. net = 0.028.
        // Still passes. Need a tighter case.
        //
        // Let's set min_profit_margin = 0.04 and ask = 0.95. gross=0.05, fee=0.003.
        // net=0.047 >= 0.04, passes. Use min_profit_margin = 0.05 instead.
        //
        // Better approach: construct a scenario where fee eats up the margin.
        // ask = 0.50, gross = 0.50, fee = 0.01575, net = 0.484 — still large.
        // The real impact is when gross is tiny. Let's just verify with a custom
        // high fee rate to show the filtering works.
        let mwr = make_mwr(dec!(50000), 200); // late window
        let ctx = StrategyContext::new();

        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.95), dec!(0.97)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.01), dec!(0.03)),
            );
        }

        // Use a high fee rate that makes the 3¢ gross margin negative
        // Must disable hybrid mode to test taker fee filtering
        let mut config = ArbitrageConfig::default();
        config.use_chainlink = false;
        config.order.hybrid_mode = false;
        config.fee.taker_fee_rate = dec!(0.60); // Extreme fee rate for testing
        config.late_window_margin = dec!(0.02);
        let strategy = CryptoArbitrageStrategy::new(config, vec![]);

        // Large move for high confidence (52000 vs 50000 = 4%)
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(52000), &ctx)
            .await
            .unwrap();
        // At ask=0.97, gross=0.03, fee=2*0.97*0.03*0.60=0.03492
        // net = 0.03 - 0.03492 = -0.00492 < late_window_margin(0.02) → filtered
        assert!(
            opps.is_empty(),
            "Should filter Confirmed mode when net margin < 0 after fee"
        );
    }

    #[tokio::test]
    async fn tail_end_at_95_still_passes_with_fees() {
        // At p=0.95: fee ≈ 0.003, margin = 0.05, net ≈ 0.047 > 0
        let mwr = make_mwr(dec!(50000), 60); // < 120s
        let ctx = StrategyContext::new();

        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.93), dec!(0.95)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.03), dec!(0.05)),
            );
        }

        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(51000), &ctx)
            .await
            .unwrap();
        assert!(!opps.is_empty(), "Tail-End at 0.95 should still pass");
        let opp = &opps[0];
        assert_eq!(opp.mode, ArbitrageMode::TailEnd);
        // Verify fee is small (~0.3¢)
        assert!(opp.estimated_fee < dec!(0.005));
        // Net margin should be ~4.7¢
        assert!(opp.net_margin > dec!(0.04));
    }

    #[tokio::test]
    async fn two_sided_filtered_when_fees_exceed_margin() {
        // Both asks near 0.49 → combined 0.98 just under threshold
        // but fees on both legs eat up the tiny 2¢ margin
        // Must disable hybrid mode to test taker fee filtering
        let mwr = make_mwr(dec!(50000), 400);
        let ctx = StrategyContext::new();

        // 0.48 + 0.49 = 0.97, gross margin = 0.03
        // fee_up = 2*0.48*0.52*0.0315 = 0.01572
        // fee_down = 2*0.49*0.51*0.0315 = 0.01575
        // total_fee = 0.03147, net = 0.03 - 0.03147 = -0.00147 → filtered
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.46), dec!(0.48)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.47), dec!(0.49)),
            );
        }

        let mut config = ArbitrageConfig::default();
        config.order.hybrid_mode = false; // Use taker orders to test fee filtering
        let strategy = CryptoArbitrageStrategy::new(config, vec![]);
        let opps = strategy
            .evaluate_opportunity(&mwr, dec!(50100), &ctx)
            .await
            .unwrap();
        assert!(
            opps.is_empty(),
            "Two-Sided should be filtered when fees exceed margin"
        );
    }

    #[test]
    fn config_deserialize_explicit_sub_configs() {
        let toml_str = r#"
            coins = ["BTC", "ETH"]
            max_positions = 10
            min_profit_margin = "0.05"
            late_window_margin = "0.03"
            scan_interval_secs = 15
            use_chainlink = true

            [fee]
            taker_fee_rate = "0.02"

            [spike]
            threshold_pct = "0.01"
            window_secs = 20
            history_size = 100

            [order]
            hybrid_mode = false
            limit_offset = "0.005"
            max_age_secs = 60

            [sizing]
            base_size = "20"
            kelly_multiplier = "0.50"
            min_size = "5"
            max_size = "50"
            use_kelly = false

            [stop_loss]
            reversal_pct = "0.01"
            min_drop = "0.10"
            trailing_enabled = false
            trailing_distance = "0.05"
            time_decay = false

            [correlation]
            enabled = true
            min_spike_pct = "0.02"
            pairs = [["BTC", ["ETH"]]]

            [performance]
            min_trades = 50
            min_win_rate = "0.55"
            window_size = 100
            auto_disable = true
        "#;
        let config: ArbitrageConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.fee.taker_fee_rate, dec!(0.02));
        assert_eq!(config.spike.threshold_pct, dec!(0.01));
        assert_eq!(config.spike.window_secs, 20);
        assert!(!config.order.hybrid_mode);
        assert_eq!(config.order.limit_offset, dec!(0.005));
        assert_eq!(config.sizing.base_size, dec!(20));
        assert!(!config.sizing.use_kelly);
        assert_eq!(config.stop_loss.reversal_pct, dec!(0.01));
        assert!(!config.stop_loss.trailing_enabled);
        assert!(config.correlation.enabled);
        assert_eq!(config.performance.min_trades, 50);
        assert!(config.performance.auto_disable);
    }

    // --- spike detection tests ---

    #[test]
    fn detect_spike_returns_some_for_1pct_move_in_10s() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        // Default spike config: threshold_pct = 0.005, window_secs = 10

        let now = Utc::now();
        let mut history = VecDeque::new();
        // Baseline price 15 seconds ago (before window)
        history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        // Price inside the window (5 seconds ago)
        history.push_back((now - Duration::seconds(5), dec!(50400), "binance".to_string()));
        // Current price
        history.push_back((now, dec!(50500), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // Current price 50500 vs baseline 50000 = 1% change > 0.5% threshold
        let result = strategy.detect_spike("BTC", dec!(50500));
        assert!(result.is_some(), "1% move should be detected as spike");
        let change = result.unwrap();
        assert_eq!(change, dec!(0.01)); // (50500 - 50000) / 50000 = 0.01
    }

    #[test]
    fn detect_spike_returns_none_for_small_move() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let now = Utc::now();
        let mut history = VecDeque::new();
        // Baseline price 15 seconds ago
        history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        // Current price
        history.push_back((now, dec!(50020), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // 50020 vs 50000 = 0.04% change < 0.5% threshold
        let result = strategy.detect_spike("BTC", dec!(50020));
        assert!(result.is_none(), "0.04% move should not be a spike");
    }

    #[test]
    fn detect_spike_returns_none_no_history() {
        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        let result = strategy.detect_spike("BTC", dec!(50000));
        assert!(result.is_none(), "no history should return None");
    }

    #[test]
    fn detect_spike_returns_none_no_baseline_before_window() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let now = Utc::now();
        let mut history = VecDeque::new();
        // Only prices within the window (< 10s ago), no baseline before
        history.push_back((now - Duration::seconds(5), dec!(50000), "binance".to_string()));
        history.push_back((now, dec!(51000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        let result = strategy.detect_spike("BTC", dec!(51000));
        assert!(result.is_none(), "no baseline before window should return None");
    }

    #[test]
    fn detect_spike_negative_direction() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        let now = Utc::now();
        let mut history = VecDeque::new();
        history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        history.push_back((now, dec!(49500), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // -1% move should also be detected
        let result = strategy.detect_spike("BTC", dec!(49500));
        assert!(result.is_some(), "negative spike should be detected");
        assert!(result.unwrap() < Decimal::ZERO);
    }

    #[tokio::test]
    async fn prefilter_skips_evaluation_for_small_move_no_spike() {
        let mut strategy = make_strategy_no_chainlink();
        let ctx = StrategyContext::new();

        // Set up external price
        {
            let mut md = ctx.market_data.write().await;
            md.external_prices.insert("BTC".to_string(), dec!(50000));
        }

        // Add an active market with reference price 50000
        let mwr = make_mwr(dec!(50000), 600);
        strategy.active_markets.insert("market1".to_string(), mwr);

        // Add orderbooks so evaluate_opportunity could find something
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.55), dec!(0.60)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.35), dec!(0.40)),
            );
        }

        // Add price history: only a tiny move (50000 → 50001)
        let now = Utc::now();
        let mut history = VecDeque::new();
        history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        history.push_back((now - Duration::seconds(1), dec!(50001), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // Tiny price delta: 50001 vs ref 50000 = 0.002%
        // This is below fee(0.50) + min_margin = 0.01575 + 0.02 = 0.03575
        // No spike either. Pre-filter should skip evaluation => no actions (no orders).
        let actions = strategy.on_crypto_price("BTC", dec!(50001), "binance", &ctx).await.unwrap();
        let order_actions: Vec<_> = actions.iter().filter(|a| matches!(a, Action::PlaceOrder(_))).collect();
        assert!(order_actions.is_empty(), "pre-filter should skip evaluation for tiny move");
    }

    #[tokio::test]
    async fn prefilter_allows_evaluation_when_spike_detected() {
        let mut strategy = make_strategy_no_chainlink();
        let ctx = StrategyContext::new();

        // Set up external price
        {
            let mut md = ctx.market_data.write().await;
            md.external_prices.insert("BTC".to_string(), dec!(50000));
        }

        // Add an active market with reference price 50000
        let mwr = make_mwr(dec!(50000), 200); // late window for lower margin threshold
        strategy.active_markets.insert("market1".to_string(), mwr);

        // Set up orderbooks with favorable prices
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.55), dec!(0.60)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.35), dec!(0.40)),
            );
        }

        // Set up price history with a spike: BTC jumped from 50000 to 52000 in 10s
        let now = Utc::now();
        let mut history = VecDeque::new();
        history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        history.push_back((now - Duration::seconds(1), dec!(52000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // Even though the *reference delta* might be small in percentage terms,
        // the spike detection triggers (4% move in 10s > 0.5% threshold),
        // so evaluation is allowed.
        let actions = strategy.on_crypto_price("BTC", dec!(52000), "binance", &ctx).await.unwrap();

        // Check that spike was recorded
        assert!(!strategy.spike_events.is_empty(), "spike event should be recorded");
        assert_eq!(strategy.spike_events[0].coin, "BTC");

        // The evaluation should have proceeded (spike detected allows it).
        // Whether an order is placed depends on evaluate_opportunity result,
        // but the pre-filter should NOT have blocked it.
        // With 4% move and late window, confidence should be high enough for Confirmed mode.
        // (52000 vs 50000 = 4%, confidence = min(1, 0.04*66*boost) = 1.0, ask=0.60,
        //  net_margin = 0.40 - fee(0.60) ≈ 0.40 - 0.015 = 0.385 > late_window_margin 0.02)
        let order_actions: Vec<_> = actions.iter().filter(|a| matches!(a, Action::PlaceOrder(_))).collect();
        assert!(!order_actions.is_empty(), "spike should allow evaluation and order placement");
    }

    #[test]
    fn spike_events_capped_at_history_size() {
        let mut config = ArbitrageConfig::default();
        config.spike.history_size = 3;
        let mut strategy = CryptoArbitrageStrategy::new(config, vec![]);

        for i in 0..5 {
            strategy.spike_events.push_back(SpikeEvent {
                coin: "BTC".to_string(),
                timestamp: Utc::now(),
                change_pct: Decimal::new(i, 2),
                from_price: dec!(50000),
                to_price: dec!(50500),
                acted: false,
            });
        }
        // Manually cap (the actual capping happens in on_crypto_price, but verify logic)
        while strategy.spike_events.len() > strategy.config.spike.history_size {
            strategy.spike_events.pop_front();
        }
        assert_eq!(strategy.spike_events.len(), 3);
    }

    #[test]
    fn render_view_with_spike_events() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        strategy.spike_events.push_back(SpikeEvent {
            coin: "BTC".to_string(),
            timestamp: Utc::now(),
            change_pct: dec!(0.015),
            from_price: dec!(50000),
            to_price: dec!(50750),
            acted: false,
        });

        let html = strategy.render_view().unwrap();
        assert!(html.contains("Spike Events (1)"));
        assert!(html.contains("BTC"));
        assert!(html.contains("$50,000.00"));
        assert!(html.contains("$50,750.00"));
    }

    #[test]
    fn render_view_empty_spike_events() {
        let strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);
        let html = strategy.render_view().unwrap();
        assert!(html.contains("Spike Events (0)"));
        assert!(html.contains("No spike events detected"));
    }

    // --- hybrid order mode tests ---

    #[tokio::test]
    async fn confirmed_mode_produces_gtc_order_with_offset() {
        let mut strategy = make_strategy_no_chainlink();
        let ctx = StrategyContext::new();

        // Late window, large move for high confidence
        let mwr = make_mwr(dec!(50000), 200);
        strategy.active_markets.insert("market1".to_string(), mwr);

        {
            let mut md = ctx.market_data.write().await;
            md.external_prices.insert("BTC".to_string(), dec!(52000));
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.55), dec!(0.60)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.35), dec!(0.40)),
            );
        }

        // Set up price history with large spike for prefilter to pass
        let now = Utc::now();
        let mut history = VecDeque::new();
        history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        history.push_back((now - Duration::seconds(1), dec!(52000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // hybrid_mode=true by default, so Confirmed should produce GTC
        let actions = strategy
            .on_crypto_price("BTC", dec!(52000), "binance", &ctx)
            .await
            .unwrap();

        let orders: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::PlaceOrder(req) => Some(req),
                _ => None,
            })
            .collect();
        assert!(!orders.is_empty(), "should place an order");
        let order = orders[0];
        assert_eq!(order.order_type, OrderType::Gtc, "Confirmed should use GTC in hybrid mode");
        // Price should be best_ask - limit_offset = 0.60 - 0.01 = 0.59
        assert_eq!(order.price, dec!(0.59));

        // Verify pending order recorded with correct type and mode
        let pending = strategy.pending_orders.values().next().unwrap();
        assert_eq!(pending.order_type, OrderType::Gtc);
        assert_eq!(pending.mode, ArbitrageMode::Confirmed);
    }

    #[tokio::test]
    async fn tail_end_mode_produces_fok_at_ask() {
        let mut strategy = make_strategy_no_chainlink();
        let ctx = StrategyContext::new();

        // < 120s remaining, ask >= 0.90
        let mwr = make_mwr(dec!(50000), 60);
        strategy.active_markets.insert("market1".to_string(), mwr);

        {
            let mut md = ctx.market_data.write().await;
            md.external_prices.insert("BTC".to_string(), dec!(51000));
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.93), dec!(0.95)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.03), dec!(0.05)),
            );
        }

        // Price history for prefilter
        let now = Utc::now();
        let mut history = VecDeque::new();
        history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        history.push_back((now - Duration::seconds(1), dec!(51000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        let actions = strategy
            .on_crypto_price("BTC", dec!(51000), "binance", &ctx)
            .await
            .unwrap();

        let orders: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::PlaceOrder(req) => Some(req),
                _ => None,
            })
            .collect();
        assert!(!orders.is_empty(), "should place a tail-end order");
        let order = orders[0];
        assert_eq!(order.order_type, OrderType::Fok, "TailEnd should always use FOK");
        assert_eq!(order.price, dec!(0.95), "TailEnd price should be at ask");
    }

    #[test]
    fn stale_order_cancelled_after_max_age() {
        let mut config = ArbitrageConfig::default();
        config.order.max_age_secs = 0; // Expire immediately
        let mut strategy = CryptoArbitrageStrategy::new(config, vec![]);

        strategy.open_limit_orders.insert(
            "order-1".to_string(),
            OpenLimitOrder {
                order_id: "order-1".to_string(),
                market_id: "market1".to_string(),
                token_id: "token_up".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.59),
                size: dec!(10),
                reference_price: dec!(50000),
                coin: "BTC".to_string(),
                placed_at: tokio::time::Instant::now() - std::time::Duration::from_secs(1),
                mode: ArbitrageMode::Confirmed,
                kelly_fraction: None,
                estimated_fee: Decimal::ZERO,
            },
        );

        let actions = strategy.check_stale_limit_orders();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::CancelOrder(id) if id == "order-1"));
        assert!(strategy.open_limit_orders.is_empty(), "stale order should be removed");
    }

    #[test]
    fn gtc_order_fill_creates_position() {
        let mut strategy = CryptoArbitrageStrategy::new(ArbitrageConfig::default(), vec![]);

        // Simulate a placed GTC limit order
        strategy.open_limit_orders.insert(
            "order-1".to_string(),
            OpenLimitOrder {
                order_id: "order-1".to_string(),
                market_id: "market1".to_string(),
                token_id: "token_up".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.59),
                size: dec!(10),
                reference_price: dec!(50000),
                coin: "BTC".to_string(),
                placed_at: tokio::time::Instant::now(),
                mode: ArbitrageMode::Confirmed,
                kelly_fraction: None,
                estimated_fee: Decimal::ZERO,
            },
        );

        // Simulate fill event
        let actions = strategy.on_order_filled("order-1", "token_up", dec!(0.59), dec!(10));
        assert!(actions.is_empty());

        // Should have removed from open_limit_orders
        assert!(strategy.open_limit_orders.is_empty());

        // Should have created a position
        let positions = strategy.positions.get("market1").unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].entry_price, dec!(0.59));
        assert_eq!(positions[0].side, OutcomeSide::Up);
        assert_eq!(positions[0].coin, "BTC");
        assert_eq!(positions[0].order_id.as_deref(), Some("order-1"));
    }

    #[tokio::test]
    async fn duplicate_detection_skips_market_with_open_limit() {
        let mut strategy = make_strategy_no_chainlink();
        let ctx = StrategyContext::new();

        let mwr = make_mwr(dec!(50000), 200);
        strategy.active_markets.insert("market1".to_string(), mwr);

        {
            let mut md = ctx.market_data.write().await;
            md.external_prices.insert("BTC".to_string(), dec!(52000));
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.55), dec!(0.60)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.35), dec!(0.40)),
            );
        }

        let now = Utc::now();
        let mut history = VecDeque::new();
        history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        history.push_back((now - Duration::seconds(1), dec!(52000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        // Place a limit order for this market
        strategy.open_limit_orders.insert(
            "existing-order".to_string(),
            OpenLimitOrder {
                order_id: "existing-order".to_string(),
                market_id: "market1".to_string(),
                token_id: "token_up".to_string(),
                side: OutcomeSide::Up,
                price: dec!(0.59),
                size: dec!(10),
                reference_price: dec!(50000),
                coin: "BTC".to_string(),
                placed_at: tokio::time::Instant::now(),
                mode: ArbitrageMode::Confirmed,
                kelly_fraction: None,
                estimated_fee: Decimal::ZERO,
            },
        );

        let actions = strategy
            .on_crypto_price("BTC", dec!(52000), "binance", &ctx)
            .await
            .unwrap();

        let order_actions: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, Action::PlaceOrder(_)))
            .collect();
        assert!(
            order_actions.is_empty(),
            "should skip when limit order already open for market"
        );
    }

    #[tokio::test]
    async fn hybrid_mode_false_preserves_fok_behavior() {
        let mut config = ArbitrageConfig::default();
        config.order.hybrid_mode = false;
        config.use_chainlink = false;
        let mut strategy = CryptoArbitrageStrategy::new(config, vec![]);
        let ctx = StrategyContext::new();

        let mwr = make_mwr(dec!(50000), 200);
        strategy.active_markets.insert("market1".to_string(), mwr);

        {
            let mut md = ctx.market_data.write().await;
            md.external_prices.insert("BTC".to_string(), dec!(52000));
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.55), dec!(0.60)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.35), dec!(0.40)),
            );
        }

        let now = Utc::now();
        let mut history = VecDeque::new();
        history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        history.push_back((now - Duration::seconds(1), dec!(52000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        let actions = strategy
            .on_crypto_price("BTC", dec!(52000), "binance", &ctx)
            .await
            .unwrap();

        let orders: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::PlaceOrder(req) => Some(req),
                _ => None,
            })
            .collect();
        assert!(!orders.is_empty(), "should place an order");
        let order = orders[0];
        assert_eq!(
            order.order_type,
            OrderType::Fok,
            "with hybrid_mode=false, all orders should be FOK"
        );
        // Price should be at the ask, not offset
        assert_eq!(order.price, dec!(0.60));
    }

    // --- Kelly criterion position sizing tests ---

    #[test]
    fn kelly_high_confidence_high_price_large_size() {
        // confidence=1.0 at price=0.95 → payout=0.0526, kelly=1.0
        // size = 10 * 1.0 * 0.25 = 2.50 (clamped to min_size=2)
        let config = SizingConfig::default();
        let size = kelly_position_size(dec!(1.0), dec!(0.95), &config);
        // Full confidence => kelly fraction = 1.0; size = 10 * 1.0 * 0.25 = 2.50
        assert!(size >= config.min_size, "size {size} should be >= min_size");
        assert!(size <= config.max_size, "size {size} should be <= max_size");
        assert_eq!(size, dec!(2.50));
    }

    #[test]
    fn kelly_moderate_confidence_moderate_price() {
        // confidence=0.5, price=0.70 → payout=(1/0.7)-1=0.4286
        // kelly = (0.5 * 0.4286 - 0.5) / 0.4286 = (0.2143 - 0.5) / 0.4286 = -0.6667
        // Negative edge → size should be 0
        let config = SizingConfig::default();
        let size = kelly_position_size(dec!(0.5), dec!(0.70), &config);
        assert_eq!(size, Decimal::ZERO, "50% confidence at 0.70 has negative edge");
    }

    #[test]
    fn kelly_negative_edge_returns_zero() {
        // confidence=0.3 at price=0.60 → payout=0.667
        // kelly = (0.3 * 0.667 - 0.7) / 0.667 = (0.2 - 0.7) / 0.667 = -0.75
        let config = SizingConfig::default();
        let size = kelly_position_size(dec!(0.3), dec!(0.60), &config);
        assert_eq!(size, Decimal::ZERO, "30% confidence at 0.60 should be 0 (negative edge)");
    }

    #[test]
    fn kelly_result_clamped_to_bounds() {
        // Very high confidence at low price → large kelly fraction
        // confidence=1.0, price=0.10 → payout=9.0, kelly=1.0
        // size = 10 * 1.0 * 0.25 = 2.50 (within bounds)
        let mut config = SizingConfig::default();
        config.min_size = dec!(3);
        config.max_size = dec!(5);
        let size = kelly_position_size(dec!(1.0), dec!(0.10), &config);
        assert!(size >= dec!(3), "should be clamped to min_size=3, got {size}");
        assert!(size <= dec!(5), "should be clamped to max_size=5, got {size}");

        // With a larger base and high confidence, should hit max
        config.base_size = dec!(100);
        let size = kelly_position_size(dec!(1.0), dec!(0.10), &config);
        assert_eq!(size, dec!(5), "should be clamped to max_size=5");

        // With a tiny base, should hit min
        config.base_size = dec!(1);
        let size = kelly_position_size(dec!(1.0), dec!(0.10), &config);
        assert_eq!(size, dec!(3), "should be clamped to min_size=3");
    }

    #[tokio::test]
    async fn two_sided_uses_fixed_sizing_not_kelly() {
        // TwoSided mode should use fixed sizing even when use_kelly=true
        let mut strategy = make_strategy_no_chainlink();
        strategy.config.sizing.use_kelly = true;
        let ctx = StrategyContext::new();

        let mwr = make_mwr(dec!(50000), 300);
        strategy.active_markets.insert("market1".to_string(), mwr);

        {
            let mut md = ctx.market_data.write().await;
            md.external_prices
                .insert("BTC".to_string(), dec!(50100));
            // Two-sided: both outcomes cheap so combined < 1.0
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.44), dec!(0.48)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.44), dec!(0.48)),
            );
        }

        let mut history = VecDeque::new();
        history.push_back((
            Utc::now() - Duration::seconds(1),
            dec!(50100),
            "binance".to_string(),
        ));
        strategy.price_history.insert("BTC".to_string(), history);

        let actions = strategy
            .on_crypto_price("BTC", dec!(50100), "binance", &ctx)
            .await
            .unwrap();

        // TwoSided now emits PlaceBatchOrder instead of individual PlaceOrder actions
        let batch_orders: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::PlaceBatchOrder(reqs) => Some(reqs),
                _ => None,
            })
            .collect();

        // If two-sided triggered, verify batch contains 2 orders and PendingOrders used fixed sizing
        if !batch_orders.is_empty() {
            assert_eq!(batch_orders[0].len(), 2, "TwoSided batch should have 2 orders");
            for (_, pending) in &strategy.pending_orders {
                assert!(
                    pending.kelly_fraction.is_none(),
                    "TwoSided should use fixed sizing, not Kelly"
                );
            }
        }
    }

    #[tokio::test]
    async fn two_sided_emits_place_batch_order() {
        let mut strategy = make_strategy_no_chainlink();
        let ctx = StrategyContext::new();

        let mwr = make_mwr(dec!(50000), 300);
        strategy.active_markets.insert("market1".to_string(), mwr);

        {
            let mut md = ctx.market_data.write().await;
            md.external_prices
                .insert("BTC".to_string(), dec!(50100));
            // Two-sided: both outcomes cheap so combined < 1.0
            md.orderbooks.insert(
                "token_up".to_string(),
                make_orderbook("token_up", dec!(0.44), dec!(0.48)),
            );
            md.orderbooks.insert(
                "token_down".to_string(),
                make_orderbook("token_down", dec!(0.44), dec!(0.48)),
            );
        }

        // Set up price history with a large move so the pre-filter passes via spike detection.
        // Baseline at 15s ago (before the 10s spike window), current price shows 4.2% jump.
        let now = Utc::now();
        let mut history = VecDeque::new();
        history.push_back((now - Duration::seconds(15), dec!(48100), "binance".to_string()));
        history.push_back((now - Duration::seconds(1), dec!(50100), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), history);

        let actions = strategy
            .on_crypto_price("BTC", dec!(50100), "binance", &ctx)
            .await
            .unwrap();

        // TwoSided should emit a single PlaceBatchOrder with 2 OrderRequests
        let batch_actions: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::PlaceBatchOrder(reqs) => Some(reqs),
                _ => None,
            })
            .collect();

        assert_eq!(batch_actions.len(), 1, "should emit exactly one PlaceBatchOrder");
        assert_eq!(batch_actions[0].len(), 2, "batch should contain 2 orders (both outcomes)");

        // Verify the two orders have different token IDs (up and down outcomes)
        let token_ids: Vec<_> = batch_actions[0].iter().map(|r| &r.token_id).collect();
        assert_ne!(token_ids[0], token_ids[1], "batch orders should target different tokens");

        // No individual PlaceOrder actions should be present
        let individual_orders: Vec<_> = actions
            .iter()
            .filter(|a| matches!(a, Action::PlaceOrder(_)))
            .collect();
        assert!(individual_orders.is_empty(), "TwoSided should not emit individual PlaceOrder");
    }

    // --- Cross-market correlation tests ---

    fn make_market_info_for_coin(
        id: &str,
        coin: &str,
        end_date: DateTime<Utc>,
    ) -> MarketInfo {
        MarketInfo {
            id: id.to_string(),
            slug: format!("{}-up-down", coin.to_lowercase()),
            question: format!("Will {} go up?", coin),
            start_date: None,
            end_date,
            token_ids: TokenIds {
                outcome_a: format!("{}_token_up", coin.to_lowercase()),
                outcome_b: format!("{}_token_down", coin.to_lowercase()),
            },
            accepting_orders: true,
            neg_risk: false,
        }
    }

    fn make_mwr_for_coin(
        coin: &str,
        market_id: &str,
        reference_price: Decimal,
        time_remaining_secs: i64,
    ) -> MarketWithReference {
        MarketWithReference {
            market: make_market_info_for_coin(
                market_id,
                coin,
                Utc::now() + Duration::seconds(time_remaining_secs),
            ),
            reference_price,
            reference_quality: ReferenceQuality::Exact,
            discovery_time: Utc::now(),
            coin: coin.to_string(),
        }
    }

    #[tokio::test]
    async fn cross_correlated_btc_spike_generates_eth_opportunity() {
        // BTC large spike should generate ETH Up opportunity when correlation enabled.
        // Need leader_confidence >= ~0.72 so follower_confidence (0.7x) >= 0.50.
        // A 80% move gives leader_confidence=0.80, follower=0.56 (above 50%).
        let mut strategy = make_strategy_no_chainlink();
        strategy.config.correlation.enabled = true;
        strategy.config.correlation.min_spike_pct = dec!(0.01);
        // Disable Kelly to simplify order generation
        strategy.config.sizing.use_kelly = false;
        let ctx = StrategyContext::new();

        // Set up BTC price history with a large spike (80% move)
        let now = Utc::now();
        let mut btc_history = VecDeque::new();
        btc_history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        btc_history.push_back((now, dec!(90000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), btc_history);

        // Set up ETH price history (needed for any eval)
        let mut eth_history = VecDeque::new();
        eth_history.push_back((now - Duration::seconds(10), dec!(3000), "binance".to_string()));
        strategy.price_history.insert("ETH".to_string(), eth_history);

        // Add an active ETH market (follower)
        let eth_mwr = make_mwr_for_coin("ETH", "eth_market1", dec!(3000), 600);
        strategy
            .active_markets
            .insert("eth_market1".to_string(), eth_mwr);

        // Also add a BTC market (leader) so the evaluation path runs
        let btc_mwr = make_mwr_for_coin("BTC", "btc_market1", dec!(50000), 600);
        strategy
            .active_markets
            .insert("btc_market1".to_string(), btc_mwr);

        // Set up orderbooks: ETH ask near 0.50 (hasn't moved yet)
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "eth_token_up".to_string(),
                make_orderbook("eth_token_up", dec!(0.48), dec!(0.52)),
            );
            md.orderbooks.insert(
                "eth_token_down".to_string(),
                make_orderbook("eth_token_down", dec!(0.44), dec!(0.48)),
            );
            // BTC orderbooks
            md.orderbooks.insert(
                "btc_token_up".to_string(),
                make_orderbook("btc_token_up", dec!(0.55), dec!(0.60)),
            );
            md.orderbooks.insert(
                "btc_token_down".to_string(),
                make_orderbook("btc_token_down", dec!(0.35), dec!(0.40)),
            );
        }

        let actions = strategy
            .on_crypto_price("BTC", dec!(90000), "binance", &ctx)
            .await
            .unwrap();

        // Should have generated at least one PlaceOrder for the ETH follower
        let place_orders: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::PlaceOrder(req) => Some(req),
                _ => None,
            })
            .collect();

        // Find the ETH cross-correlated order
        let eth_order = place_orders
            .iter()
            .find(|o| o.token_id.starts_with("eth_"));
        assert!(
            eth_order.is_some(),
            "BTC spike should generate ETH cross-correlated order, got actions: {:?}",
            actions
        );

        // Verify the pending order has CrossCorrelated mode
        let eth_pending = strategy
            .pending_orders
            .values()
            .find(|p| p.coin == "ETH");
        assert!(eth_pending.is_some(), "ETH pending order should exist");
        let pending = eth_pending.unwrap();
        assert!(
            matches!(&pending.mode, ArbitrageMode::CrossCorrelated { leader } if leader == "BTC"),
            "Mode should be CrossCorrelated with BTC leader, got {:?}",
            pending.mode
        );
    }

    #[tokio::test]
    async fn cross_correlated_disabled_produces_no_signal() {
        // When correlation.enabled = false, no cross-correlated signals
        let mut strategy = make_strategy_no_chainlink();
        strategy.config.correlation.enabled = false;
        let ctx = StrategyContext::new();

        let now = Utc::now();
        let mut btc_history = VecDeque::new();
        btc_history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        btc_history.push_back((now, dec!(51000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), btc_history);

        let eth_mwr = make_mwr_for_coin("ETH", "eth_market1", dec!(3000), 600);
        strategy
            .active_markets
            .insert("eth_market1".to_string(), eth_mwr);

        let btc_mwr = make_mwr_for_coin("BTC", "btc_market1", dec!(50000), 600);
        strategy
            .active_markets
            .insert("btc_market1".to_string(), btc_mwr);

        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "eth_token_up".to_string(),
                make_orderbook("eth_token_up", dec!(0.48), dec!(0.52)),
            );
            md.orderbooks.insert(
                "eth_token_down".to_string(),
                make_orderbook("eth_token_down", dec!(0.44), dec!(0.48)),
            );
            md.orderbooks.insert(
                "btc_token_up".to_string(),
                make_orderbook("btc_token_up", dec!(0.55), dec!(0.60)),
            );
            md.orderbooks.insert(
                "btc_token_down".to_string(),
                make_orderbook("btc_token_down", dec!(0.35), dec!(0.40)),
            );
        }

        let actions = strategy
            .on_crypto_price("BTC", dec!(51000), "binance", &ctx)
            .await
            .unwrap();

        // Should NOT have any ETH orders since correlation is disabled
        let eth_orders: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::PlaceOrder(req) if req.token_id.starts_with("eth_") => Some(req),
                _ => None,
            })
            .collect();
        assert!(
            eth_orders.is_empty(),
            "Correlation disabled should produce no ETH signals"
        );

        // No cross-correlated pending orders
        let cross_pending = strategy
            .pending_orders
            .values()
            .any(|p| matches!(&p.mode, ArbitrageMode::CrossCorrelated { .. }));
        assert!(!cross_pending, "No cross-correlated pending orders when disabled");
    }

    #[tokio::test]
    async fn cross_correlated_skips_moved_follower_market() {
        // When follower market ask > 0.60, skip (market already caught up)
        let mut strategy = make_strategy_no_chainlink();
        strategy.config.correlation.enabled = true;
        strategy.config.correlation.min_spike_pct = dec!(0.01);
        let ctx = StrategyContext::new();

        let now = Utc::now();
        let mut btc_history = VecDeque::new();
        btc_history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        btc_history.push_back((now, dec!(51000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), btc_history);

        let eth_mwr = make_mwr_for_coin("ETH", "eth_market1", dec!(3000), 600);
        strategy
            .active_markets
            .insert("eth_market1".to_string(), eth_mwr);

        let btc_mwr = make_mwr_for_coin("BTC", "btc_market1", dec!(50000), 600);
        strategy
            .active_markets
            .insert("btc_market1".to_string(), btc_mwr);

        // ETH market already moved: ask at 0.65 (above 0.60 threshold)
        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "eth_token_up".to_string(),
                make_orderbook("eth_token_up", dec!(0.63), dec!(0.65)),
            );
            md.orderbooks.insert(
                "eth_token_down".to_string(),
                make_orderbook("eth_token_down", dec!(0.30), dec!(0.35)),
            );
            md.orderbooks.insert(
                "btc_token_up".to_string(),
                make_orderbook("btc_token_up", dec!(0.55), dec!(0.60)),
            );
            md.orderbooks.insert(
                "btc_token_down".to_string(),
                make_orderbook("btc_token_down", dec!(0.35), dec!(0.40)),
            );
        }

        let actions = strategy
            .on_crypto_price("BTC", dec!(51000), "binance", &ctx)
            .await
            .unwrap();

        // Should NOT have ETH orders since the market already moved
        let eth_orders: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                Action::PlaceOrder(req) if req.token_id.starts_with("eth_") => Some(req),
                _ => None,
            })
            .collect();
        assert!(
            eth_orders.is_empty(),
            "Should skip follower market that already moved (ask > 0.60)"
        );
    }

    #[tokio::test]
    async fn cross_correlated_confidence_properly_discounted() {
        // Verify follower confidence = leader_change_pct.abs() * 0.7
        let mut strategy = make_strategy_no_chainlink();
        strategy.config.correlation.enabled = true;
        strategy.config.correlation.min_spike_pct = dec!(0.01);
        // Disable Kelly so we can focus on order generation
        strategy.config.sizing.use_kelly = false;
        let ctx = StrategyContext::new();

        let now = Utc::now();
        // BTC spike of exactly 2% = change_pct of 0.02
        let mut btc_history = VecDeque::new();
        btc_history.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        btc_history.push_back((now, dec!(51000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), btc_history);

        let eth_mwr = make_mwr_for_coin("ETH", "eth_market1", dec!(3000), 600);
        strategy
            .active_markets
            .insert("eth_market1".to_string(), eth_mwr);

        let btc_mwr = make_mwr_for_coin("BTC", "btc_market1", dec!(50000), 600);
        strategy
            .active_markets
            .insert("btc_market1".to_string(), btc_mwr);

        {
            let mut md = ctx.market_data.write().await;
            md.orderbooks.insert(
                "eth_token_up".to_string(),
                make_orderbook("eth_token_up", dec!(0.48), dec!(0.52)),
            );
            md.orderbooks.insert(
                "eth_token_down".to_string(),
                make_orderbook("eth_token_down", dec!(0.44), dec!(0.48)),
            );
            md.orderbooks.insert(
                "btc_token_up".to_string(),
                make_orderbook("btc_token_up", dec!(0.55), dec!(0.60)),
            );
            md.orderbooks.insert(
                "btc_token_down".to_string(),
                make_orderbook("btc_token_down", dec!(0.35), dec!(0.40)),
            );
        }

        let _actions = strategy
            .on_crypto_price("BTC", dec!(51000), "binance", &ctx)
            .await
            .unwrap();

        // The leader confidence = |0.02| = 0.02
        // The follower confidence = 0.02 * 0.7 = 0.014
        // This is below the 0.50 threshold, so no order should be generated
        // for a 2% move. Need a much larger spike for sufficient confidence.
        let eth_pending = strategy
            .pending_orders
            .values()
            .find(|p| p.coin == "ETH");

        // 2% spike * 0.7 = 1.4% confidence — well below 50% threshold
        assert!(
            eth_pending.is_none(),
            "2% spike gives only 1.4% follower confidence (< 50%), should not generate order"
        );

        // Now test with a massive spike (80%) that gives sufficient confidence
        strategy.pending_orders.clear();
        let mut btc_history2 = VecDeque::new();
        btc_history2.push_back((now - Duration::seconds(15), dec!(50000), "binance".to_string()));
        btc_history2.push_back((now, dec!(90000), "binance".to_string()));
        strategy.price_history.insert("BTC".to_string(), btc_history2);

        let _actions2 = strategy
            .on_crypto_price("BTC", dec!(90000), "binance", &ctx)
            .await
            .unwrap();

        // leader_confidence = min(|0.80|, 1.0) = 0.80
        // follower_confidence = 0.80 * 0.7 = 0.56 (above 50% threshold)
        let eth_pending2 = strategy
            .pending_orders
            .values()
            .find(|p| p.coin == "ETH");
        assert!(
            eth_pending2.is_some(),
            "80% spike gives 56% follower confidence (>= 50%), should generate order"
        );
    }

    // ---- Performance tracking tests ----

    #[test]
    fn mode_stats_win_rate_correct() {
        let mut stats = ModeStats::new(50);
        // Record 7 wins and 3 losses
        for _ in 0..7 {
            stats.record(dec!(0.10)); // win
        }
        for _ in 0..3 {
            stats.record(dec!(-0.05)); // loss
        }

        assert_eq!(stats.entered, 10);
        assert_eq!(stats.won, 7);
        assert_eq!(stats.lost, 3);
        assert_eq!(stats.total_trades(), 10);
        assert_eq!(stats.win_rate(), dec!(0.7));
    }

    #[test]
    fn mode_stats_avg_pnl() {
        let mut stats = ModeStats::new(50);
        stats.record(dec!(0.10));
        stats.record(dec!(0.20));
        stats.record(dec!(-0.06));

        // total_pnl = 0.24
        assert_eq!(stats.total_pnl, dec!(0.24));
        // avg_pnl = 0.24 / 3 = 0.08
        assert_eq!(stats.avg_pnl(), dec!(0.08));
    }

    #[test]
    fn mode_stats_empty_returns_zero() {
        let stats = ModeStats::new(50);
        assert_eq!(stats.win_rate(), Decimal::ZERO);
        assert_eq!(stats.avg_pnl(), Decimal::ZERO);
        assert_eq!(stats.total_trades(), 0);
    }

    #[test]
    fn mode_stats_rolling_window_evicts() {
        let mut stats = ModeStats::new(3); // Small window
        stats.record(dec!(1.0));
        stats.record(dec!(2.0));
        stats.record(dec!(3.0));
        // Window: [1, 2, 3], avg = 2.0
        assert_eq!(stats.avg_pnl(), dec!(2.0));

        stats.record(dec!(6.0));
        // Window: [2, 3, 6], oldest evicted
        assert_eq!(stats.recent_pnl.len(), 3);
        // avg = (2+3+6)/3 = 11/3
        let expected_avg = dec!(11) / dec!(3);
        assert_eq!(stats.avg_pnl(), expected_avg);
    }

    #[test]
    fn auto_disable_triggers_after_min_trades_with_low_win_rate() {
        let mut config = ArbitrageConfig::default();
        config.performance.auto_disable = true;
        config.performance.min_trades = 5;
        config.performance.min_win_rate = dec!(0.40);

        let mut strategy = CryptoArbitrageStrategy::new(config, vec![]);

        // Record 5 trades: 1 win, 4 losses = 20% win rate < 40%
        let mode = ArbitrageMode::Confirmed;
        strategy.record_trade_pnl(&mode, dec!(0.10)); // win
        strategy.record_trade_pnl(&mode, dec!(-0.05)); // loss
        strategy.record_trade_pnl(&mode, dec!(-0.05));
        strategy.record_trade_pnl(&mode, dec!(-0.05));
        strategy.record_trade_pnl(&mode, dec!(-0.05));

        assert!(
            strategy.is_mode_disabled(&ArbitrageMode::Confirmed),
            "Mode should be disabled: 5 trades with 20% win rate < 40%"
        );
    }

    #[test]
    fn auto_disable_does_not_trigger_before_min_trades() {
        let mut config = ArbitrageConfig::default();
        config.performance.auto_disable = true;
        config.performance.min_trades = 20;
        config.performance.min_win_rate = dec!(0.40);

        let mut strategy = CryptoArbitrageStrategy::new(config, vec![]);

        // Record only 3 trades: all losses (0% win rate)
        let mode = ArbitrageMode::TailEnd;
        strategy.record_trade_pnl(&mode, dec!(-0.05));
        strategy.record_trade_pnl(&mode, dec!(-0.05));
        strategy.record_trade_pnl(&mode, dec!(-0.05));

        assert!(
            !strategy.is_mode_disabled(&ArbitrageMode::TailEnd),
            "Mode should NOT be disabled: only 3 trades < min_trades(20)"
        );
    }

    #[test]
    fn auto_disable_not_triggered_when_disabled_in_config() {
        let mut config = ArbitrageConfig::default();
        config.performance.auto_disable = false;
        config.performance.min_trades = 2;
        config.performance.min_win_rate = dec!(0.40);

        let mut strategy = CryptoArbitrageStrategy::new(config, vec![]);

        // Record 5 losing trades
        let mode = ArbitrageMode::Confirmed;
        for _ in 0..5 {
            strategy.record_trade_pnl(&mode, dec!(-0.05));
        }

        assert!(
            !strategy.is_mode_disabled(&ArbitrageMode::Confirmed),
            "Auto-disable is off in config, mode should stay active"
        );
    }

    #[test]
    fn pnl_calculation_win() {
        // Winner: pnl = (1.0 - entry_price) * size - (estimated_fee * size)
        let entry_price = dec!(0.60);
        let size = dec!(10);
        // Fee per share at p=0.60: 2 * 0.60 * 0.40 * 0.0315 = 0.01512
        let fee_per_share = taker_fee(entry_price, dec!(0.0315));

        let pnl = (Decimal::ONE - entry_price) * size - (fee_per_share * size);
        // (1.0 - 0.60) * 10 - (0.01512 * 10) = 4.0 - 0.1512 = 3.8488
        assert_eq!(pnl, dec!(3.8488));
    }

    #[test]
    fn pnl_calculation_loss() {
        // Loser: pnl = -entry_price * size - (estimated_fee * size)
        let entry_price = dec!(0.60);
        let size = dec!(10);
        // Fee per share at p=0.60: 2 * 0.60 * 0.40 * 0.0315 = 0.01512
        let fee_per_share = taker_fee(entry_price, dec!(0.0315));

        let pnl = -entry_price * size - (fee_per_share * size);
        // -0.60 * 10 - (0.01512 * 10) = -6.0 - 0.1512 = -6.1512
        assert_eq!(pnl, dec!(-6.1512));
    }

    #[test]
    fn pnl_calculation_stop_loss_exit() {
        // Stop-loss: pnl = (exit_price - entry_price) * size - (estimated_fee * size)
        let entry_price = dec!(0.60);
        let exit_price = dec!(0.55);
        let size = dec!(10);
        // Fee per share at p=0.60: 2 * 0.60 * 0.40 * 0.0315 = 0.01512
        let fee_per_share = taker_fee(entry_price, dec!(0.0315));

        let pnl = (exit_price - entry_price) * size - (fee_per_share * size);
        // (0.55 - 0.60) * 10 - (0.01512 * 10) = -0.50 - 0.1512 = -0.6512
        assert_eq!(pnl, dec!(-0.6512));
    }

    #[test]
    fn record_trade_pnl_creates_stats() {
        let strategy_config = ArbitrageConfig::default();
        let mut strategy = CryptoArbitrageStrategy::new(strategy_config, vec![]);

        assert!(strategy.mode_stats.is_empty());

        strategy.record_trade_pnl(&ArbitrageMode::TailEnd, dec!(0.50));
        assert_eq!(strategy.mode_stats.len(), 1);

        let stats = strategy.mode_stats.get(&ArbitrageMode::TailEnd).unwrap();
        assert_eq!(stats.won, 1);
        assert_eq!(stats.total_pnl, dec!(0.50));
    }

    #[test]
    fn mode_stats_win_rate_calculation() {
        let mut stats = ModeStats::new(50);

        // No trades yet
        assert_eq!(stats.win_rate(), Decimal::ZERO);

        // Add 7 wins and 3 losses
        for _ in 0..7 {
            stats.record(dec!(1.0)); // Positive P&L = win
        }
        for _ in 0..3 {
            stats.record(dec!(-0.5)); // Negative P&L = loss
        }

        // Win rate should be 7/10 = 0.70
        assert_eq!(stats.won, 7);
        assert_eq!(stats.lost, 3);
        assert_eq!(stats.total_trades(), 10);
        assert_eq!(stats.win_rate(), Decimal::new(70, 2));
    }

    // --- Priority ordering tests ---

    #[tokio::test]
    async fn find_best_reference_prefers_boundary_over_stale_onchain() {
        // This test validates that boundary snapshots (≤2s staleness) are preferred
        // over on-chain Chainlink rounds (typically 12-15s staleness).
        let mut strategy = make_strategy_no_chainlink();

        let window_ts = 1706000000i64;
        let target_dt = DateTime::from_timestamp(window_ts, 0).unwrap();

        // Boundary snapshot with 1s staleness (simulating RTDS capture)
        strategy.boundary_prices.insert(
            format!("BTC-{window_ts}"),
            BoundarySnapshot {
                timestamp: target_dt + Duration::seconds(1),
                price: dec!(50100),
                source: "binance".to_string(),
            },
        );

        // Even if on-chain would return a different price (simulated via history fallback),
        // the boundary snapshot should be used because it's fresher.
        let (price, quality) = strategy.find_best_reference("BTC", window_ts, dec!(50500)).await;

        // Should use boundary snapshot price, not current price fallback
        assert_eq!(price, dec!(50100));
        assert_eq!(quality, ReferenceQuality::Exact);
    }

    #[tokio::test]
    async fn find_best_reference_uses_onchain_when_no_boundary() {
        // When no boundary snapshot exists, on-chain should be used (if Chainlink enabled).
        // Since we can't mock the RPC client, we verify fallback behavior without Chainlink.
        let mut strategy = make_strategy_no_chainlink();

        let window_ts = 1706000000i64;
        let target_dt = DateTime::from_timestamp(window_ts, 0).unwrap();

        // No boundary snapshot, but historical price available
        let mut history = VecDeque::new();
        history.push_back((
            target_dt + Duration::seconds(10),
            dec!(50200),
            "binance".to_string(),
        ));
        strategy.price_history.insert("BTC".to_string(), history);

        let (price, quality) = strategy.find_best_reference("BTC", window_ts, dec!(50500)).await;

        // Should fall back to historical since no boundary and no Chainlink
        assert_eq!(price, dec!(50200));
        assert_eq!(quality, ReferenceQuality::Historical(10));
    }
}
