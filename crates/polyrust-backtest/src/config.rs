use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestConfig {
    /// Strategy name to backtest
    pub strategy_name: String,
    /// Market IDs to include (empty = auto-discover)
    pub market_ids: Vec<String>,
    /// Backtest window start
    pub start_date: DateTime<Utc>,
    /// Backtest window end
    pub end_date: DateTime<Utc>,
    /// Initial USDC balance
    pub initial_balance: Decimal,
    /// Price history granularity in minutes
    #[serde(default = "default_fidelity")]
    pub data_fidelity_mins: u64,
    /// Path to persistent historical data cache
    #[serde(default = "default_data_db_path")]
    pub data_db_path: String,
}

fn default_fidelity() -> u64 {
    1
}

fn default_data_db_path() -> String {
    "backtest_data.db".to_string()
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            strategy_name: String::new(),
            market_ids: Vec::new(),
            start_date: Utc::now(),
            end_date: Utc::now(),
            initial_balance: Decimal::ZERO,
            data_fidelity_mins: 1,
            data_db_path: "backtest_data.db".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_backtest_config_defaults() {
        let config = BacktestConfig::default();
        assert_eq!(config.strategy_name, "");
        assert_eq!(config.market_ids.len(), 0);
        assert_eq!(config.initial_balance, Decimal::ZERO);
        assert_eq!(config.data_fidelity_mins, 1);
        assert_eq!(config.data_db_path, "backtest_data.db");
    }

    #[test]
    fn test_backtest_config_custom_values() {
        let start = Utc::now();
        let end = Utc::now();
        let config = BacktestConfig {
            strategy_name: "test-strategy".to_string(),
            market_ids: vec!["market1".to_string(), "market2".to_string()],
            start_date: start,
            end_date: end,
            initial_balance: dec!(1000.00),
            data_fidelity_mins: 5,
            data_db_path: "custom_backtest_data.db".to_string(),
        };
        assert_eq!(config.strategy_name, "test-strategy");
        assert_eq!(config.market_ids.len(), 2);
        assert_eq!(config.initial_balance, dec!(1000.00));
        assert_eq!(config.data_fidelity_mins, 5);
        assert_eq!(config.data_db_path, "custom_backtest_data.db");
    }
}
