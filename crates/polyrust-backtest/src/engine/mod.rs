use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use polyrust_core::actions::Action;
use polyrust_core::context::{BalanceState, StrategyContext};
use polyrust_core::error::Result;
use polyrust_core::events::{Event, MarketDataEvent};
use polyrust_core::strategy::Strategy;
use polyrust_core::types::*;
use polyrust_store::Store;

use crate::config::BacktestConfig;
use crate::data::store::HistoricalDataStore;

/// Historical market data loaded from the database for replay.
#[derive(Debug, Clone)]
pub struct HistoricalEvent {
    pub timestamp: DateTime<Utc>,
    pub token_id: String,
    pub event: Event,
}

/// A completed backtest trade with realized P&L.
#[derive(Debug, Clone)]
pub struct BacktestTrade {
    pub timestamp: DateTime<Utc>,
    pub token_id: String,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub realized_pnl: Option<Decimal>,
}

/// Backtesting engine that replays historical events through a strategy.
///
/// This engine:
/// - Loads historical data from HistoricalDataStore (persistent cache)
/// - Generates a chronologically-sorted event stream
/// - Advances a simulated clock through each event
/// - Executes strategy logic and collects actions
/// - Simulates immediate fills at current market price
/// - Tracks positions and balance
/// - Records trades to an in-memory Store (using existing schema)
pub struct BacktestEngine {
    config: BacktestConfig,
    strategy: Box<dyn Strategy>,
    data_store: Arc<HistoricalDataStore>,
    store: Arc<Store>,
    ctx: StrategyContext,
    current_time: DateTime<Utc>,
    /// Token price cache: token_id -> latest price
    token_prices: HashMap<String, Decimal>,
    /// Track entry prices for P&L calculation: token_id -> (size, avg_entry_price)
    position_entries: HashMap<String, (Decimal, Decimal)>,
}

impl BacktestEngine {
    /// Create a new backtest engine.
    ///
    /// - `config`: backtest configuration
    /// - `strategy`: strategy to test
    /// - `data_store`: historical data cache (persistent DB)
    /// - `store`: fresh in-memory Store for recording simulated trades
    pub async fn new(
        config: BacktestConfig,
        strategy: Box<dyn Strategy>,
        data_store: Arc<HistoricalDataStore>,
        store: Arc<Store>,
    ) -> Self {
        let ctx = StrategyContext::new();
        let current_time = config.start_date;

        // Initialize balance
        let balance = BalanceState {
            available_usdc: config.initial_balance,
            ..Default::default()
        };

        // Update context with initial balance
        {
            let mut bal = ctx.balance.write().await;
            *bal = balance;
        }

        Self {
            config,
            strategy,
            data_store,
            store,
            ctx,
            current_time,
            token_prices: HashMap::new(),
            position_entries: HashMap::new(),
        }
    }

    /// Run the backtest from start_date to end_date.
    ///
    /// Returns the list of all trades executed during the backtest.
    pub async fn run(&mut self) -> Result<Vec<BacktestTrade>> {
        info!(
            strategy = self.strategy.name(),
            start = %self.config.start_date,
            end = %self.config.end_date,
            "Starting backtest"
        );

        // Call strategy.on_start
        self.strategy.on_start(&self.ctx).await?;

        // Load historical events
        let events = self.load_events().await?;
        info!(event_count = events.len(), "Loaded historical events");

        // Validate that we have events to replay
        if events.is_empty() {
            return Err(polyrust_core::error::PolyError::Config(
                "No historical events found for configured market_ids and date range. \
                Check that data has been fetched and cached in the backtest database."
                    .to_string(),
            ));
        }

        let mut trades = Vec::new();

        // Replay events in chronological order
        for historical_event in events {
            self.current_time = historical_event.timestamp;

            // Update token price cache if this is a price/trade event
            match &historical_event.event {
                Event::MarketData(MarketDataEvent::PriceChange { token_id, price, .. }) => {
                    self.token_prices.insert(token_id.clone(), *price);
                }
                Event::MarketData(MarketDataEvent::Trade { token_id, price, .. }) => {
                    self.token_prices.insert(token_id.clone(), *price);
                }
                _ => {}
            }

            // Update market data state (so strategy can access latest prices)
            self.update_market_data_state(&historical_event.event).await;

            // Call strategy.on_event
            let actions = self
                .strategy
                .on_event(&historical_event.event, &self.ctx)
                .await?;

            // Execute actions
            for action in actions {
                if let Some(trade) = self.execute_action(action).await? {
                    trades.push(trade);
                }
            }
        }

        // Emit MarketExpired events for all markets at end_date
        self.current_time = self.config.end_date;
        let market_ids = self.config.market_ids.clone();
        for market_id in market_ids {
            let expiration_event = Event::MarketData(MarketDataEvent::MarketExpired(market_id));
            let actions = self.strategy.on_event(&expiration_event, &self.ctx).await?;
            for action in actions {
                if let Some(trade) = self.execute_action(action).await? {
                    trades.push(trade);
                }
            }
        }

        // Call strategy.on_stop
        let final_actions = self.strategy.on_stop(&self.ctx).await?;
        for action in final_actions {
            if let Some(trade) = self.execute_action(action).await? {
                trades.push(trade);
            }
        }

        info!(
            strategy = self.strategy.name(),
            trade_count = trades.len(),
            "Backtest complete"
        );

        Ok(trades)
    }

    /// Load historical events from the data store.
    async fn load_events(&self) -> Result<Vec<HistoricalEvent>> {
        let mut events = Vec::new();

        // For each market_id, load prices and trades for both tokens
        for market_id in &self.config.market_ids {
            // Query the historical_markets table to get both token IDs
            let market = self
                .data_store
                .get_historical_market(market_id)
                .await
                .map_err(|e| polyrust_core::error::PolyError::Config(e.to_string()))?;

            let token_ids = if let Some(m) = market {
                vec![m.token_a, m.token_b]
            } else {
                // Market not found in cache - assume market_id IS a token_id for backwards compatibility
                warn!(market_id, "Market not found in cache, treating as token_id");
                vec![market_id.clone()]
            };

            // Load data for each token in the market
            for token_id in token_ids {
                // Load price history
                let prices = self
                    .data_store
                    .get_historical_prices(
                        &token_id,
                        self.config.start_date,
                        self.config.end_date,
                    )
                    .await
                    .map_err(|e| polyrust_core::error::PolyError::Config(e.to_string()))?;

                for price in prices {
                    events.push(HistoricalEvent {
                        timestamp: price.timestamp,
                        token_id: price.token_id.clone(),
                        event: Event::MarketData(MarketDataEvent::PriceChange {
                            token_id: price.token_id,
                            price: price.price,
                            side: OrderSide::Buy, // Simplified: not tracking side in cache
                        best_bid: price.price,
                        best_ask: price.price,
                    }),
                });
                }

                // Load trade history for this token
                let trades = self
                    .data_store
                    .get_historical_trades(
                        &token_id,
                        self.config.start_date,
                        self.config.end_date,
                    )
                    .await
                    .map_err(|e| polyrust_core::error::PolyError::Config(e.to_string()))?;

                for trade in trades {
                    events.push(HistoricalEvent {
                        timestamp: trade.timestamp,
                        token_id: trade.token_id.clone(),
                        event: Event::MarketData(MarketDataEvent::Trade {
                            token_id: trade.token_id,
                            price: trade.price,
                            size: trade.size,
                            timestamp: trade.timestamp,
                        }),
                    });
                }
            } // end token_ids loop
        } // end market_ids loop

        // Sort events chronologically
        events.sort_by_key(|e| e.timestamp);

        Ok(events)
    }

    /// Update the market data state based on an event.
    async fn update_market_data_state(&self, event: &Event) {
        let mut market_data = self.ctx.market_data.write().await;

        match event {
            Event::MarketData(MarketDataEvent::PriceChange { token_id, price, .. }) => {
                // Update external_prices as a simple cache
                market_data
                    .external_prices
                    .insert(token_id.clone(), *price);
            }
            Event::MarketData(MarketDataEvent::Trade { token_id, price, .. }) => {
                market_data
                    .external_prices
                    .insert(token_id.clone(), *price);
            }
            _ => {}
        }
    }

    /// Execute a single action from the strategy.
    ///
    /// Returns Some(BacktestTrade) if the action resulted in a trade.
    async fn execute_action(&mut self, action: Action) -> Result<Option<BacktestTrade>> {
        match action {
            Action::PlaceOrder(order_req) => self.execute_order(order_req).await,
            Action::PlaceBatchOrder(orders) => {
                // Execute each order in the batch
                // NOTE: All trades are persisted to Store and included in the final report.
                // This return value only affects the in-memory trades list used for logging.
                let mut batch_trades = Vec::new();
                for order in orders {
                    if let Some(trade) = self.execute_order(order).await? {
                        batch_trades.push(trade);
                    }
                }
                // Return the first trade for simplicity (all trades are in Store)
                Ok(batch_trades.into_iter().next())
            }
            Action::Log { level, message } => {
                match level {
                    polyrust_core::actions::LogLevel::Debug => debug!("{}", message),
                    polyrust_core::actions::LogLevel::Info => info!("{}", message),
                    polyrust_core::actions::LogLevel::Warn => warn!("{}", message),
                    polyrust_core::actions::LogLevel::Error => {
                        tracing::error!("{}", message)
                    }
                }
                Ok(None)
            }
            _ => {
                // Other actions (CancelOrder, EmitSignal, etc.) are not simulated in backtest
                debug!("Ignoring action: {:?}", action);
                Ok(None)
            }
        }
    }

    /// Execute an order immediately at the current market price.
    ///
    /// This is a simplified "Immediate fill mode" implementation.
    /// Historical orderbook depth is not available from Polymarket APIs.
    async fn execute_order(&mut self, order: OrderRequest) -> Result<Option<BacktestTrade>> {
        let current_price = self
            .token_prices
            .get(&order.token_id)
            .cloned()
            .unwrap_or(order.price);

        // Validate price and size
        if order.price <= Decimal::ZERO || order.price > Decimal::ONE {
            warn!(
                token_id = %order.token_id,
                price = %order.price,
                "Invalid order price, skipping"
            );
            return Ok(None);
        }
        if order.size <= Decimal::ZERO {
            warn!(
                token_id = %order.token_id,
                size = %order.size,
                "Invalid order size, skipping"
            );
            return Ok(None);
        }

        let mut balance = self.ctx.balance.write().await;
        let mut positions = self.ctx.positions.write().await;

        match order.side {
            OrderSide::Buy => {
                // Calculate cost (price * size) + fee
                let cost = current_price * order.size;
                let fee = cost * self.config.fees.taker_fee_rate;
                let total_cost = cost + fee;

                if balance.available_usdc < total_cost {
                    warn!(
                        token_id = %order.token_id,
                        cost = %total_cost,
                        balance = %balance.available_usdc,
                        "Insufficient balance for BUY, skipping"
                    );
                    return Ok(None);
                }

                // Deduct balance
                balance.available_usdc -= total_cost;

                // Update position entry tracking
                // Include fees in the effective entry price for accurate P&L calculation
                let effective_buy_price = current_price * (Decimal::ONE + self.config.fees.taker_fee_rate);

                let (cur_size, cur_entry) = self
                    .position_entries
                    .get(&order.token_id)
                    .cloned()
                    .unwrap_or((Decimal::ZERO, Decimal::ZERO));
                let new_size = cur_size + order.size;
                let new_entry = if new_size > Decimal::ZERO {
                    (cur_entry * cur_size + effective_buy_price * order.size) / new_size
                } else {
                    effective_buy_price
                };
                self.position_entries
                    .insert(order.token_id.clone(), (new_size, new_entry));

                // Update PositionState
                // Find existing position or create new one
                let existing_pos = positions
                    .open_positions
                    .iter()
                    .find(|(_, p)| {
                        p.token_id == order.token_id && p.strategy_name == self.strategy.name()
                    })
                    .map(|(id, _)| *id);

                if let Some(pos_id) = existing_pos {
                    // Update existing position
                    if let Some(pos) = positions.open_positions.get_mut(&pos_id) {
                        pos.size = new_size;
                        pos.entry_price = new_entry;
                        pos.current_price = current_price;
                    }
                } else {
                    // Create new position
                    let position_id = Uuid::new_v4();
                    positions.open_positions.insert(
                        position_id,
                        Position {
                            id: position_id,
                            market_id: String::new(), // Not tracked in backtest
                            token_id: order.token_id.clone(),
                            side: OutcomeSide::Yes, // Simplified
                            entry_price: new_entry,
                            size: new_size,
                            current_price,
                            entry_time: self.current_time,
                            strategy_name: self.strategy.name().to_string(),
                        },
                    );
                }

                // Record trade in Store
                let trade = Trade {
                    id: Uuid::new_v4(),
                    order_id: Uuid::new_v4().to_string(),
                    market_id: String::new(),
                    token_id: order.token_id.clone(),
                    side: OrderSide::Buy,
                    price: current_price,
                    size: order.size,
                    realized_pnl: None,
                    strategy_name: self.strategy.name().to_string(),
                    timestamp: self.current_time,
                };
                self.store.insert_trade(&trade).await.map_err(|e| {
                    polyrust_core::error::PolyError::Execution(format!(
                        "Failed to insert trade: {}",
                        e
                    ))
                })?;

                debug!(
                    token_id = %order.token_id,
                    price = %current_price,
                    size = %order.size,
                    cost = %total_cost,
                    "BUY order filled"
                );

                Ok(Some(BacktestTrade {
                    timestamp: self.current_time,
                    token_id: order.token_id,
                    side: OrderSide::Buy,
                    price: current_price,
                    size: order.size,
                    realized_pnl: None,
                }))
            }
            OrderSide::Sell => {
                // Check if we have enough position
                let (cur_size, entry_price) = self
                    .position_entries
                    .get(&order.token_id)
                    .cloned()
                    .unwrap_or((Decimal::ZERO, Decimal::ZERO));

                if cur_size < order.size {
                    warn!(
                        token_id = %order.token_id,
                        requested = %order.size,
                        available = %cur_size,
                        "Insufficient position for SELL, skipping"
                    );
                    return Ok(None);
                }

                // Calculate revenue (price * size) - fee
                let revenue = current_price * order.size;
                let fee = revenue * self.config.fees.taker_fee_rate;
                let net_revenue = revenue - fee;

                // Calculate realized P&L
                let cost_basis = entry_price * order.size;
                let realized_pnl = net_revenue - cost_basis;

                // Add revenue to balance
                balance.available_usdc += net_revenue;

                // Update position tracking
                let new_size = cur_size - order.size;
                if new_size > Decimal::ZERO {
                    self.position_entries
                        .insert(order.token_id.clone(), (new_size, entry_price));
                } else {
                    self.position_entries.remove(&order.token_id);
                }

                // Update PositionState (remove or reduce position)
                // Find the position to update
                let position_to_update = positions
                    .open_positions
                    .iter()
                    .find(|(_, p)| p.token_id == order.token_id && p.strategy_name == self.strategy.name())
                    .map(|(id, _)| *id);

                if let Some(pos_id) = position_to_update {
                    if new_size > Decimal::ZERO {
                        if let Some(pos) = positions.open_positions.get_mut(&pos_id) {
                            pos.size = new_size;
                            pos.current_price = current_price;
                        }
                    } else {
                        positions.open_positions.remove(&pos_id);
                    }
                }

                // Record trade in Store
                let trade = Trade {
                    id: Uuid::new_v4(),
                    order_id: Uuid::new_v4().to_string(),
                    market_id: String::new(),
                    token_id: order.token_id.clone(),
                    side: OrderSide::Sell,
                    price: current_price,
                    size: order.size,
                    realized_pnl: Some(realized_pnl),
                    strategy_name: self.strategy.name().to_string(),
                    timestamp: self.current_time,
                };
                self.store.insert_trade(&trade).await.map_err(|e| {
                    polyrust_core::error::PolyError::Execution(format!(
                        "Failed to insert trade: {}",
                        e
                    ))
                })?;

                debug!(
                    token_id = %order.token_id,
                    price = %current_price,
                    size = %order.size,
                    revenue = %net_revenue,
                    realized_pnl = %realized_pnl,
                    "SELL order filled"
                );

                Ok(Some(BacktestTrade {
                    timestamp: self.current_time,
                    token_id: order.token_id,
                    side: OrderSide::Sell,
                    price: current_price,
                    size: order.size,
                    realized_pnl: Some(realized_pnl),
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FeeConfig;
    use async_trait::async_trait;
    use polyrust_core::actions::Action;
    use polyrust_core::context::StrategyContext;
    use polyrust_core::error::Result;
    use polyrust_core::events::Event;
    use polyrust_core::strategy::Strategy;
    use rust_decimal_macros::dec;

    // Simple test strategy that buys on first event, sells on second
    struct TestStrategy {
        event_count: usize,
    }

    #[async_trait]
    impl Strategy for TestStrategy {
        fn name(&self) -> &str {
            "test-strategy"
        }

        fn description(&self) -> &str {
            "Test strategy for backtest engine"
        }

        async fn on_event(&mut self, event: &Event, _ctx: &StrategyContext) -> Result<Vec<Action>> {
            self.event_count += 1;

            match event {
                Event::MarketData(MarketDataEvent::PriceChange { token_id, .. }) => {
                    if self.event_count == 1 {
                        // First event: BUY
                        Ok(vec![Action::PlaceOrder(OrderRequest {
                            token_id: token_id.clone(),
                            price: dec!(0.50),
                            size: dec!(10),
                            side: OrderSide::Buy,
                            order_type: OrderType::Gtc,
                            neg_risk: false,
                        })])
                    } else if self.event_count == 2 {
                        // Second event: SELL
                        Ok(vec![Action::PlaceOrder(OrderRequest {
                            token_id: token_id.clone(),
                            price: dec!(0.60),
                            size: dec!(10),
                            side: OrderSide::Sell,
                            order_type: OrderType::Gtc,
                            neg_risk: false,
                        })])
                    } else {
                        Ok(vec![])
                    }
                }
                _ => Ok(vec![]),
            }
        }
    }

    #[tokio::test]
    async fn backtest_engine_executes_buy_and_sell() {
        // Create an in-memory Store
        let store = Arc::new(Store::new(":memory:").await.unwrap());

        // Create an in-memory HistoricalDataStore
        let data_store = Arc::new(HistoricalDataStore::new(":memory:").await.unwrap());

        // Insert test price data
        data_store
            .insert_historical_prices(vec![
                crate::data::store::HistoricalPrice {
                    token_id: "token1".to_string(),
                    timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                    price: dec!(0.50),
                    source: "test".to_string(),
                },
                crate::data::store::HistoricalPrice {
                    token_id: "token1".to_string(),
                    timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                    price: dec!(0.60),
                    source: "test".to_string(),
                },
            ])
            .await
            .unwrap();

        // Create config
        let config = BacktestConfig {
            strategy_name: "test-strategy".to_string(),
            market_ids: vec!["token1".to_string()],
            start_date: DateTime::from_timestamp(500, 0).unwrap(),
            end_date: DateTime::from_timestamp(3000, 0).unwrap(),
            initial_balance: dec!(1000),
            data_fidelity_mins: 1,
            data_db_path: ":memory:".to_string(),
            fees: FeeConfig {
                taker_fee_rate: dec!(0.01),
            },
        };

        let strategy = Box::new(TestStrategy { event_count: 0 });

        let mut engine = BacktestEngine::new(config, strategy, data_store, store.clone()).await;

        let trades = engine.run().await.unwrap();

        // Should have 2 trades: BUY and SELL
        assert_eq!(trades.len(), 2);

        // First trade: BUY at 0.50
        assert_eq!(trades[0].side, OrderSide::Buy);
        assert_eq!(trades[0].price, dec!(0.50));
        assert_eq!(trades[0].size, dec!(10));

        // Second trade: SELL at 0.60
        assert_eq!(trades[1].side, OrderSide::Sell);
        assert_eq!(trades[1].price, dec!(0.60));
        assert_eq!(trades[1].size, dec!(10));

        // Check realized P&L on SELL trade
        // Buy cost: 0.50 * 10 = 5.00 + 1% fee = 5.05
        // Sell revenue: 0.60 * 10 = 6.00 - 1% fee = 5.94
        // Realized P&L = 5.94 - 5.00 = 0.94
        assert!(trades[1].realized_pnl.is_some());
        let pnl = trades[1].realized_pnl.unwrap();
        // Expected: (0.60 * 10 * 0.99) - (0.50 * 10) = 5.94 - 5.00 = 0.94
        assert!(pnl > dec!(0.8) && pnl < dec!(1.0)); // Rough check due to fees

        // Verify trades were recorded in Store
        let stored_trades = store.list_trades(Some("test-strategy"), 10).await.unwrap();
        assert_eq!(stored_trades.len(), 2);
    }

    #[tokio::test]
    async fn backtest_engine_sorts_events_chronologically() {
        let store = Arc::new(Store::new(":memory:").await.unwrap());
        let data_store = Arc::new(HistoricalDataStore::new(":memory:").await.unwrap());

        // Insert price data in reverse chronological order
        data_store
            .insert_historical_prices(vec![
                crate::data::store::HistoricalPrice {
                    token_id: "token1".to_string(),
                    timestamp: DateTime::from_timestamp(3000, 0).unwrap(),
                    price: dec!(0.70),
                    source: "test".to_string(),
                },
                crate::data::store::HistoricalPrice {
                    token_id: "token1".to_string(),
                    timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                    price: dec!(0.50),
                    source: "test".to_string(),
                },
                crate::data::store::HistoricalPrice {
                    token_id: "token1".to_string(),
                    timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                    price: dec!(0.60),
                    source: "test".to_string(),
                },
            ])
            .await
            .unwrap();

        let config = BacktestConfig {
            strategy_name: "test-strategy".to_string(),
            market_ids: vec!["token1".to_string()],
            start_date: DateTime::from_timestamp(500, 0).unwrap(),
            end_date: DateTime::from_timestamp(4000, 0).unwrap(),
            initial_balance: dec!(1000),
            data_fidelity_mins: 1,
            data_db_path: ":memory:".to_string(),
            fees: FeeConfig {
                taker_fee_rate: dec!(0.01),
            },
        };

        let strategy = Box::new(TestStrategy { event_count: 0 });
        let mut engine = BacktestEngine::new(config, strategy, data_store, store).await;

        let trades = engine.run().await.unwrap();

        // Strategy should receive events in chronological order
        // First event at t=1000 (0.50) -> BUY
        // Second event at t=2000 (0.60) -> SELL
        assert_eq!(trades[0].timestamp.timestamp(), 1000);
        assert_eq!(trades[0].price, dec!(0.50));
        assert_eq!(trades[1].timestamp.timestamp(), 2000);
        assert_eq!(trades[1].price, dec!(0.60));
    }

    #[tokio::test]
    async fn backtest_engine_insufficient_balance_skips_order() {
        let store = Arc::new(Store::new(":memory:").await.unwrap());
        let data_store = Arc::new(HistoricalDataStore::new(":memory:").await.unwrap());

        data_store
            .insert_historical_prices(vec![crate::data::store::HistoricalPrice {
                token_id: "token1".to_string(),
                timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                price: dec!(0.50),
                source: "test".to_string(),
            }])
            .await
            .unwrap();

        let config = BacktestConfig {
            strategy_name: "test-strategy".to_string(),
            market_ids: vec!["token1".to_string()],
            start_date: DateTime::from_timestamp(500, 0).unwrap(),
            end_date: DateTime::from_timestamp(2000, 0).unwrap(),
            initial_balance: dec!(1.0), // Insufficient for 0.50 * 10 = 5.00 + fee
            data_fidelity_mins: 1,
            data_db_path: ":memory:".to_string(),
            fees: FeeConfig {
                taker_fee_rate: dec!(0.01),
            },
        };

        let strategy = Box::new(TestStrategy { event_count: 0 });
        let mut engine = BacktestEngine::new(config, strategy, data_store, store).await;

        let trades = engine.run().await.unwrap();

        // Should have 0 trades (BUY was skipped due to insufficient balance)
        assert_eq!(trades.len(), 0);
    }
}
