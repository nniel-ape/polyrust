pub(crate) mod analyzer;
mod config;
pub(crate) mod scanner;
mod strategy;
mod types;

pub use analyzer::ArbitrageAnalyzer;
pub use config::DutchBookConfig;
pub use scanner::GammaScanner;
pub use strategy::DutchBookStrategy;
pub use types::{
    ArbitrageOpportunity, ExecutionState, MarketEntry, PairedOrder, PairedPosition,
};

#[cfg(test)]
mod tests;
