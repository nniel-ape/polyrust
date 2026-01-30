//! Configuration structs for the crypto arbitrage strategies.
//!
//! Each trading mode (TailEnd, TwoSided, Confirmed, CrossCorrelated) has its own
//! configuration struct with an `enabled` flag. All modes are disabled by default
//! and must be explicitly enabled in config.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Per-Mode Configuration Structs
// ---------------------------------------------------------------------------

/// TailEnd mode configuration.
///
/// Entry conditions:
/// - Time remaining < `time_threshold_secs` (default 120s)
/// - Predicted winner's ask >= `ask_threshold` (default 0.90)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TailEndConfig {
    /// Enable TailEnd trading mode. Default: false.
    pub enabled: bool,
    /// Maximum seconds remaining to enter (default 120).
    pub time_threshold_secs: u64,
    /// Minimum ask price to enter (default 0.90).
    pub ask_threshold: Decimal,
}

impl Default for TailEndConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            time_threshold_secs: 120,
            ask_threshold: Decimal::new(90, 2), // 0.90
        }
    }
}

/// TwoSided mode configuration.
///
/// Entry conditions:
/// - Combined ask prices < `combined_threshold` (default 0.98)
/// - Risk-free arbitrage when both outcomes are mispriced
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TwoSidedConfig {
    /// Enable TwoSided trading mode. Default: false.
    pub enabled: bool,
    /// Maximum combined ask price for both outcomes (default 0.98).
    pub combined_threshold: Decimal,
}

impl Default for TwoSidedConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            combined_threshold: Decimal::new(98, 2), // 0.98
        }
    }
}

/// Confirmed mode configuration.
///
/// Entry conditions:
/// - Dynamic confidence model based on price movement
/// - Net profit margin >= `min_margin` after fees
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfirmedConfig {
    /// Enable Confirmed trading mode. Default: false.
    pub enabled: bool,
    /// Minimum confidence level to enter (default 0.50).
    pub min_confidence: Decimal,
    /// Minimum net profit margin after fees (default 0.02 = 2%).
    pub min_margin: Decimal,
}

impl Default for ConfirmedConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_confidence: Decimal::new(50, 2), // 0.50
            min_margin: Decimal::new(2, 2),      // 0.02
        }
    }
}

// ---------------------------------------------------------------------------
// Shared Configuration Structs
// ---------------------------------------------------------------------------

/// Fee model configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeeConfig {
    /// Taker fee rate (default 0.0315 = 3.15% at 50/50).
    pub taker_fee_rate: Decimal,
}

impl Default for FeeConfig {
    fn default() -> Self {
        Self {
            taker_fee_rate: Decimal::new(315, 4), // 0.0315
        }
    }
}

/// Spike detection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpikeConfig {
    /// Minimum price change percentage to count as a spike.
    pub threshold_pct: Decimal,
    /// Lookback window in seconds for spike detection.
    pub window_secs: u64,
    /// Maximum number of spike events to retain.
    pub history_size: usize,
}

impl Default for SpikeConfig {
    fn default() -> Self {
        Self {
            threshold_pct: Decimal::new(5, 3), // 0.005
            window_secs: 10,
            history_size: 50,
        }
    }
}

/// Hybrid order mode configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OrderConfig {
    /// Use GTC maker orders for Confirmed/TwoSided modes.
    pub hybrid_mode: bool,
    /// Price offset below best ask for GTC limit orders.
    pub limit_offset: Decimal,
    /// Cancel stale GTC orders after this many seconds.
    pub max_age_secs: u64,
}

impl Default for OrderConfig {
    fn default() -> Self {
        Self {
            hybrid_mode: true,
            limit_offset: Decimal::new(1, 2), // 0.01
            max_age_secs: 30,
        }
    }
}

/// Position sizing configuration (Kelly criterion).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SizingConfig {
    /// Base position size in USDC.
    pub base_size: Decimal,
    /// Kelly fraction multiplier (fractional Kelly).
    pub kelly_multiplier: Decimal,
    /// Minimum position size in USDC.
    pub min_size: Decimal,
    /// Maximum position size in USDC.
    pub max_size: Decimal,
    /// Whether to use Kelly sizing (vs fixed).
    pub use_kelly: bool,
}

impl Default for SizingConfig {
    fn default() -> Self {
        Self {
            base_size: Decimal::new(10, 0),
            kelly_multiplier: Decimal::new(25, 2), // 0.25
            min_size: Decimal::new(2, 0),
            max_size: Decimal::new(25, 0),
            use_kelly: true,
        }
    }
}

/// Stop-loss configuration (dual-trigger + trailing).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StopLossConfig {
    /// Crypto price reversal percentage trigger (e.g. 0.005 = 0.5%).
    pub reversal_pct: Decimal,
    /// Minimum market price drop to confirm stop-loss (e.g. 0.05 = 5¢).
    pub min_drop: Decimal,
    /// Enable trailing stop-loss.
    pub trailing_enabled: bool,
    /// Trailing stop distance from peak bid.
    pub trailing_distance: Decimal,
    /// Tighten trailing distance as time remaining decreases.
    pub time_decay: bool,
}

impl Default for StopLossConfig {
    fn default() -> Self {
        Self {
            reversal_pct: Decimal::new(5, 3),       // 0.005
            min_drop: Decimal::new(5, 2),           // 0.05
            trailing_enabled: true,
            trailing_distance: Decimal::new(3, 2), // 0.03
            time_decay: true,
        }
    }
}

/// Cross-market correlation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CorrelationConfig {
    /// Enable cross-market correlation signals.
    pub enabled: bool,
    /// Minimum spike percentage in leader coin to trigger follower signals.
    pub min_spike_pct: Decimal,
    /// Leader → follower coin pairs (e.g. BTC → [ETH, SOL]).
    pub pairs: Vec<(String, Vec<String>)>,
    /// Confidence discount factor for correlation signals (default 0.7).
    pub discount_factor: Decimal,
}

impl Default for CorrelationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_spike_pct: Decimal::new(1, 2), // 0.01
            pairs: vec![
                ("BTC".into(), vec!["ETH".into(), "SOL".into()]),
                ("ETH".into(), vec!["SOL".into()]),
            ],
            discount_factor: Decimal::new(7, 1), // 0.7
        }
    }
}

/// Performance tracking and auto-disable configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PerformanceConfig {
    /// Minimum trades before auto-disable can trigger.
    pub min_trades: u64,
    /// Minimum win rate to keep a mode enabled.
    pub min_win_rate: Decimal,
    /// Rolling window size for recent P&L tracking.
    pub window_size: usize,
    /// Automatically disable modes with poor performance.
    pub auto_disable: bool,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            min_trades: 20,
            min_win_rate: Decimal::new(40, 2), // 0.40
            window_size: 50,
            auto_disable: false,
        }
    }
}

/// Configuration for the crypto arbitrage strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ArbitrageConfig {
    /// Coins to track (e.g. ["BTC", "ETH", "SOL", "XRP"])
    pub coins: Vec<String>,
    /// Maximum concurrent positions
    pub max_positions: usize,
    /// Minimum profit margin for confirmed mode
    pub min_profit_margin: Decimal,
    /// Minimum profit margin in late window (120-300s)
    pub late_window_margin: Decimal,
    /// Interval in seconds between market discovery scans
    pub scan_interval_secs: u64,
    /// Whether to use on-chain Chainlink RPC for resolution reference price
    pub use_chainlink: bool,

    // -------------------------------------------------------------------------
    // Per-mode configurations (each mode disabled by default)
    // -------------------------------------------------------------------------
    /// TailEnd mode configuration.
    #[serde(default)]
    pub tailend: TailEndConfig,
    /// TwoSided mode configuration.
    #[serde(default)]
    pub twosided: TwoSidedConfig,
    /// Confirmed mode configuration.
    #[serde(default)]
    pub confirmed: ConfirmedConfig,
    /// Cross-market correlation configuration.
    #[serde(default)]
    pub correlation: CorrelationConfig,

    // -------------------------------------------------------------------------
    // Shared configurations
    // -------------------------------------------------------------------------
    /// Fee model configuration.
    #[serde(default)]
    pub fee: FeeConfig,
    /// Spike detection configuration.
    #[serde(default)]
    pub spike: SpikeConfig,
    /// Hybrid order mode configuration.
    #[serde(default)]
    pub order: OrderConfig,
    /// Position sizing configuration.
    #[serde(default)]
    pub sizing: SizingConfig,
    /// Stop-loss configuration.
    #[serde(default)]
    pub stop_loss: StopLossConfig,
    /// Performance tracking configuration.
    #[serde(default)]
    pub performance: PerformanceConfig,
}

impl Default for ArbitrageConfig {
    fn default() -> Self {
        Self {
            coins: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            max_positions: 5,
            min_profit_margin: Decimal::new(3, 2),  // 0.03
            late_window_margin: Decimal::new(2, 2), // 0.02
            scan_interval_secs: 30,
            use_chainlink: true,
            // Per-mode configs (all disabled by default)
            tailend: TailEndConfig::default(),
            twosided: TwoSidedConfig::default(),
            confirmed: ConfirmedConfig::default(),
            correlation: CorrelationConfig::default(),
            // Shared configs
            fee: FeeConfig::default(),
            spike: SpikeConfig::default(),
            order: OrderConfig::default(),
            sizing: SizingConfig::default(),
            stop_loss: StopLossConfig::default(),
            performance: PerformanceConfig::default(),
        }
    }
}

impl ArbitrageConfig {
    /// Returns true if at least one trading mode is enabled.
    pub fn any_mode_enabled(&self) -> bool {
        self.tailend.enabled
            || self.twosided.enabled
            || self.confirmed.enabled
            || self.correlation.enabled
    }
}
