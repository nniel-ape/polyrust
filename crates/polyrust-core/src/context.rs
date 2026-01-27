use crate::types::*;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Thread-safe shared state accessible by all strategies
#[derive(Debug, Clone)]
pub struct StrategyContext {
    pub positions: Arc<RwLock<PositionState>>,
    pub market_data: Arc<RwLock<MarketDataState>>,
    pub balance: Arc<RwLock<BalanceState>>,
    pub strategy_count: Arc<AtomicUsize>,
}

impl StrategyContext {
    pub fn new() -> Self {
        Self {
            positions: Arc::new(RwLock::new(PositionState::default())),
            market_data: Arc::new(RwLock::new(MarketDataState::default())),
            balance: Arc::new(RwLock::new(BalanceState::default())),
            strategy_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Default for StrategyContext {
    fn default() -> Self {
        Self::new()
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
        self.open_positions.values().map(|p| p.unrealized_pnl()).sum()
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
