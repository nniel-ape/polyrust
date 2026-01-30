use rust_decimal::Decimal;

/// Backtest results and metrics
pub struct BacktestReport {
    pub total_pnl: Decimal,
}

impl BacktestReport {
    pub fn new() -> Self {
        Self {
            total_pnl: Decimal::ZERO,
        }
    }
}

impl Default for BacktestReport {
    fn default() -> Self {
        Self::new()
    }
}
