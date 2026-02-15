pub mod crypto_arb;
pub mod dutch_book;

// Re-export main strategy types for convenience
pub use crypto_arb::{
    ArbitrageConfig, CryptoArbBase, CryptoArbDashboard, ReferenceQualityLevel, SizingConfig,
    TailEndConfig, TailEndStrategy,
};
pub use dutch_book::{DutchBookConfig, DutchBookDashboard, DutchBookState, DutchBookStrategy};
