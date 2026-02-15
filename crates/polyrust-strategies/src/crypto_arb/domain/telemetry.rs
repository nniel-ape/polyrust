//! Telemetry and observability domain types for the crypto arbitrage strategy.

use std::collections::VecDeque;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

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

    /// Adjust total P&L without counting as a separate trade.
    /// Used for costs that are part of an existing trade lifecycle
    /// (e.g., recovery buy cost) to avoid inflating trade count and win rate.
    pub fn adjust_pnl(&mut self, amount: Decimal) {
        self.total_pnl += amount;
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

/// Classification of stop-loss sell rejection reasons.
///
/// Determines which cooldown schedule to use and whether to fall back to GTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopLossRejectionKind {
    /// "couldn't be fully filled" or "no match" — transient liquidity gap.
    /// Uses fast cooldowns and marks token for GTC fallback.
    Liquidity,
    /// "not enough balance" or "allowance" — token settlement issue.
    /// Uses longer cooldowns.
    BalanceAllowance,
    /// "invalid amounts" / "must be higher than 0" — dust position too small to sell.
    /// Position should be removed immediately; no cooldown retry.
    InvalidSize,
    /// Everything else — treated like balance/allowance (longer cooldowns).
    Transient,
}

impl StopLossRejectionKind {
    /// Classify a rejection reason string.
    pub fn classify(reason: &str) -> Self {
        let lower = reason.to_lowercase();
        if lower.contains("fully filled") || lower.contains("no match") {
            Self::Liquidity
        } else if lower.contains("not enough balance") || lower.contains("allowance") {
            Self::BalanceAllowance
        } else if lower.contains("invalid amounts") || lower.contains("must be higher than 0") {
            Self::InvalidSize
        } else {
            Self::Transient
        }
    }
}
