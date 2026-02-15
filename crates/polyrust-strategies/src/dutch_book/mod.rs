pub(crate) mod analyzer;
mod config;
mod dashboard;
pub(crate) mod scanner;
mod strategy;
mod types;

pub use analyzer::ArbitrageAnalyzer;
pub use config::DutchBookConfig;
pub use dashboard::DutchBookDashboard;
pub use scanner::GammaScanner;
pub use strategy::DutchBookStrategy;
pub use types::{
    ArbitrageOpportunity, DutchBookState, ExecutionState, MarketEntry, PairedOrder, PairedPosition,
};

#[cfg(test)]
mod tests;
