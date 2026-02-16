pub mod crypto_arb;
pub mod dutch_book;
pub mod shared;

// Re-export main strategy types for convenience
pub use crypto_arb::{
    ArbitrageConfig, CryptoArbDashboard, CryptoArbRuntime, ReferenceQualityLevel, SizingConfig,
    TailEndConfig, TailEndStrategy,
};
pub use dutch_book::{DutchBookConfig, DutchBookDashboard, DutchBookState, DutchBookStrategy};
