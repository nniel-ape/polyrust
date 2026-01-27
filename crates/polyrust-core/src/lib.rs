pub mod actions;
pub mod context;
pub mod error;
pub mod event_bus;
pub mod events;
pub mod execution;
pub mod strategy;
pub mod types;

/// Prelude for convenient imports
pub mod prelude {
    pub use crate::actions::*;
    pub use crate::context::*;
    pub use crate::error::{PolyError, Result};
    pub use crate::event_bus::EventBus;
    pub use crate::events::*;
    pub use crate::execution::ExecutionBackend;
    pub use crate::strategy::Strategy;
    pub use crate::types::*;
    pub use async_trait::async_trait;
    pub use rust_decimal::Decimal;
}
