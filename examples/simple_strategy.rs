//! Minimal example: a strategy that logs every event it receives.
//!
//! Run with: `cargo run --example simple_strategy`

use async_trait::async_trait;
use polyrust_core::prelude::*;
use polyrust_execution::{FillMode, PaperBackend};
use rust_decimal::Decimal;
use tracing_subscriber::EnvFilter;

/// A logging-only strategy that prints every event topic it receives.
struct LoggingStrategy {
    event_count: u64,
}

impl LoggingStrategy {
    fn new() -> Self {
        Self { event_count: 0 }
    }
}

#[async_trait]
impl Strategy for LoggingStrategy {
    fn name(&self) -> &str {
        "logging"
    }

    fn description(&self) -> &str {
        "Logs every event topic (example strategy)"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        tracing::info!("LoggingStrategy started");
        Ok(())
    }

    async fn on_event(&mut self, event: &Event, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        self.event_count += 1;
        let mut actions = vec![Action::Log {
            level: LogLevel::Info,
            message: format!("event #{}: topic={}", self.event_count, event.topic()),
        }];

        // Every 10 events, emit a heartbeat signal
        if self.event_count % 10 == 0 {
            actions.push(Action::EmitSignal {
                signal_type: "heartbeat".into(),
                payload: serde_json::json!({ "event_count": self.event_count }),
            });
        }

        Ok(actions)
    }

    async fn on_stop(&mut self, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        tracing::info!("LoggingStrategy stopped after {} events", self.event_count);
        Ok(vec![])
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,polyrust=debug")),
        )
        .init();

    tracing::info!("simple_strategy example starting");

    let backend = PaperBackend::new(Decimal::new(10_000, 0), FillMode::Immediate);
    let strategy = LoggingStrategy::new();

    let mut engine = Engine::builder()
        .strategy(strategy)
        .execution(backend)
        .build()
        .await?;

    tracing::info!("engine built, running (press Ctrl+C to stop)");
    engine.run().await?;

    Ok(())
}
