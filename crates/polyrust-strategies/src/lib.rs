pub mod crypto_arb;

// Re-export main strategy types for convenience
pub use crypto_arb::{
    ArbitrageConfig, ConfirmedConfig, ConfirmedDashboard, ConfirmedStrategy, CorrelationConfig,
    CrossCorrDashboard, CrossCorrStrategy, CryptoArbBase, CryptoArbDashboard, TailEndConfig,
    TailEndDashboard, TailEndStrategy, TwoSidedConfig, TwoSidedDashboard, TwoSidedStrategy,
};
