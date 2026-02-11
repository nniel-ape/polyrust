//! Crypto arbitrage strategies for 15-minute Up/Down prediction markets.
//!
//! This module provides two specialized strategies that share a common base:
//! - **TailEnd**: High-confidence trades near expiration (< 120s, market >= 90%)
//! - **TwoSided**: Risk-free arbitrage when both outcomes mispriced (combined < 98%)
//!
//! All strategies share state through `CryptoArbBase` for efficient resource usage.

mod base;
mod config;
mod dashboard;
mod tailend;
mod twosided;
mod types;

pub use base::CryptoArbBase;
pub use config::{
    ArbitrageConfig, FeeConfig, OrderConfig, PerformanceConfig, ReferenceQualityLevel,
    SizingConfig, SpikeConfig, StopLossConfig, TailEndConfig, TwoSidedConfig,
};
pub use dashboard::{CryptoArbDashboard, TailEndDashboard, TwoSidedDashboard};
pub use tailend::TailEndStrategy;
pub use twosided::TwoSidedStrategy;
pub use types::{
    ArbitrageMode, ArbitrageOpportunity, ArbitragePosition, BoundarySnapshot, MarketWithReference,
    ModeStats, OpenLimitOrder, PendingOrder, ReferenceQuality, SpikeEvent,
};

// Re-export fee helpers for use in strategies
pub use base::{kelly_position_size, net_profit_margin, taker_fee};

#[cfg(test)]
mod tests;
