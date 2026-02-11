pub mod crypto_arb;

// Re-export main strategy types for convenience
pub use crypto_arb::{
    ArbitrageConfig, CryptoArbBase, CryptoArbDashboard, ReferenceQualityLevel, TailEndConfig,
    TailEndDashboard, TailEndStrategy, TwoSidedConfig, TwoSidedDashboard, TwoSidedStrategy,
};
