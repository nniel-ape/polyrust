//! Domain types for the crypto arbitrage strategies.

use std::collections::VecDeque;
use std::fmt;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::crypto_arb::config::ReferenceQualityLevel;
use polyrust_core::prelude::*;

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

    /// Convert to quality level for threshold comparison.
    pub fn as_level(&self) -> ReferenceQualityLevel {
        match self {
            ReferenceQuality::Exact => ReferenceQualityLevel::Exact,
            ReferenceQuality::OnChain(_) => ReferenceQualityLevel::OnChain,
            ReferenceQuality::Historical(_) => ReferenceQualityLevel::Historical,
            ReferenceQuality::Current => ReferenceQualityLevel::Current,
        }
    }

    /// Check if this quality meets the minimum required level.
    pub fn meets_threshold(&self, min_level: ReferenceQualityLevel) -> bool {
        self.as_level() >= min_level
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
    /// Window start timestamp (unix seconds) used for reference lookup.
    /// Needed to correlate with boundary snapshots for retroactive quality upgrades.
    pub window_ts: i64,
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


/// A detected arbitrage opportunity ready for execution.
///
/// Contains all information needed to place an order: market, outcome, price,
/// confidence, and profitability after fees. The `net_margin` field accounts
/// for Polymarket's dynamic taker fees (0% for maker/GTC orders).
#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
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
    /// Estimated fee **per share** at entry (for P&L calculation).
    /// Total fee for position = `estimated_fee * size`.
    pub estimated_fee: Decimal,
    /// Market price (best bid) at entry time (for post-entry confirmation).
    /// Used to detect false signals when price drops shortly after entry.
    pub entry_market_price: Decimal,
    /// Market tick size for order rounding.
    pub tick_size: Decimal,
    /// Fee rate in basis points for this market.
    pub fee_rate_bps: u32,
    /// Order type used for entry (GTC = maker/0% fee, FOK = taker fee).
    /// Used for correct P&L calculation instead of relying on `estimated_fee`.
    pub entry_order_type: OrderType,
    /// Actual fee per share at entry: 0 for GTC (maker), `taker_fee(price, rate)` for FOK.
    pub entry_fee_per_share: Decimal,
    /// Accumulated realized P&L from partial exits (starts at 0).
    pub realized_pnl: Decimal,
}

impl ArbitragePosition {
    /// Create a position from a filled limit order.
    ///
    /// Used by both `on_order_placed` (FOK fallback) and `on_order_filled` (GTC fill)
    /// to avoid duplicating the field mapping.
    pub fn from_limit_order(
        lo: &OpenLimitOrder,
        fill_price: Decimal,
        fill_size: Decimal,
        order_id: Option<String>,
        entry_time: DateTime<Utc>,
    ) -> Self {
        Self {
            market_id: lo.market_id.clone(),
            token_id: lo.token_id.clone(),
            side: lo.side,
            entry_price: fill_price,
            size: fill_size,
            reference_price: lo.reference_price,
            coin: lo.coin.clone(),
            order_id,
            entry_time,
            kelly_fraction: lo.kelly_fraction,
            peak_bid: fill_price,
            estimated_fee: lo.estimated_fee,
            entry_market_price: fill_price,
            tick_size: lo.tick_size,
            fee_rate_bps: lo.fee_rate_bps,
            entry_order_type: OrderType::Gtc,
            entry_fee_per_share: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
        }
    }
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
    pub fn new(window_size: usize) -> Self {
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
    pub fn record(&mut self, pnl: Decimal) {
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

/// Order lifecycle telemetry for queue outcome tracking.
///
/// Records fill times, post-only rejections, and cancel rates
/// to inform adaptive sizing and execution quality monitoring.
#[derive(Debug, Default)]
pub struct OrderTelemetry {
    /// Number of times a postOnly order was rejected (would have matched).
    pub post_only_rejects: u64,
    /// (seconds_to_expiry, fill_time_secs) for filled orders.
    pub fill_times: Vec<(i64, f64)>,
    /// Per-coin cancel count (order cancelled before fill).
    pub cancel_before_fill: std::collections::HashMap<String, u64>,
    /// Total orders submitted.
    pub total_orders: u64,
    /// Total fills received.
    pub total_fills: u64,
    /// Total cancels received.
    pub total_cancels: u64,
}

impl OrderTelemetry {
    /// Fill rate as a fraction. Returns 0 if no orders.
    pub fn fill_rate(&self) -> f64 {
        if self.total_orders == 0 {
            0.0
        } else {
            self.total_fills as f64 / self.total_orders as f64
        }
    }
}

/// A pending order awaiting confirmation from the execution backend.
#[derive(Debug, Clone)]
pub struct PendingOrder {
    pub market_id: MarketId,
    pub token_id: TokenId,
    pub side: OutcomeSide,
    pub price: Decimal,
    pub size: Decimal,
    pub reference_price: Decimal,
    pub coin: String,
    pub order_type: OrderType,
    pub kelly_fraction: Option<Decimal>,
    /// Estimated fee **per share** at entry. Total fee = `estimated_fee * size`.
    pub estimated_fee: Decimal,
    /// Market tick size for order rounding.
    pub tick_size: Decimal,
    /// Fee rate in basis points for this market.
    pub fee_rate_bps: u32,
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
    /// Timestamp when order was placed (for staleness check).
    /// Uses `DateTime<Utc>` instead of `tokio::time::Instant` so that
    /// backtests with simulated time can correctly detect stale orders.
    pub placed_at: DateTime<Utc>,
    /// Kelly fraction used for sizing (None if fixed).
    pub kelly_fraction: Option<Decimal>,
    /// Estimated fee **per share** at entry (0 for GTC maker orders).
    /// Total fee = `estimated_fee * size`.
    pub estimated_fee: Decimal,
    /// Market tick size for order rounding.
    pub tick_size: Decimal,
    /// Fee rate in basis points for this market.
    pub fee_rate_bps: u32,
    /// Whether a cancel request is in flight for this order.
    /// Prevents duplicate cancel actions on subsequent event cycles.
    pub cancel_pending: bool,
    /// Number of consecutive reconciliation snapshots where this order was missing
    /// from the CLOB. A synthetic fill is only created after `>= 2` consecutive
    /// misses, protecting against transient API snapshot gaps.
    pub reconcile_miss_count: u8,
}

// ---------------------------------------------------------------------------
// Position Lifecycle State Machine Types
// ---------------------------------------------------------------------------

/// Classification of stop-loss trigger that caused an exit evaluation.
///
/// Priority order (highest first): HardCrash > DualTrigger > TrailingStop > PostEntryExit.
/// Only the highest-priority trigger that fires is returned.
#[derive(Debug, Clone, PartialEq)]
pub enum StopLossTriggerKind {
    /// Level 1: Catastrophic bid drop or external price reversal.
    /// Requires only 1 fresh source + fresh orderbook. Bypasses hysteresis.
    HardCrash {
        /// Absolute bid drop from entry (e.g. 0.08).
        bid_drop: Decimal,
        /// External price reversal percentage (e.g. 0.006).
        reversal_pct: Decimal,
    },
    /// Level 2: Both crypto reversal AND market drop confirmed for N consecutive ticks.
    DualTrigger {
        /// Number of consecutive ticks both conditions held.
        consecutive_ticks: usize,
    },
    /// Level 3: Peak bid minus current bid exceeds trailing distance (with time decay).
    TrailingStop {
        /// Peak bid observed since entry.
        peak_bid: Decimal,
        /// Current bid that triggered the stop.
        current_bid: Decimal,
        /// Effective trailing distance used (after headroom cap + time decay).
        effective_distance: Decimal,
    },
    /// Level 4: Adverse move detected during post-entry sell delay window.
    /// Transition to DeferredExit — actual sell happens when delay expires.
    PostEntryExit {
        /// Bid drop from entry that triggered the deferred exit.
        bid_drop: Decimal,
    },
}

impl fmt::Display for StopLossTriggerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HardCrash { bid_drop, reversal_pct } => {
                write!(f, "HardCrash(bid_drop={bid_drop}, reversal={reversal_pct})")
            }
            Self::DualTrigger { consecutive_ticks } => {
                write!(f, "DualTrigger(ticks={consecutive_ticks})")
            }
            Self::TrailingStop { peak_bid, current_bid, effective_distance } => {
                write!(
                    f,
                    "TrailingStop(peak={peak_bid}, current={current_bid}, dist={effective_distance})"
                )
            }
            Self::PostEntryExit { bid_drop } => {
                write!(f, "PostEntryExit(bid_drop={bid_drop})")
            }
        }
    }
}

/// Per-position lifecycle state in the state machine.
///
/// Valid transitions:
/// - Healthy -> DeferredExit (trigger during sell delay)
/// - Healthy -> ExitExecuting (trigger when sellable)
/// - DeferredExit -> ExitExecuting (delay elapsed, trigger persists)
/// - DeferredExit -> Healthy (trigger cleared)
/// - ExitExecuting -> ResidualRisk (partial fill or rejection)
/// - ExitExecuting -> (resolved) (fully filled — position removed)
/// - ResidualRisk -> ExitExecuting (retry)
/// - ResidualRisk -> RecoveryProbe (max retries or risk under budget)
/// - RecoveryProbe -> ExitExecuting (recovery order fails, retry exit)
/// - RecoveryProbe -> Cooldown (recovery neutralized position)
/// - Cooldown -> Healthy (cooldown elapsed)
#[derive(Debug, Clone, PartialEq)]
pub enum PositionLifecycleState {
    /// Position is active and being monitored. No exit trigger has fired.
    Healthy,
    /// A trigger fired during the sell delay window. Exit is deferred until sellable.
    DeferredExit {
        trigger: StopLossTriggerKind,
        armed_at: DateTime<Utc>,
    },
    /// An exit order has been submitted and is in flight.
    ExitExecuting {
        order_id: OrderId,
        order_type: OrderType,
        exit_price: Decimal,
        submitted_at: DateTime<Utc>,
    },
    /// Partial fill or rejection left residual exposure. Retrying exit.
    ResidualRisk {
        remaining_size: Decimal,
        retry_count: u32,
        last_attempt: DateTime<Utc>,
        use_gtc_next: bool,
    },
    /// Attempting recovery: opposite-side set completion or re-entry.
    RecoveryProbe {
        recovery_order_id: OrderId,
        probe_side: OutcomeSide,
        submitted_at: DateTime<Utc>,
    },
    /// Post-recovery cooldown before position can be re-evaluated.
    Cooldown {
        until: DateTime<Utc>,
    },
}

impl fmt::Display for PositionLifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Healthy => write!(f, "Healthy"),
            Self::DeferredExit { trigger, .. } => write!(f, "DeferredExit({trigger})"),
            Self::ExitExecuting { order_type, exit_price, .. } => {
                write!(f, "ExitExecuting({order_type:?}@{exit_price})")
            }
            Self::ResidualRisk { remaining_size, retry_count, .. } => {
                write!(f, "ResidualRisk(remaining={remaining_size}, retries={retry_count})")
            }
            Self::RecoveryProbe { probe_side, .. } => {
                write!(f, "RecoveryProbe({probe_side:?})")
            }
            Self::Cooldown { until } => write!(f, "Cooldown(until={until})"),
        }
    }
}

impl PositionLifecycleState {
    /// State name for logging.
    fn name(&self) -> &'static str {
        match self {
            Self::Healthy => "Healthy",
            Self::DeferredExit { .. } => "DeferredExit",
            Self::ExitExecuting { .. } => "ExitExecuting",
            Self::ResidualRisk { .. } => "ResidualRisk",
            Self::RecoveryProbe { .. } => "RecoveryProbe",
            Self::Cooldown { .. } => "Cooldown",
        }
    }

    /// Check whether transitioning from `self` to `target` is valid.
    ///
    /// Uses enum variant matching for compile-time safety: adding a new variant
    /// forces updating the match arms.
    fn can_transition_to(&self, target: &PositionLifecycleState) -> bool {
        use PositionLifecycleState::*;
        matches!(
            (self, target),
            (Healthy, DeferredExit { .. })
                | (Healthy, ExitExecuting { .. })
                | (DeferredExit { .. }, ExitExecuting { .. })
                | (DeferredExit { .. }, Healthy)
                | (ExitExecuting { .. }, ResidualRisk { .. })
                | (ResidualRisk { .. }, ExitExecuting { .. })
                | (ResidualRisk { .. }, RecoveryProbe { .. })
                | (RecoveryProbe { .. }, ExitExecuting { .. })
                | (RecoveryProbe { .. }, Cooldown { .. })
                | (Cooldown { .. }, Healthy)
        )
    }
}

/// Maximum number of entries in the transition log before oldest entries are dropped.
const TRANSITION_LOG_CAP: usize = 50;

/// Per-position lifecycle tracker.
///
/// Wraps `PositionLifecycleState` with auxiliary tracking fields and an
/// append-only transition log (capped at 50 entries for memory safety).
#[derive(Debug, Clone)]
pub struct PositionLifecycle {
    /// Current state in the lifecycle.
    pub state: PositionLifecycleState,
    /// Counter of consecutive ticks where both dual-trigger conditions held.
    pub dual_trigger_ticks: usize,
    /// True if trailing stop cannot arm due to insufficient headroom (entry near price cap).
    pub trailing_unarmable: bool,
    /// Most recent composite price used for stop-loss evaluation.
    pub last_composite: Option<CompositePriceSnapshot>,
    /// Timestamp of the most recent composite price.
    pub last_composite_at: Option<DateTime<Utc>>,
    /// Order ID of the pending exit order (for routing fills/rejects).
    pub pending_exit_order_id: Option<OrderId>,
    /// Append-only log of state transitions (capped at TRANSITION_LOG_CAP).
    pub transition_log: Vec<(DateTime<Utc>, String)>,
}

/// Snapshot of composite price data for stop-loss decisions.
/// Kept separate from `base::CompositePriceResult` to avoid coupling types.rs to base.rs.
#[derive(Debug, Clone)]
pub struct CompositePriceSnapshot {
    pub price: Decimal,
    pub sources_used: usize,
    pub max_lag_ms: i64,
    pub dispersion_bps: Decimal,
}

impl CompositePriceSnapshot {
    /// Create a snapshot from a `CompositePriceResult`.
    pub fn from_result(r: &super::base::CompositePriceResult) -> Self {
        Self {
            price: r.price,
            sources_used: r.sources_used,
            max_lag_ms: r.max_lag_ms,
            dispersion_bps: r.dispersion_bps,
        }
    }
}

impl Default for PositionLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl PositionLifecycle {
    /// Create a new lifecycle tracker in the Healthy state.
    pub fn new() -> Self {
        Self {
            state: PositionLifecycleState::Healthy,
            dual_trigger_ticks: 0,
            trailing_unarmable: false,
            last_composite: None,
            last_composite_at: None,
            pending_exit_order_id: None,
            transition_log: Vec::new(),
        }
    }

    /// Attempt to transition to a new state.
    ///
    /// Returns `Ok(())` if the transition is valid, `Err` with a descriptive
    /// message if not. On success, appends the transition to the log.
    pub fn transition(
        &mut self,
        new_state: PositionLifecycleState,
        reason: &str,
        now: DateTime<Utc>,
    ) -> std::result::Result<(), String> {
        if !self.state.can_transition_to(&new_state) {
            return Err(format!(
                "Invalid transition: {} -> {} (reason: {})",
                self.state.name(),
                new_state.name(),
                reason
            ));
        }
        let entry = format!("{} -> {}: {}", self.state.name(), new_state.name(), reason);
        self.state = new_state;
        self.transition_log.push((now, entry));
        // Cap the log to prevent unbounded growth
        if self.transition_log.len() > TRANSITION_LOG_CAP {
            let excess = self.transition_log.len() - TRANSITION_LOG_CAP;
            self.transition_log.drain(..excess);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Trigger Evaluation
// ---------------------------------------------------------------------------

/// Input data bundle for `evaluate_triggers`.
///
/// Keeps the function signature clean by grouping position, market, and
/// price data that the trigger hierarchy needs.
#[derive(Debug, Clone)]
pub struct TriggerEvalContext {
    /// Entry price of the position.
    pub entry_price: Decimal,
    /// Peak bid observed since entry.
    pub peak_bid: Decimal,
    /// Which side the position is on (Up/Down).
    pub side: OutcomeSide,
    /// Reference crypto price at window start.
    pub reference_price: Decimal,
    /// Market tick size.
    pub tick_size: Decimal,
    /// When the position was entered.
    pub entry_time: DateTime<Utc>,
    /// Current best bid from the orderbook.
    pub current_bid: Decimal,
    /// Age of the orderbook snapshot in milliseconds.
    pub book_age_ms: i64,
    /// Latest external/composite crypto price (if fresh enough).
    pub external_price: Option<Decimal>,
    /// Age of the external price in milliseconds (None if no price).
    pub external_age_ms: Option<i64>,
    /// Number of sources in the composite (None if single source).
    pub composite_sources: Option<usize>,
    /// Seconds remaining on the market.
    pub time_remaining: i64,
    /// Current time.
    pub now: DateTime<Utc>,
}

impl PositionLifecycle {
    /// Evaluate the 4-level trigger hierarchy for a position.
    ///
    /// Returns the highest-priority trigger that fires, or `None`.
    /// Mutates `self.dual_trigger_ticks` and `self.trailing_unarmable` as side effects.
    ///
    /// Priority (highest first):
    /// 1. Hard Crash — absolute bid drop or external reversal (1 fresh source + fresh book)
    /// 2. Dual Trigger — crypto reversed AND market dropped for N consecutive ticks
    /// 3. Trailing Stop — peak-to-current drop with headroom fix
    /// 4. Post-Entry Deferred — adverse move within sell delay window
    pub fn evaluate_triggers(
        &mut self,
        ctx: &TriggerEvalContext,
        sl_config: &super::config::StopLossConfig,
        tailend_config: &super::config::TailEndConfig,
    ) -> Option<StopLossTriggerKind> {
        let book_fresh = ctx.book_age_ms <= sl_config.sl_max_book_age_ms;
        let external_fresh = ctx
            .external_age_ms
            .is_some_and(|age| age <= sl_config.sl_max_external_age_ms);
        let has_relaxed_fresh_source = external_fresh
            || ctx
                .external_age_ms
                .is_some_and(|age| age <= sl_config.sl_max_external_age_ms * 2);

        // ── Level 1: Hard Crash ──────────────────────────────────────────
        // Requires only 1 fresh external source + fresh book.
        // Bypasses hysteresis — immediate exit.
        if book_fresh && ctx.external_price.is_some() && has_relaxed_fresh_source {
            let bid_drop = ctx.entry_price - ctx.current_bid;
            let hard_bid = bid_drop >= sl_config.hard_drop_abs;

            let hard_reversal = if let Some(ext_price) = ctx.external_price {
                let reversal = compute_reversal(
                    ctx.side,
                    ctx.reference_price,
                    ext_price,
                );
                reversal >= sl_config.hard_reversal_pct
            } else {
                false
            };

            if hard_bid || hard_reversal {
                // Reset dual trigger counter on hard crash (supersedes)
                self.dual_trigger_ticks = 0;
                return Some(StopLossTriggerKind::HardCrash {
                    bid_drop,
                    reversal_pct: if let Some(ext_price) = ctx.external_price {
                        compute_reversal(ctx.side, ctx.reference_price, ext_price)
                    } else {
                        Decimal::ZERO
                    },
                });
            }
        }

        // ── Level 2: Dual Trigger + Hysteresis ───────────────────────────
        // Both crypto_reversed AND market_dropped must hold for
        // `dual_trigger_consecutive_ticks` consecutive evaluations.
        // Requires fresh composite + fresh book.
        if book_fresh && external_fresh {
            let composite_ok = ctx
                .composite_sources
                .is_some_and(|s| s >= sl_config.sl_min_sources);

            let crypto_reversed = if let Some(ext_price) = ctx.external_price {
                compute_reversal(ctx.side, ctx.reference_price, ext_price)
                    >= sl_config.reversal_pct
            } else {
                false
            };

            let market_dropped =
                (ctx.entry_price - ctx.current_bid) >= sl_config.min_drop;

            if composite_ok && crypto_reversed && market_dropped {
                self.dual_trigger_ticks += 1;
                if self.dual_trigger_ticks >= sl_config.dual_trigger_consecutive_ticks {
                    return Some(StopLossTriggerKind::DualTrigger {
                        consecutive_ticks: self.dual_trigger_ticks,
                    });
                }
            } else {
                // Either condition cleared — reset counter
                self.dual_trigger_ticks = 0;
            }
        }

        // ── Level 3: Trailing Stop with headroom fix ─────────────────────
        // Prevents impossible arming at high entry prices.
        if book_fresh && sl_config.trailing_enabled {
            let price_cap = Decimal::ONE - ctx.tick_size;
            let headroom = (price_cap - ctx.entry_price).max(Decimal::ZERO);
            let effective_arm_distance =
                sl_config.trailing_arm_distance.min(headroom);

            if effective_arm_distance < ctx.tick_size {
                self.trailing_unarmable = true;
            } else {
                // Arming check: peak_bid >= entry + effective_arm_distance
                let armed =
                    ctx.peak_bid >= ctx.entry_price + effective_arm_distance;
                if armed {
                    // Compute effective trailing distance with time decay
                    let base_distance = sl_config.trailing_distance;
                    let effective_distance = if sl_config.time_decay {
                        let decay_factor =
                            Decimal::from(ctx.time_remaining) / Decimal::from(900i64);
                        let clamped = decay_factor
                            .max(Decimal::ZERO)
                            .min(Decimal::ONE);
                        (base_distance * clamped)
                            .max(sl_config.trailing_min_distance)
                    } else {
                        base_distance
                    };

                    let drop_from_peak = ctx.peak_bid - ctx.current_bid;
                    if drop_from_peak >= effective_distance {
                        return Some(StopLossTriggerKind::TrailingStop {
                            peak_bid: ctx.peak_bid,
                            current_bid: ctx.current_bid,
                            effective_distance,
                        });
                    }
                }
            }
        }

        // ── Level 4: Post-Entry Deferred ─────────────────────────────────
        // Fires when within sell delay window and adverse move detected.
        let seconds_since_entry = ctx
            .now
            .signed_duration_since(ctx.entry_time)
            .num_seconds();
        let within_sell_delay =
            seconds_since_entry < tailend_config.min_sell_delay_secs;
        // Note: within_post_entry_window is always true when within_sell_delay
        // is true (config validates post_entry_window_secs > min_sell_delay_secs).
        // The DeferredExit path in handle_orderbook_update re-evaluates triggers
        // after sell delay expires, using Levels 1-3.

        if within_sell_delay && book_fresh {
            let bid_drop = ctx.entry_price - ctx.current_bid;
            if bid_drop >= tailend_config.post_entry_exit_drop {
                return Some(StopLossTriggerKind::PostEntryExit { bid_drop });
            }
        }

        None
    }
}

/// Compute the signed reversal percentage of the external price relative to
/// the reference price, from the perspective of the position side.
///
/// For Up/Yes positions: reversal = (reference - current) / reference  (positive when price drops)
/// For Down/No positions: reversal = (current - reference) / reference (positive when price rises)
fn compute_reversal(
    side: OutcomeSide,
    reference_price: Decimal,
    current_price: Decimal,
) -> Decimal {
    if reference_price.is_zero() {
        return Decimal::ZERO;
    }
    match side {
        OutcomeSide::Up | OutcomeSide::Yes => {
            (reference_price - current_price) / reference_price
        }
        OutcomeSide::Down | OutcomeSide::No => {
            (current_price - reference_price) / reference_price
        }
    }
}

/// Metadata for tracking an exit or recovery order back to its position.
///
/// Stored in `exit_orders_by_id` so that fill/reject events from the execution
/// backend can be routed to the correct position lifecycle.
#[derive(Debug, Clone)]
pub struct ExitOrderMeta {
    /// Token ID of the position this exit order belongs to.
    pub token_id: TokenId,
    /// Token ID the order was actually placed for (differs from `token_id`
    /// for recovery orders which buy the opposite side).
    pub order_token_id: TokenId,
    /// Order type (GTC or FOK) for fee model selection.
    pub order_type: OrderType,
    /// Lifecycle state that spawned this order (for context in logs).
    pub source_state: String,
    /// Retry count at time of order submission (for escalation after cancel).
    pub retry_count: u32,
}

/// Compute the exit clip size for a single exit order, capped by available
/// bid depth in the orderbook.
///
/// Returns the number of shares to sell in this clip, or `Decimal::ZERO` if the
/// result would be below `min_size` (dust).
///
/// Formula: `clip = min(remaining, bid_depth * cap_factor)`
/// If `clip < min_size`, returns zero (treat as dust — not worth an order).
pub fn compute_exit_clip(
    remaining: Decimal,
    bid_depth: Decimal,
    cap_factor: Decimal,
    min_size: Decimal,
) -> Decimal {
    let capped = remaining.min(bid_depth * cap_factor);
    if capped < min_size {
        Decimal::ZERO
    } else {
        capped
    }
}
