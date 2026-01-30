pub mod clob_fetcher;
pub mod gamma_fetcher;
pub mod store;

pub use clob_fetcher::ClobFetcher;
pub use gamma_fetcher::GammaFetcher;
pub use store::{
    DataFetchLog, HistoricalDataStore, HistoricalMarket, HistoricalPrice, HistoricalTrade,
};

/// Data fetching orchestrator
pub struct DataFetcher {
    // Placeholder for now
}

impl DataFetcher {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for DataFetcher {
    fn default() -> Self {
        Self::new()
    }
}
