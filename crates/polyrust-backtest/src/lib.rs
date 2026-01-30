pub mod config;
pub mod data;
pub mod engine;
pub mod error;
pub mod report;

pub use config::{BacktestConfig, FeeConfig};
pub use data::{DataFetcher, HistoricalDataStore, HistoricalMarket, HistoricalPrice, HistoricalTrade};
pub use engine::BacktestEngine;
pub use error::{BacktestError, BacktestResult};
pub use report::BacktestReport;
