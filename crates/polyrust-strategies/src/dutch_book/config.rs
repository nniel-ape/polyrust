use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Configuration for the Dutch Book arbitrage strategy.
///
/// Dutch Book arbitrage buys both YES and NO tokens when their combined ask
/// price is below $1.00, locking in a guaranteed profit upon market resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DutchBookConfig {
    /// Whether the strategy is enabled (default: false)
    pub enabled: bool,

    /// Maximum combined cost of YES + NO tokens to consider profitable (default: 0.99)
    /// Must be < 1.0 to leave room for fees and guarantee profit.
    pub max_combined_cost: Decimal,

    /// Minimum profit percentage threshold (default: 0.005 = 0.5%)
    /// profit_pct = (1.0 - combined_cost) / combined_cost
    pub min_profit_threshold: Decimal,

    /// Maximum USDC size per side of a paired order (default: 100)
    pub max_position_size: Decimal,

    /// Minimum market liquidity in USD to consider (default: 10000)
    pub min_liquidity_usd: Decimal,

    /// Maximum days until market resolution to consider (default: 7)
    pub max_days_until_resolution: u64,

    /// How often to scan Gamma API for new markets, in seconds (default: 600 = 10 min)
    pub scan_interval_secs: u64,

    /// Maximum number of concurrent paired positions (default: 10)
    pub max_concurrent_positions: usize,

    /// Discount applied when emergency-unwinding a partial fill (default: 0.03 = sell at 97%)
    pub unwind_discount: Decimal,
}

impl Default for DutchBookConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_combined_cost: Decimal::new(99, 2),       // 0.99
            min_profit_threshold: Decimal::new(5, 3),     // 0.005
            max_position_size: Decimal::new(100, 0),      // 100 USDC
            min_liquidity_usd: Decimal::new(10000, 0),    // 10,000 USD
            max_days_until_resolution: 7,
            scan_interval_secs: 600,
            max_concurrent_positions: 10,
            unwind_discount: Decimal::new(3, 2),          // 0.03
        }
    }
}

impl DutchBookConfig {
    /// Validate configuration values. Returns `Err` with a description on invalid config.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_combined_cost <= Decimal::ZERO || self.max_combined_cost >= Decimal::ONE {
            return Err(format!(
                "max_combined_cost must be in (0, 1), got {}",
                self.max_combined_cost
            ));
        }

        if self.min_profit_threshold <= Decimal::ZERO {
            return Err(format!(
                "min_profit_threshold must be positive, got {}",
                self.min_profit_threshold
            ));
        }

        if self.max_position_size <= Decimal::ZERO {
            return Err(format!(
                "max_position_size must be positive, got {}",
                self.max_position_size
            ));
        }

        if self.min_liquidity_usd < Decimal::ZERO {
            return Err(format!(
                "min_liquidity_usd must be non-negative, got {}",
                self.min_liquidity_usd
            ));
        }

        if self.max_days_until_resolution == 0 {
            return Err("max_days_until_resolution must be > 0".to_string());
        }

        if self.scan_interval_secs == 0 {
            return Err("scan_interval_secs must be > 0".to_string());
        }

        if self.max_concurrent_positions == 0 {
            return Err("max_concurrent_positions must be > 0".to_string());
        }

        if self.unwind_discount <= Decimal::ZERO || self.unwind_discount >= Decimal::ONE {
            return Err(format!(
                "unwind_discount must be in (0, 1), got {}",
                self.unwind_discount
            ));
        }

        Ok(())
    }
}
