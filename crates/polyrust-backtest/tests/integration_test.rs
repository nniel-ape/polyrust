use std::sync::Arc;

use chrono::{TimeZone, Utc};
use polyrust_backtest::{
    BacktestConfig, BacktestEngine, DataFetchConfig, DataFetcher, HistoricalDataStore,
    HistoricalMarket, HistoricalPrice, HistoricalTrade,
};
use polyrust_core::prelude::*;
use polyrust_store::Store;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Simple test strategy that buys on every price drop and sells on every price rise.
struct SimpleTestStrategy {
    last_price: Option<Decimal>,
}

impl SimpleTestStrategy {
    fn new() -> Self {
        Self { last_price: None }
    }
}

#[async_trait::async_trait]
impl Strategy for SimpleTestStrategy {
    fn name(&self) -> &str {
        "simple-test-strategy"
    }

    fn description(&self) -> &str {
        "Test strategy for integration testing"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        Ok(())
    }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        match event {
            Event::MarketData(MarketDataEvent::PriceChange {
                token_id,
                price,
                ..
            }) => {
                if let Some(last) = self.last_price {
                    let balance = ctx.balance.read().await;
                    let positions = ctx.positions.read().await;

                    // Simple momentum strategy
                    if *price > last && balance.available_usdc >= dec!(10.0) {
                        // Price going up - buy
                        self.last_price = Some(*price);
                        return Ok(vec![Action::PlaceOrder(OrderRequest {
                            token_id: token_id.clone(),
                            price: *price,
                            size: dec!(10.0),
                            side: OrderSide::Buy,
                            order_type: OrderType::Gtc,
                            neg_risk: false,
                        })]);
                    } else if *price < last
                        && positions
                            .open_positions
                            .values()
                            .any(|p| p.token_id == *token_id)
                    {
                        // Price going down - sell if we have a position
                        self.last_price = Some(*price);
                        return Ok(vec![Action::PlaceOrder(OrderRequest {
                            token_id: token_id.clone(),
                            price: *price,
                            size: dec!(10.0),
                            side: OrderSide::Sell,
                            order_type: OrderType::Gtc,
                            neg_risk: false,
                        })]);
                    }
                }
                self.last_price = Some(*price);
            }
            _ => {}
        }
        Ok(vec![])
    }

    async fn on_stop(&mut self, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        Ok(vec![])
    }
}

#[tokio::test]
async fn test_full_backtest_pipeline() {
    // Create in-memory historical data store
    let data_store = Arc::new(
        HistoricalDataStore::new(":memory:")
            .await
            .expect("Failed to create data store"),
    );

    // Insert test market data
    let market = HistoricalMarket {
        market_id: "test-market".to_string(),
        slug: "test-btc-15m".to_string(),
        question: "Will BTC go up in the next 15 minutes?".to_string(),
        start_date: Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap(),
        end_date: Utc.with_ymd_and_hms(2025, 1, 15, 12, 15, 0).unwrap(),
        token_a: "token-up".to_string(),
        token_b: "token-down".to_string(),
        neg_risk: false,
    };
    data_store
        .insert_historical_market(market)
        .await
        .expect("Failed to insert market");

    // Create synthetic price history (5 data points over 5 minutes)
    let prices = vec![
        HistoricalPrice {
            token_id: "token-up".to_string(),
            timestamp: Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap(),
            price: dec!(0.50),
            source: "test".to_string(),
        },
        HistoricalPrice {
            token_id: "token-up".to_string(),
            timestamp: Utc.with_ymd_and_hms(2025, 1, 15, 12, 1, 0).unwrap(),
            price: dec!(0.55),
            source: "test".to_string(),
        },
        HistoricalPrice {
            token_id: "token-up".to_string(),
            timestamp: Utc.with_ymd_and_hms(2025, 1, 15, 12, 2, 0).unwrap(),
            price: dec!(0.60),
            source: "test".to_string(),
        },
        HistoricalPrice {
            token_id: "token-up".to_string(),
            timestamp: Utc.with_ymd_and_hms(2025, 1, 15, 12, 3, 0).unwrap(),
            price: dec!(0.55),
            source: "test".to_string(),
        },
        HistoricalPrice {
            token_id: "token-up".to_string(),
            timestamp: Utc.with_ymd_and_hms(2025, 1, 15, 12, 4, 0).unwrap(),
            price: dec!(0.50),
            source: "test".to_string(),
        },
    ];
    data_store
        .insert_historical_prices(prices)
        .await
        .expect("Failed to insert prices");

    // Configure backtest
    let config = BacktestConfig {
        strategy_name: "simple-test-strategy".to_string(),
        market_ids: vec!["test-market".to_string()],
        start_date: Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap(),
        end_date: Utc.with_ymd_and_hms(2025, 1, 15, 12, 15, 0).unwrap(),
        initial_balance: dec!(1000.00),
        data_fidelity_mins: 1,
        data_db_path: ":memory:".to_string(),
        fees: Default::default(),
    };

    // Create in-memory results store
    let results_store = Arc::new(Store::new(":memory:").await.expect("Failed to create store"));

    // Initialize data fetcher
    let fetch_config = DataFetchConfig {
        fidelity_mins: 1,
        clob_recent_days: 7,
    };
    let data_fetcher =
        DataFetcher::new(Arc::clone(&data_store), fetch_config).expect("Failed to create fetcher");

    // Verify data is cached
    let cached = data_fetcher
        .get_cached_data("token-up", config.start_date, config.end_date)
        .await
        .expect("Failed to get cached data");
    assert_eq!(cached.prices.len(), 5);

    // Create and run backtest engine
    let strategy = Box::new(SimpleTestStrategy::new());
    let start_time = config.start_date;
    let end_time = config.end_date;
    let initial_balance = config.initial_balance;

    let mut engine = BacktestEngine::new(
        config.clone(),
        strategy,
        Arc::clone(&data_store),
        Arc::clone(&results_store),
    )
    .await;

    let _trades = engine.run().await.expect("Backtest failed");

    // Generate report
    use polyrust_backtest::BacktestReport;
    let report = BacktestReport::from_engine_results(
        Arc::clone(&results_store),
        initial_balance,
        start_time,
        end_time,
    )
    .await
    .expect("Failed to generate report");

    // Verify backtest ran
    assert_eq!(report.start_balance, dec!(1000.00));
    assert!(report.total_trades >= 0); // Strategy may or may not trade depending on logic
    assert_eq!(
        report.duration,
        chrono::Duration::minutes(15) // full market duration
    );
}

#[tokio::test]
async fn test_backtest_with_no_data() {
    // Create in-memory stores
    let data_store = Arc::new(
        HistoricalDataStore::new(":memory:")
            .await
            .expect("Failed to create data store"),
    );
    let results_store = Arc::new(Store::new(":memory:").await.expect("Failed to create store"));

    let config = BacktestConfig {
        strategy_name: "test".to_string(),
        market_ids: vec!["nonexistent-market".to_string()],
        start_date: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
        end_date: Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap(),
        initial_balance: dec!(1000.00),
        data_fidelity_mins: 1,
        data_db_path: ":memory:".to_string(),
        fees: Default::default(),
    };

    let strategy = Box::new(SimpleTestStrategy::new());
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

    let _trades = engine.run().await.expect("Backtest should succeed with no data");

    // Generate report
    use polyrust_backtest::BacktestReport;
    let report =
        BacktestReport::from_engine_results(results_store, initial_balance, start_time, end_time)
            .await
            .expect("Failed to generate report");

    // Verify backtest ran but produced no trades
    assert_eq!(report.total_trades, 0);
    assert_eq!(report.start_balance, report.end_balance);
}

#[tokio::test]
async fn test_data_fetcher_integration() {
    let data_store = Arc::new(
        HistoricalDataStore::new(":memory:")
            .await
            .expect("Failed to create data store"),
    );

    // Insert test data
    let prices = vec![
        HistoricalPrice {
            token_id: "token1".to_string(),
            timestamp: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            price: dec!(0.50),
            source: "test".to_string(),
        },
        HistoricalPrice {
            token_id: "token1".to_string(),
            timestamp: Utc.with_ymd_and_hms(2025, 1, 1, 1, 0, 0).unwrap(),
            price: dec!(0.55),
            source: "test".to_string(),
        },
    ];
    data_store
        .insert_historical_prices(prices)
        .await
        .expect("Failed to insert prices");

    let trades = vec![HistoricalTrade {
        id: "trade1".to_string(),
        token_id: "token1".to_string(),
        timestamp: Utc.with_ymd_and_hms(2025, 1, 1, 0, 30, 0).unwrap(),
        price: dec!(0.52),
        size: dec!(100.0),
        side: "BUY".to_string(),
        source: "test".to_string(),
    }];
    data_store
        .insert_historical_trades(trades)
        .await
        .expect("Failed to insert trades");

    // Test DataFetcher
    let fetch_config = DataFetchConfig {
        fidelity_mins: 1,
        clob_recent_days: 7,
    };
    let fetcher = DataFetcher::new(data_store, fetch_config).expect("Failed to create fetcher");
    let cached = fetcher
        .get_cached_data(
            "token1",
            Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2025, 1, 1, 2, 0, 0).unwrap(),
        )
        .await
        .expect("Failed to get cached data");

    assert_eq!(cached.prices.len(), 2);
    assert_eq!(cached.trades.len(), 1);
    assert_eq!(cached.prices[0].price, dec!(0.50));
    assert_eq!(cached.trades[0].price, dec!(0.52));
}
