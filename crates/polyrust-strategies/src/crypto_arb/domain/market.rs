//! Market-related domain types for the crypto arbitrage strategy.

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

/// Result of a composite fair price calculation from multiple data sources.
#[derive(Debug, Clone)]
pub struct CompositePriceResult {
    /// Weighted average price across sources.
    pub price: Decimal,
    /// Number of sources that contributed.
    pub sources_used: usize,
    /// Maximum lag in milliseconds across contributing sources.
    pub max_lag_ms: i64,
    /// Maximum dispersion from composite in basis points.
    pub dispersion_bps: Decimal,
}

/// Snapshot of composite price data for stop-loss decisions.
#[derive(Debug, Clone)]
pub struct CompositePriceSnapshot {
    pub price: Decimal,
    pub sources_used: usize,
    pub max_lag_ms: i64,
    pub dispersion_bps: Decimal,
}

impl CompositePriceSnapshot {
    /// Create a snapshot from a `CompositePriceResult`.
    pub fn from_result(r: &CompositePriceResult) -> Self {
        Self {
            price: r.price,
            sources_used: r.sources_used,
            max_lag_ms: r.max_lag_ms,
            dispersion_bps: r.dispersion_bps,
        }
    }
}
