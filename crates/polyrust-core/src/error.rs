use thiserror::Error;

#[derive(Error, Debug)]
pub enum PolyError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Execution error: {0}")]
    Execution(String),

    #[error("Market data error: {0}")]
    MarketData(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Strategy error: {0}")]
    Strategy(String),

    #[error("Event bus error: {0}")]
    EventBus(String),

    #[error("Polymarket SDK error: {0}")]
    Sdk(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, PolyError>;
