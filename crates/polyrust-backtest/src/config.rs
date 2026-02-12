use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::sweep::config::SweepConfig;

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
    /// Price history granularity in seconds (e.g., 60 = 1min, 300 = 5min)
    #[serde(default = "default_fidelity")]
    pub data_fidelity_secs: u64,
    /// Path to persistent historical data cache
    #[serde(default = "default_data_db_path")]
    pub data_db_path: String,
    /// Fee model configuration
    #[serde(default)]
    pub fees: FeeConfig,
    /// Optional: Filter markets by exact duration (in seconds)
    /// Example: 900 for 15-minute markets, 3600 for 1-hour markets
    #[serde(default)]
    pub market_duration_secs: Option<u64>,
    /// Number of markets to fetch concurrently (default: 10).
    #[serde(default = "default_fetch_concurrency")]
    pub fetch_concurrency: usize,
    /// Offline mode: skip all network fetches, use only cached data from backtest_data.db.
    #[serde(default)]
    pub offline: bool,
    /// Realism settings for more accurate simulation (slippage, depth, fees).
    #[serde(default)]
    pub realism: RealismConfig,
    /// Optional parameter sweep configuration for grid search.
    #[serde(default)]
    pub sweep: Option<SweepConfig>,
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

/// Realism configuration for more accurate backtest simulation.
///
/// These settings model real-world frictions that the default "immediate fill"
/// engine ignores: slippage, finite orderbook depth, and GTC-as-taker fees.
/// All defaults are conservative (zero friction) so existing backtests are unaffected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RealismConfig {
    /// Fixed slippage penalty in ticks (0.01 each) added to BUY fills and subtracted
    /// from SELL fills. Simulates the cost of walking the orderbook.
    /// Example: 1 = 1 tick = 0.01 price penalty per fill.
    pub slippage_ticks: u32,

    /// Typical orderbook depth (shares) at best bid/ask. Replaces the hardcoded 1000.
    /// Real Polymarket 15-min markets have ~50-200 shares near expiration.
    pub typical_depth: Decimal,

    /// Whether depth should decay linearly as market approaches expiration.
    /// When true: effective_depth = typical_depth * max(0.2, time_remaining / market_duration).
    /// When false: depth is always `typical_depth`.
    pub depth_decay_near_expiry: bool,

    /// Charge taker fee on GTC orders that would match immediately in live
    /// (i.e., GTC BUY at price >= best_ask, GTC SELL at price <= best_bid).
    pub gtc_taker_fee_heuristic: bool,
}

impl Default for RealismConfig {
    fn default() -> Self {
        Self {
            slippage_ticks: 0,
            typical_depth: Decimal::new(1000, 0),
            depth_decay_near_expiry: false,
            gtc_taker_fee_heuristic: false,
        }
    }
}

fn default_fidelity() -> u64 {
    60
}

fn default_data_db_path() -> String {
    "backtest_data.db".to_string()
}

fn default_fetch_concurrency() -> usize {
    10
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            strategy_name: String::new(),
            market_ids: Vec::new(),
            start_date: Utc::now(),
            end_date: Utc::now(),
            initial_balance: Decimal::ZERO,
            data_fidelity_secs: 60,
            data_db_path: "backtest_data.db".to_string(),
            fees: FeeConfig::default(),
            market_duration_secs: None,

            fetch_concurrency: 10,
            offline: false,
            realism: RealismConfig::default(),
            sweep: None,
        }
    }
}

impl BacktestConfig {
    /// Apply POLY_BACKTEST_* environment variable overrides.
    pub fn with_env_overrides(mut self) -> Result<Self, polyrust_core::error::PolyError> {
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
        if let Ok(v) = std::env::var("POLY_BACKTEST_FIDELITY_SECS")
            && let Ok(fid) = v.parse::<u64>()
        {
            self.data_fidelity_secs = fid;
        }
        if let Ok(v) = std::env::var("POLY_BACKTEST_MARKET_IDS") {
            self.market_ids = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Ok(v) = std::env::var("POLY_BACKTEST_MARKET_DURATION_SECS")
            && let Ok(dur) = v.parse::<u64>()
        {
            self.market_duration_secs = Some(dur);
        }
        if let Ok(v) = std::env::var("POLY_BACKTEST_FETCH_CONCURRENCY")
            && let Ok(n) = v.parse::<usize>()
        {
            self.fetch_concurrency = n;
        }
        if let Ok(v) = std::env::var("POLY_BACKTEST_OFFLINE") {
            let lower = v.trim().to_lowercase();
            self.offline = matches!(lower.as_str(), "1" | "true" | "yes");
        }
        // Validate fetch_concurrency
        if self.fetch_concurrency == 0 {
            return Err(polyrust_core::error::PolyError::Config(
                "fetch_concurrency must be > 0".into(),
            ));
        }
        // Validate fidelity
        if self.data_fidelity_secs == 0 {
            return Err(polyrust_core::error::PolyError::Config(
                "data_fidelity_secs must be > 0".into(),
            ));
        }
        if self.data_fidelity_secs > 86400 {
            return Err(polyrust_core::error::PolyError::Config(
                "data_fidelity_secs must be <= 86400 (24 hours)".into(),
            ));
        }
        if self.data_fidelity_secs < 60 {
            tracing::info!(
                "Sub-minute granularity ({} seconds): synthesizing PriceChange events from trade data",
                self.data_fidelity_secs
            );
        }

        // Validate date range
        if self.start_date >= self.end_date {
            return Err(polyrust_core::error::PolyError::Config(format!(
                "Invalid backtest date range: start_date ({}) must be before end_date ({})",
                self.start_date, self.end_date
            )));
        }

        Ok(self)
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
        assert_eq!(config.data_fidelity_secs, 60);
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
            data_fidelity_secs: 300,
            data_db_path: "custom_backtest_data.db".to_string(),
            fees: FeeConfig {
                taker_fee_rate: dec!(0.02),
            },
            market_duration_secs: None,

            fetch_concurrency: 10,
            offline: false,
            realism: RealismConfig::default(),
            sweep: None,
        };
        assert_eq!(config.strategy_name, "test-strategy");
        assert_eq!(config.market_ids.len(), 2);
        assert_eq!(config.initial_balance, dec!(1000.00));
        assert_eq!(config.data_fidelity_secs, 300);
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
            data_fidelity_secs = 300
            data_db_path = "test_backtest.db"

            [fees]
            taker_fee_rate = "0.0250"
        "#;
        let config: BacktestConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.strategy_name, "crypto-arb");
        assert_eq!(config.data_fidelity_secs, 300);
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
            std::env::remove_var("POLY_BACKTEST_FIDELITY_SECS");
            std::env::remove_var("POLY_BACKTEST_MARKET_IDS");
            std::env::remove_var("POLY_BACKTEST_START");
            std::env::remove_var("POLY_BACKTEST_END");

            std::env::set_var("POLY_BACKTEST_STRATEGY", "env-strategy");
            std::env::set_var("POLY_BACKTEST_INITIAL_BALANCE", "5000");
            std::env::set_var("POLY_BACKTEST_DATA_DB_PATH", "env_backtest.db");
            std::env::set_var("POLY_BACKTEST_FIDELITY_SECS", "10");
            std::env::set_var("POLY_BACKTEST_MARKET_IDS", "market1,market2,market3");
            std::env::set_var("POLY_BACKTEST_START", "2025-01-01T00:00:00Z");
            std::env::set_var("POLY_BACKTEST_END", "2025-02-01T00:00:00Z");
        }

        let config = BacktestConfig::default().with_env_overrides().unwrap();
        assert_eq!(config.strategy_name, "env-strategy");
        assert_eq!(config.initial_balance, dec!(5000));
        assert_eq!(config.data_db_path, "env_backtest.db");
        assert_eq!(config.data_fidelity_secs, 10);
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
            std::env::remove_var("POLY_BACKTEST_FIDELITY_SECS");
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
            std::env::remove_var("POLY_BACKTEST_FIDELITY_SECS");
            std::env::remove_var("POLY_BACKTEST_MARKET_IDS");
            std::env::remove_var("POLY_BACKTEST_START");
            std::env::remove_var("POLY_BACKTEST_END");

            std::env::set_var("POLY_BACKTEST_STRATEGY", "partial-env");
        }
        let mut config = BacktestConfig::default();
        config.initial_balance = dec!(2000);
        // Set valid date range (default has start_date = end_date = now)
        config.start_date = Utc::now() - chrono::Duration::days(7);
        config.end_date = Utc::now();
        config = config.with_env_overrides().unwrap();
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
        assert_eq!(config.data_fidelity_secs, 60); // default
        assert_eq!(config.data_db_path, "backtest_data.db"); // default
        assert_eq!(config.fees.taker_fee_rate, dec!(0.0315)); // default
        assert!(config.market_ids.is_empty()); // default
    }
}
