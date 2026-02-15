//! Domain types for the crypto arbitrage strategies.
//!
//! Organized into focused submodules:
//! - `market` — market discovery, reference prices, composite pricing
//! - `position` — opportunities, positions, orders, exit metadata
//! - `lifecycle` — position lifecycle state machine and trigger evaluation
//! - `telemetry` — performance stats, spike events, order telemetry

pub mod lifecycle;
pub mod market;
pub mod position;
pub mod telemetry;

// Re-export all public types for convenient access
pub use lifecycle::{
    PositionLifecycle, PositionLifecycleState, StopLossTriggerKind, TriggerEvalContext,
};
pub use market::{
    BoundarySnapshot, CompositePriceResult, CompositePriceSnapshot, MarketWithReference,
    ReferenceQuality,
};
pub use position::{
    ArbitrageOpportunity, ArbitragePosition, ExitOrderMeta, OpenLimitOrder, PendingOrder,
    compute_exit_clip,
};
pub use telemetry::{ModeStats, OrderTelemetry, SpikeEvent, StopLossRejectionKind};
