use thiserror::Error;

#[derive(Error, Debug)]
pub enum BacktestError {
    #[error("Database error: {0}")]
    Database(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Data fetching error: {0}")]
    DataFetch(String),

    #[error("Engine error: {0}")]
    Engine(String),

    #[error("Strategy error: {0}")]
    Strategy(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Invalid input: {0}")]
    InvalidInput(String),
}

pub type BacktestResult<T> = std::result::Result<T, BacktestError>;
