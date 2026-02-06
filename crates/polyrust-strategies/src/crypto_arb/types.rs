//! Domain types for the crypto arbitrage strategies.

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use polyrust_core::prelude::*;
use crate::crypto_arb::config::ReferenceQualityLevel;

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
    /// Uses GTC orders with aggressive pricing (0% maker fee, no USDC clamping).
    TailEnd,
    /// Two-sided mode: both outcomes priced below combined $0.98.
    /// Guaranteed profit regardless of outcome. Uses batch GTC orders.
    TwoSided,
    /// Confirmed mode: standard directional with dynamic confidence.
    /// Uses GTC maker orders to avoid taker fees.
    Confirmed,
    /// Cross-market correlation: follower coin triggered by leader spike.
    /// Confidence discounted by correlation factor for uncertainty.
    CrossCorrelated {
        /// The leader coin that spiked (e.g. "BTC").
        leader: String,
    },
}

impl ArbitrageMode {
    /// Get the canonical mode variant for performance tracking.
    /// Strips the leader field from CrossCorrelated to unify stats across all leaders.
    pub fn canonical(&self) -> Self {
        match self {
            ArbitrageMode::CrossCorrelated { .. } => ArbitrageMode::CrossCorrelated {
                leader: String::new(),
            },
            other => other.clone(),
        }
    }
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
    /// Market price (best bid) at entry time (for post-entry confirmation).
    /// Used to detect false signals when price drops shortly after entry.
    pub entry_market_price: Decimal,
    /// Market tick size for order rounding.
    pub tick_size: Decimal,
    /// Fee rate in basis points for this market.
    pub fee_rate_bps: u32,
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
    pub mode: ArbitrageMode,
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
    /// Instant when order was placed (for staleness check).
    pub placed_at: tokio::time::Instant,
    /// Trading mode that generated this order.
    pub mode: ArbitrageMode,
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
}
