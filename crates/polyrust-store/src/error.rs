use thiserror::Error;

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("Connection error: {0}")]
    Connection(String),

    #[error("Migration error: {0}")]
    Migration(String),

    #[error("Query error: {0}")]
    Query(String),
}

pub type StoreResult<T> = std::result::Result<T, StoreError>;
