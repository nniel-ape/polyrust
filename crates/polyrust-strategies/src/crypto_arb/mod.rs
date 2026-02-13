//! Crypto arbitrage strategy for 15-minute Up/Down prediction markets.
//!
//! High-confidence trades near expiration (< 120s, market >= 90%).
//! State is managed through `CryptoArbBase`.

mod base;
mod config;
mod dashboard;
mod tailend;
mod types;

pub use base::CryptoArbBase;
pub use config::{
    ArbitrageConfig, FeeConfig, OrderConfig, PerformanceConfig, ReferenceQualityLevel,
    SizingConfig, SpikeConfig, StopLossConfig, TailEndConfig,
};
pub use dashboard::CryptoArbDashboard;
pub use tailend::TailEndStrategy;
pub use types::{
    ArbitrageOpportunity, ArbitragePosition, BoundarySnapshot, CompositePriceSnapshot,
    ExitOrderMeta, MarketWithReference, ModeStats, OpenLimitOrder, PendingOrder,
    PositionLifecycle, PositionLifecycleState, ReferenceQuality, SpikeEvent,
    StopLossTriggerKind, TriggerEvalContext,
};

// Re-export fee helpers for use in strategies
pub use base::{kelly_position_size, net_profit_margin, taker_fee};

#[cfg(test)]
mod tests;
