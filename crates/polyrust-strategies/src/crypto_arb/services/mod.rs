//! Service modules implementing `CryptoArbRuntime` methods.
//!
//! Each module adds an `impl CryptoArbRuntime` block with methods
//! for a specific concern (pricing, market lifecycle, etc.).

mod fee_math;
mod market;
mod observability;
mod order;
mod position;
mod pricing;

// Re-export free functions for external use
pub use fee_math::{kelly_position_size, net_profit_margin, taker_fee};
#[cfg(test)]
pub use fee_math::parse_slug_timestamp;

// Re-export formatting helpers used by dashboard
pub use fee_math::{fmt_market_price, fmt_usd};
pub use crate::shared::escape_html;
