pub mod error;
pub mod types;

/// Prelude for convenient imports
pub mod prelude {
    pub use crate::error::{PolyError, Result};
    pub use crate::types::*;
}
