//! Crypto arbitrage strategy for 15-minute Up/Down prediction markets.
//!
//! High-confidence trades near expiration (< 120s, market >= 90%).
//! State is managed through `CryptoArbRuntime`.

mod config;
mod dashboard;
pub mod domain;
mod runtime;
pub(crate) mod services;
mod strategy;

pub use runtime::CryptoArbRuntime;
pub use config::{
    ArbitrageConfig, FeeConfig, OrderConfig, PerformanceConfig, ReferenceQualityLevel,
    SizingConfig, SpikeConfig, StopLossConfig, TailEndConfig,
};
pub use dashboard::CryptoArbDashboard;
pub use strategy::tailend::TailEndStrategy;
pub use domain::{
    ArbitrageOpportunity, ArbitragePosition, BoundarySnapshot, CompositePriceResult,
    CompositePriceSnapshot, ExitOrderMeta, MarketWithReference, ModeStats, OpenLimitOrder,
    OrderTelemetry, PendingOrder, PositionLifecycle, PositionLifecycleState, ReferenceQuality,
    SpikeEvent, StopLossRejectionKind, StopLossTriggerKind, TriggerEvalContext, compute_exit_clip,
};

// Re-export fee helpers and utilities for use in strategies
pub use services::{escape_html, kelly_position_size, net_profit_margin, taker_fee};

#[cfg(test)]
mod tests;
