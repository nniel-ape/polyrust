use crate::actions::Action;
use crate::context::StrategyContext;
use crate::error::Result;
use crate::events::Event;
use async_trait::async_trait;

/// Core strategy plugin interface.
///
/// Implement this trait to create a trading strategy.
/// The engine calls `on_event` for every event routed to this strategy.
/// Return a `Vec<Action>` of actions to take (or empty vec for no action).
#[async_trait]
pub trait Strategy: Send + Sync {
    /// Unique name for this strategy (used in logs, DB, dashboard)
    fn name(&self) -> &str;

    /// Human-readable description
    fn description(&self) -> &str;

    /// Called when the engine starts this strategy.
    /// Use for initialization: subscribe to markets, set up state.
    async fn on_start(&mut self, ctx: &StrategyContext) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    /// Called for every event routed to this strategy.
    /// Return actions to execute (place orders, cancel, log, etc.)
    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>>;

    /// Called when the engine stops this strategy.
    /// Use for cleanup: cancel open orders, log final state.
    /// Return actions to execute during shutdown (e.g. CancelAllOrders).
    async fn on_stop(&mut self, ctx: &StrategyContext) -> Result<Vec<Action>> {
        let _ = ctx;
        Ok(vec![])
    }
}
