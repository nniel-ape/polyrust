use crate::strategy::Strategy;
use crate::types::*;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use tokio::sync::RwLock;

/// A thread-safe handle to a boxed strategy.
pub type StrategyHandle = Arc<RwLock<Box<dyn Strategy>>>;

/// Thread-safe shared state accessible by all strategies
#[derive(Clone)]
pub struct StrategyContext {
    pub positions: Arc<RwLock<PositionState>>,
    pub market_data: Arc<RwLock<MarketDataState>>,
    pub balance: Arc<RwLock<BalanceState>>,
    pub strategy_count: Arc<AtomicUsize>,
    /// Strategies that provide custom dashboard views, keyed by view name.
    pub strategy_views: Arc<RwLock<HashMap<String, StrategyHandle>>>,
}

impl StrategyContext {
    pub fn new() -> Self {
        Self {
            positions: Arc::new(RwLock::new(PositionState::default())),
            market_data: Arc::new(RwLock::new(MarketDataState::default())),
            balance: Arc::new(RwLock::new(BalanceState::default())),
            strategy_count: Arc::new(AtomicUsize::new(0)),
            strategy_views: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Returns the view names of all strategies that have custom dashboard views.
    pub async fn strategy_names(&self) -> Vec<String> {
        let views = self.strategy_views.read().await;
        let mut names: Vec<String> = views.keys().cloned().collect();
        names.sort();
        names
    }
}

impl Default for StrategyContext {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for StrategyContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StrategyContext")
            .field("positions", &self.positions)
            .field("market_data", &self.market_data)
            .field("balance", &self.balance)
            .field("strategy_count", &self.strategy_count)
            .field("strategy_views", &"<strategy_views>")
            .finish()
    }
}

#[derive(Debug, Default)]
pub struct PositionState {
    pub open_positions: HashMap<uuid::Uuid, Position>,
    pub open_orders: HashMap<OrderId, Order>,
}

impl PositionState {
    pub fn position_count(&self) -> usize {
        self.open_positions.len()
    }

    pub fn positions_for_strategy(&self, name: &str) -> Vec<&Position> {
        self.open_positions
            .values()
            .filter(|p| p.strategy_name == name)
            .collect()
    }

    pub fn total_unrealized_pnl(&self) -> Decimal {
        self.open_positions
            .values()
            .map(|p| p.unrealized_pnl())
            .sum()
    }
}

#[derive(Debug, Default)]
pub struct MarketDataState {
    pub orderbooks: HashMap<TokenId, OrderbookSnapshot>,
    pub markets: HashMap<MarketId, MarketInfo>,
    pub external_prices: HashMap<String, Decimal>,
}

#[derive(Debug)]
pub struct BalanceState {
    pub available_usdc: Decimal,
    pub locked_usdc: Decimal,
}

impl Default for BalanceState {
    fn default() -> Self {
        Self {
            available_usdc: Decimal::ZERO,
            locked_usdc: Decimal::ZERO,
        }
    }
}
