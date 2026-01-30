use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestConfig {
    /// Strategy name to backtest
    pub strategy_name: String,
    /// Market IDs to include (empty = auto-discover)
    #[serde(default)]
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
    /// Fee model configuration
    #[serde(default)]
    pub fees: FeeConfig,
}

/// Fee model configuration for backtesting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeeConfig {
    /// Taker fee rate (default 0.0315 = 3.15% at 50/50).
    pub taker_fee_rate: Decimal,
}

impl Default for FeeConfig {
    fn default() -> Self {
        Self {
            taker_fee_rate: Decimal::new(315, 4), // 0.0315 = 3.15%
        }
    }
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
            fees: FeeConfig::default(),
        }
    }
}

impl BacktestConfig {
    /// Apply POLY_BACKTEST_* environment variable overrides.
    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(v) = std::env::var("POLY_BACKTEST_START")
            && let Ok(dt) = DateTime::parse_from_rfc3339(&v)
        {
            self.start_date = dt.with_timezone(&Utc);
        }
        if let Ok(v) = std::env::var("POLY_BACKTEST_END")
            && let Ok(dt) = DateTime::parse_from_rfc3339(&v)
        {
            self.end_date = dt.with_timezone(&Utc);
        }
        if let Ok(v) = std::env::var("POLY_BACKTEST_INITIAL_BALANCE")
            && let Ok(bal) = v.parse::<Decimal>()
        {
            self.initial_balance = bal;
        }
        if let Ok(v) = std::env::var("POLY_BACKTEST_STRATEGY") {
            self.strategy_name = v;
        }
        if let Ok(v) = std::env::var("POLY_BACKTEST_DATA_DB_PATH") {
            self.data_db_path = v;
        }
        if let Ok(v) = std::env::var("POLY_BACKTEST_FIDELITY_MINS")
            && let Ok(fid) = v.parse::<u64>()
        {
            self.data_fidelity_mins = fid;
        }
        if let Ok(v) = std::env::var("POLY_BACKTEST_MARKET_IDS") {
            self.market_ids = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        self
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
        assert_eq!(config.fees.taker_fee_rate, dec!(0.0315));
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
            fees: FeeConfig {
                taker_fee_rate: dec!(0.02),
            },
        };
        assert_eq!(config.strategy_name, "test-strategy");
        assert_eq!(config.market_ids.len(), 2);
        assert_eq!(config.initial_balance, dec!(1000.00));
        assert_eq!(config.data_fidelity_mins, 5);
        assert_eq!(config.data_db_path, "custom_backtest_data.db");
        assert_eq!(config.fees.taker_fee_rate, dec!(0.02));
    }

    #[test]
    fn test_backtest_config_toml_parsing() {
        let toml = r#"
            strategy_name = "crypto-arb"
            start_date = "2025-01-01T00:00:00Z"
            end_date = "2025-01-31T23:59:59Z"
            initial_balance = "1000.00"
            data_fidelity_mins = 5
            data_db_path = "test_backtest.db"

            [fees]
            taker_fee_rate = "0.0250"
        "#;
        let config: BacktestConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.strategy_name, "crypto-arb");
        assert_eq!(config.data_fidelity_mins, 5);
        assert_eq!(config.initial_balance, dec!(1000.00));
        assert_eq!(config.fees.taker_fee_rate, dec!(0.0250));
    }

    #[test]
    #[serial_test::serial]
    fn test_backtest_config_env_overrides() {
        // Clear any existing env vars from other tests
        unsafe {
            std::env::remove_var("POLY_BACKTEST_STRATEGY");
            std::env::remove_var("POLY_BACKTEST_INITIAL_BALANCE");
            std::env::remove_var("POLY_BACKTEST_DATA_DB_PATH");
            std::env::remove_var("POLY_BACKTEST_FIDELITY_MINS");
            std::env::remove_var("POLY_BACKTEST_MARKET_IDS");
            std::env::remove_var("POLY_BACKTEST_START");
            std::env::remove_var("POLY_BACKTEST_END");

            std::env::set_var("POLY_BACKTEST_STRATEGY", "env-strategy");
            std::env::set_var("POLY_BACKTEST_INITIAL_BALANCE", "5000");
            std::env::set_var("POLY_BACKTEST_DATA_DB_PATH", "env_backtest.db");
            std::env::set_var("POLY_BACKTEST_FIDELITY_MINS", "10");
            std::env::set_var("POLY_BACKTEST_MARKET_IDS", "market1,market2,market3");
            std::env::set_var("POLY_BACKTEST_START", "2025-01-01T00:00:00Z");
            std::env::set_var("POLY_BACKTEST_END", "2025-02-01T00:00:00Z");
        }

        let config = BacktestConfig::default().with_env_overrides();
        assert_eq!(config.strategy_name, "env-strategy");
        assert_eq!(config.initial_balance, dec!(5000));
        assert_eq!(config.data_db_path, "env_backtest.db");
        assert_eq!(config.data_fidelity_mins, 10);
        assert_eq!(config.market_ids, vec!["market1", "market2", "market3"]);
        assert_eq!(
            config.start_date,
            DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );
        assert_eq!(
            config.end_date,
            DateTime::parse_from_rfc3339("2025-02-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        );

        // Clean up env vars
        unsafe {
            std::env::remove_var("POLY_BACKTEST_STRATEGY");
            std::env::remove_var("POLY_BACKTEST_INITIAL_BALANCE");
            std::env::remove_var("POLY_BACKTEST_DATA_DB_PATH");
            std::env::remove_var("POLY_BACKTEST_FIDELITY_MINS");
            std::env::remove_var("POLY_BACKTEST_MARKET_IDS");
            std::env::remove_var("POLY_BACKTEST_START");
            std::env::remove_var("POLY_BACKTEST_END");
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_backtest_config_partial_env_overrides() {
        // Clear all env vars first
        unsafe {
            std::env::remove_var("POLY_BACKTEST_STRATEGY");
            std::env::remove_var("POLY_BACKTEST_INITIAL_BALANCE");
            std::env::remove_var("POLY_BACKTEST_DATA_DB_PATH");
            std::env::remove_var("POLY_BACKTEST_FIDELITY_MINS");
            std::env::remove_var("POLY_BACKTEST_MARKET_IDS");
            std::env::remove_var("POLY_BACKTEST_START");
            std::env::remove_var("POLY_BACKTEST_END");

            std::env::set_var("POLY_BACKTEST_STRATEGY", "partial-env");
        }
        let mut config = BacktestConfig::default();
        config.initial_balance = dec!(2000);
        config = config.with_env_overrides();
        assert_eq!(config.strategy_name, "partial-env");
        assert_eq!(config.initial_balance, dec!(2000)); // Not overridden
        unsafe {
            std::env::remove_var("POLY_BACKTEST_STRATEGY");
        }
    }

    #[test]
    fn test_backtest_config_defaults_with_serde() {
        let toml = r#"
            strategy_name = "minimal"
            start_date = "2025-01-01T00:00:00Z"
            end_date = "2025-01-31T23:59:59Z"
            initial_balance = "1000"
        "#;
        let config: BacktestConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.data_fidelity_mins, 1); // default
        assert_eq!(config.data_db_path, "backtest_data.db"); // default
        assert_eq!(config.fees.taker_fee_rate, dec!(0.0315)); // default
        assert!(config.market_ids.is_empty()); // default
    }
}
