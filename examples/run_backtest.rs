use std::sync::Arc;

use chrono::Utc;
use polyrust_backtest::{
    BacktestConfig, BacktestEngine, BacktestReport, DataFetchConfig, DataFetcher,
    HistoricalDataStore,
};
use polyrust_core::prelude::*;
use polyrust_store::Store;
use rust_decimal_macros::dec;
use tracing_subscriber::EnvFilter;

/// Minimal example demonstrating the backtest pipeline.
///
/// This example:
/// 1. Creates a simple test strategy
/// 2. Initializes the historical data store
/// 3. Runs a backtest over a short time period
/// 4. Prints the results
///
/// Run with: cargo run --example run_backtest
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,polyrust_backtest=debug")),
        )
        .init();

    // Create a simple test strategy that just logs events
    struct TestStrategy;

    #[async_trait::async_trait]
    impl Strategy for TestStrategy {
        fn name(&self) -> &str {
            "test-strategy"
        }

        fn description(&self) -> &str {
            "Minimal test strategy for backtest example"
        }

        async fn on_start(&mut self, ctx: &StrategyContext) -> Result<()> {
            let balance = ctx.balance.read().await;
            tracing::info!(
                available = %balance.available_usdc,
                "Test strategy started"
            );
            Ok(())
        }

        async fn on_event(&mut self, event: &Event, _ctx: &StrategyContext) -> Result<Vec<Action>> {
            match event {
                Event::MarketData(MarketDataEvent::PriceChange {
                    token_id, price, ..
                }) => {
                    tracing::debug!(
                        token_id = %token_id,
                        price = %price,
                        "Price update received"
                    );
                }
                _ => {}
            }
            Ok(vec![])
        }

        async fn on_stop(&mut self, _ctx: &StrategyContext) -> Result<Vec<Action>> {
            tracing::info!("Test strategy stopped");
            Ok(vec![])
        }
    }

    // Configure backtest
    let config = BacktestConfig {
        strategy_name: "test-strategy".to_string(),
        market_ids: vec![], // Will use whatever data is available
        start_date: Utc::now() - chrono::Duration::days(7),
        end_date: Utc::now() - chrono::Duration::days(6),
        initial_balance: dec!(1000.00),
        data_fidelity_secs: 300,
        data_db_path: "backtest_data.db".to_string(),
        fees: Default::default(),
        market_duration_secs: None,
        fetch_concurrency: 10,
        offline: false,
        realism: Default::default(),
        sweep: None,
    };

    tracing::info!(
        start = %config.start_date,
        end = %config.end_date,
        initial_balance = %config.initial_balance,
        "Starting backtest example"
    );

    // Open historical data store
    let data_store = Arc::new(HistoricalDataStore::new(&config.data_db_path).await?);
    tracing::info!("Opened historical data store");

    // Create in-memory store for results
    let results_store = Arc::new(Store::new(":memory:").await?);

    // Initialize data fetcher (in a real backtest, you'd fetch data first)
    let fetch_config = DataFetchConfig {
        fidelity_secs: config.data_fidelity_secs,
    };
    let _data_fetcher = DataFetcher::new(Arc::clone(&data_store), fetch_config)?;
    tracing::info!("Initialized data fetcher (note: this example uses existing cached data)");

    // Create and run backtest engine
    let strategy = Box::new(TestStrategy);
    let start_time = config.start_date;
    let end_time = config.end_date;
    let initial_balance = config.initial_balance;

    let mut engine = BacktestEngine::new(
        config,
        strategy,
        Arc::clone(&data_store),
        Arc::clone(&results_store),
    )
    .await;

    tracing::info!("Running backtest simulation");
    let trades = engine.run().await?;

    // Generate report from results
    let report = BacktestReport::from_engine_results(
        results_store,
        trades,
        initial_balance,
        start_time,
        end_time,
    )
    .await?;

    // Print results
    println!("\n{}", report.summary());
    println!(
        "\nJSON report:\n{}",
        serde_json::to_string_pretty(&report.to_json())?
    );

    tracing::info!("Backtest example complete");
    Ok(())
}
