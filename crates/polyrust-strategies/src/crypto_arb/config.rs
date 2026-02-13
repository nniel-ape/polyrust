//! Configuration structs for the crypto arbitrage strategy.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Reference Quality Level
// ---------------------------------------------------------------------------

/// Minimum required reference price quality for tail-end entry.
///
/// Used to filter out trades when the reference price is stale or inaccurate.
/// Ordered from lowest to highest quality (for Ord derivation).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceQualityLevel {
    /// Current price at discovery time (least accurate, fallback).
    #[default]
    Current,
    /// Historical price entry (within 30s of window start).
    Historical,
    /// On-chain Chainlink price lookup.
    OnChain,
    /// Exact boundary snapshot (captured within 2s of window start).
    Exact,
}

// ---------------------------------------------------------------------------
// Per-Mode Configuration Structs
// ---------------------------------------------------------------------------

/// TailEnd mode configuration.
///
/// Entry conditions:
/// - Time remaining < `time_threshold_secs` (default 120s)
/// - Predicted winner's ask >= dynamic threshold based on time remaining
/// - Reference quality >= `min_reference_quality` (default Historical)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TailEndConfig {
    /// Maximum seconds remaining to enter (default 120).
    pub time_threshold_secs: u64,
    /// Minimum ask price to enter (default 0.90).
    /// Deprecated: use dynamic thresholds below instead.
    pub ask_threshold: Decimal,
    /// Minimum required reference quality to enter (default Historical).
    /// Trades with Current quality will be skipped.
    pub min_reference_quality: ReferenceQualityLevel,
    /// Dynamic ask thresholds by time bucket (in seconds remaining).
    /// Higher thresholds as expiration approaches to reduce false positives.
    /// Default: 120s->0.90, 90s->0.92, 60s->0.93, 30s->0.95
    #[serde(default = "default_time_thresholds")]
    pub dynamic_thresholds: Vec<(u64, Decimal)>,
    /// Maximum spread in basis points (1 bp = 0.01%, default 100 = 1%).
    /// Filters out illiquid markets where wide spread masquerades as certainty.
    pub max_spread_bps: Decimal,
    /// Minimum seconds the crypto price must have favored the predicted direction.
    /// Filters out sudden spikes that immediately reverse. Default: 10 seconds.
    /// With ~5s RTDS intervals, 10s captures 2-3 ticks to establish direction.
    pub min_sustained_secs: u64,
    /// Maximum recent volatility (price wick) in last 10 seconds.
    /// Filters out choppy/volatile conditions. Default: 0.01 (1%).
    pub max_recent_volatility: Decimal,
    /// Cooldown in seconds after an order is rejected before re-evaluating
    /// the same market. Prevents retry storms on every price tick. Default: 15.
    #[serde(alias = "fok_cooldown_secs")]
    pub rejection_cooldown_secs: u64,
    /// Minimum number of price ticks required in the sustained window.
    /// Prevents a single tick from satisfying the sustained direction check.
    /// With ~5s RTDS intervals and min_sustained_secs=5, this ensures at least
    /// 2 ticks confirm the direction. Default: 2.
    #[serde(default = "default_min_sustained_ticks")]
    pub min_sustained_ticks: usize,
    /// Maximum age in seconds for an orderbook snapshot to be considered fresh.
    /// Rejects opportunities if the orderbook is older than this.
    /// Docker adds network latency, so 15s is more realistic than 5s. Default: 15.
    pub stale_ob_secs: i64,
    /// Maximum price drop from entry price within post-entry window to trigger exit.
    /// Relative to entry price (e.g., 0.05 = exit if bid drops 5 cents below entry).
    /// Previously hardcoded to absolute 0.85 threshold. Default: 0.05.
    pub post_entry_exit_drop: Decimal,
    /// Window in seconds after entry during which post-entry exit is active.
    /// Default: 10 seconds.
    pub post_entry_window_secs: i64,
    /// Post-only flag for TailEnd GTC buy orders. When true, orders are rejected
    /// if they would match immediately, enforcing maker behavior (0% fee).
    /// Post-only is incompatible with aggressive pricing (above ask) — causes
    /// 100% rejection. Default: false. Taker fee at TailEnd prices (0.90-0.99)
    /// is only 0.06-0.57%.
    pub post_only: bool,
    /// Use composite fair price from multiple sources for entry gating.
    /// When enabled, entries require min_sources fresh price feeds that agree
    /// within max_dispersion_bps. Default: false (opt-in).
    pub use_composite_price: bool,
    /// Maximum staleness in seconds for a price source to be included in composite.
    /// Default: 10.
    pub max_source_stale_secs: i64,
    /// Minimum number of price sources required for composite price.
    /// Default: 2.
    pub min_sources: usize,
    /// Maximum dispersion across sources in basis points.
    /// If sources disagree by more than this, entry is gated. Default: 50.
    pub max_dispersion_bps: Decimal,
    /// Maximum staleness in seconds for required feed sources.
    /// Entries are gated if any required feed is staler than this. Default: 30.
    pub feed_stale_secs: i64,
    /// Minimum seconds after entry before any sell (stop-loss, post-entry exit)
    /// is attempted. The CLOB needs time to settle a buy before conditional
    /// tokens are available for selling. Default: 15.
    pub min_sell_delay_secs: i64,
    /// Minimum distance from strike as percentage of reference price.
    /// Rejects entries when crypto price is too close to strike (high flip risk).
    /// Default: 0.0008 (0.08% ≈ $1.60 at $2000 ETH).
    pub min_strike_distance_pct: Decimal,
}

fn default_min_sustained_ticks() -> usize {
    2
}

fn default_time_thresholds() -> Vec<(u64, Decimal)> {
    vec![
        (120, Decimal::new(90, 2)), // 0.90 at 120s
        (90, Decimal::new(92, 2)),  // 0.92 at 90s
        (60, Decimal::new(93, 2)),  // 0.93 at 60s
        (30, Decimal::new(95, 2)),  // 0.95 at 30s
    ]
}

impl Default for TailEndConfig {
    fn default() -> Self {
        Self {
            time_threshold_secs: 120,
            ask_threshold: Decimal::new(90, 2), // 0.90
            min_reference_quality: ReferenceQualityLevel::Historical, // Default: skip Current quality
            dynamic_thresholds: default_time_thresholds(),
            max_spread_bps: Decimal::new(200, 0), // 200 bps = 2%
            min_sustained_secs: 5,
            min_sustained_ticks: default_min_sustained_ticks(),
            max_recent_volatility: Decimal::new(2, 2), // 0.02 = 2%
            rejection_cooldown_secs: 15,
            stale_ob_secs: 15,
            post_entry_exit_drop: Decimal::new(5, 2), // 0.05 (5 cents below entry)
            post_entry_window_secs: 20,
            post_only: false,
            use_composite_price: false,
            max_source_stale_secs: 10,
            min_sources: 2,
            max_dispersion_bps: Decimal::new(50, 0),
            feed_stale_secs: 30,
            min_sell_delay_secs: 10,
            min_strike_distance_pct: Decimal::new(5, 3), // 0.005 = 0.5%
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

/// Order execution configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OrderConfig {
    /// Cancel stale GTC orders after this many seconds.
    pub max_age_secs: u64,
    /// Number of tick steps above the best ask for TailEnd GTC orders.
    /// Uses the market's actual tick_size for precision. Default: 1.
    pub tick_steps_above_ask: u32,
}

impl Default for OrderConfig {
    fn default() -> Self {
        Self {
            max_age_secs: 30,
            tick_steps_above_ask: 1,
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
    /// Safety factor for orderbook depth capping.
    /// Order size is capped to `available_depth * depth_cap_factor`.
    /// E.g. 0.50 = cap to 50% of visible depth. Default: 0.50.
    pub depth_cap_factor: Decimal,
}

impl Default for SizingConfig {
    fn default() -> Self {
        Self {
            base_size: Decimal::new(10, 0),
            kelly_multiplier: Decimal::new(25, 2), // 0.25
            min_size: Decimal::new(2, 0),
            max_size: Decimal::new(25, 0),
            use_kelly: true,
            depth_cap_factor: Decimal::new(50, 2), // 0.50
        }
    }
}

impl SizingConfig {
    /// Validate sizing configuration values.
    pub fn validate(&self) -> Result<(), String> {
        if self.base_size <= Decimal::ZERO {
            return Err(format!(
                "base_size must be positive, got {}",
                self.base_size
            ));
        }
        if self.min_size <= Decimal::ZERO {
            return Err(format!("min_size must be positive, got {}", self.min_size));
        }
        if self.max_size < self.min_size {
            return Err(format!(
                "max_size ({}) must be >= min_size ({})",
                self.max_size, self.min_size
            ));
        }
        if self.kelly_multiplier <= Decimal::ZERO {
            return Err(format!(
                "kelly_multiplier must be positive, got {}",
                self.kelly_multiplier
            ));
        }
        Ok(())
    }
}

/// Stop-loss configuration (dual-trigger + trailing + lifecycle).
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
    /// Floor on effective trailing distance after time decay.
    /// Prevents noise triggers when time_decay shrinks distance to near-zero.
    pub trailing_min_distance: Decimal,
    /// Cooldown in seconds after a stale position is removed before re-entering
    /// the same market. Prevents immediate re-entry loops.
    pub stale_market_cooldown_secs: u64,
    /// Minimum seconds remaining for stop-loss to be active.
    /// Below this threshold, stop-losses are suppressed to avoid exiting
    /// positions that are about to settle. Default: 45.
    pub min_remaining_secs: i64,
    /// Cooldown schedule for liquidity rejections ("couldn't be fully filled").
    /// Fast retry since the issue is transient market depth.
    /// Default: [1, 5, 15, 30] seconds (indexed by retry count).
    #[serde(default = "default_liquidity_cooldowns")]
    pub liquidity_cooldowns: Vec<u64>,
    /// Cooldown schedule for balance/allowance rejections.
    /// Longer cooldown since underlying issue may need settlement.
    /// Default: [5, 15, 30, 60] seconds (indexed by retry count).
    #[serde(default = "default_balance_cooldowns")]
    pub balance_cooldowns: Vec<u64>,
    /// Enable GTC fallback after FOK liquidity rejection.
    /// After a FOK stop-loss sell is rejected for liquidity, the next attempt
    /// uses a GTC order resting below the current bid. Default: true.
    pub gtc_fallback: bool,
    /// Number of ticks below current bid for GTC stop-loss orders.
    /// Default: 1 (e.g. bid=0.91 → GTC at 0.90).
    pub gtc_fallback_tick_offset: u32,
    /// Maximum age in seconds for a GTC stop-loss order before cancellation.
    /// Stale GTC orders are cancelled and re-evaluated fresh. Default: 10.
    pub gtc_stop_loss_max_age_secs: u64,

    // ── Hard crash detection ────────────────────────────────────────────
    /// Absolute bid drop from entry price to trigger hard crash (default 0.08).
    /// E.g. entry at 0.95, bid drops to 0.87 → trigger.
    pub hard_drop_abs: Decimal,
    /// External price reversal percentage for hard crash (default 0.006 = 0.6%).
    pub hard_reversal_pct: Decimal,
    /// Window in milliseconds for hard crash detection (default 2000ms).
    pub hard_window_ms: i64,

    // ── Freshness gating for stop-loss data ─────────────────────────────
    /// Maximum orderbook age in ms for stop-loss decisions (default 1200ms).
    pub sl_max_book_age_ms: i64,
    /// Maximum external price age in ms for stop-loss decisions (default 1500ms).
    pub sl_max_external_age_ms: i64,
    /// Minimum fresh sources required for composite stop-loss price (default 2).
    pub sl_min_sources: usize,
    /// Maximum dispersion in bps across sources for stop-loss (default 50).
    pub sl_max_dispersion_bps: Decimal,

    // ── Hysteresis ──────────────────────────────────────────────────────
    /// Number of consecutive ticks both dual-trigger conditions must hold (default 2).
    pub dual_trigger_consecutive_ticks: usize,

    // ── Short-lived limit order refresh ─────────────────────────────────
    /// Refresh interval in seconds for short-lived GTC stop-loss orders (default 2).
    pub short_limit_refresh_secs: u64,
    /// Tick offset below bid for short-lived GTC orders (default 1).
    pub short_limit_tick_offset: u32,

    // ── Trailing arming ─────────────────────────────────────────────────
    /// Distance from entry at which trailing stop arms (default 0.015).
    /// Capped by available headroom: `min(trailing_arm_distance, price_cap - entry)`.
    pub trailing_arm_distance: Decimal,

    // ── Execution ladder ────────────────────────────────────────────────
    /// Fraction of bid depth to cap exit clip size (default 0.80).
    /// Exit clip = min(remaining, bid_depth * exit_depth_cap_factor).
    pub exit_depth_cap_factor: Decimal,
    /// Maximum exit order retries before escalating to recovery (default 5).
    pub max_exit_retries: u32,

    // ── Recovery ────────────────────────────────────────────────────────
    /// Enable recovery logic (opposite-side set completion + re-entry). Default: true.
    pub recovery_enabled: bool,
    /// Maximum combined cost for set completion (entry + other side ask). Default: 1.01.
    pub recovery_max_set_cost: Decimal,
    /// Maximum extra risk fraction for recovery alpha trades. Default: 0.15 (15%).
    pub recovery_max_extra_frac: Decimal,
    /// Number of consecutive confirming ticks before re-entry. Default: 2.
    pub reentry_confirm_ticks: usize,
    /// Cooldown in seconds after recovery before re-entry is allowed. Default: 8.
    pub reentry_cooldown_secs: i64,
}

fn default_liquidity_cooldowns() -> Vec<u64> {
    vec![1, 5, 15, 30]
}

fn default_balance_cooldowns() -> Vec<u64> {
    vec![5, 15, 30, 60]
}

impl Default for StopLossConfig {
    fn default() -> Self {
        Self {
            reversal_pct: Decimal::new(3, 3), // 0.003
            min_drop: Decimal::new(5, 2),     // 0.05
            trailing_enabled: true,
            trailing_distance: Decimal::new(5, 2), // 0.05
            time_decay: true,
            trailing_min_distance: Decimal::new(15, 3), // 0.015
            stale_market_cooldown_secs: 120,
            min_remaining_secs: 45, // Suppress near-expiry exit (was 0)
            liquidity_cooldowns: default_liquidity_cooldowns(),
            balance_cooldowns: default_balance_cooldowns(),
            gtc_fallback: true,
            gtc_fallback_tick_offset: 1,
            gtc_stop_loss_max_age_secs: 2,
            // Hard crash
            hard_drop_abs: Decimal::new(8, 2),      // 0.08
            hard_reversal_pct: Decimal::new(6, 3),   // 0.006
            hard_window_ms: 2000,
            // Freshness gating
            sl_max_book_age_ms: 1200,
            sl_max_external_age_ms: 1500,
            sl_min_sources: 2,
            sl_max_dispersion_bps: Decimal::new(50, 0), // 50 bps
            // Hysteresis
            dual_trigger_consecutive_ticks: 2,
            // Short-lived limit
            short_limit_refresh_secs: 2,
            short_limit_tick_offset: 1,
            // Trailing arming
            trailing_arm_distance: Decimal::new(15, 3), // 0.015
            // Execution ladder
            exit_depth_cap_factor: Decimal::new(80, 2), // 0.80
            max_exit_retries: 5,
            // Recovery
            recovery_enabled: true,
            recovery_max_set_cost: Decimal::new(101, 2), // 1.01
            recovery_max_extra_frac: Decimal::new(15, 2), // 0.15
            reentry_confirm_ticks: 2,
            reentry_cooldown_secs: 8,
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

/// Order amount rounding configuration.
///
/// Polymarket API has specific decimal precision requirements:
/// - Maker amount (USDC total): max 2 decimals
/// - Taker amount (shares): max 2 decimals (SDK LOT_SIZE_SCALE = 2)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RoundingConfig {
    /// Maximum decimal places for order size (taker amount).
    /// SDK enforces max 2 decimals (LOT_SIZE_SCALE = 2).
    pub size_decimals: u32,
    /// Maximum decimal places for order price.
    /// Tick size typically determines this (0.01 = 2 decimals).
    pub price_decimals: u32,
}

impl Default for RoundingConfig {
    fn default() -> Self {
        Self {
            size_decimals: 2,  // SDK enforces max 2 (LOT_SIZE_SCALE = 2)
            price_decimals: 2, // Standard tick size
        }
    }
}

/// Configuration for the crypto arbitrage strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ArbitrageConfig {
    /// Enable the arbitrage strategy. Default: false.
    pub enabled: bool,
    /// Coins to track (e.g. ["BTC", "ETH", "SOL", "XRP"])
    pub coins: Vec<String>,
    /// Maximum concurrent positions
    pub max_positions: usize,
    /// Minimum profit margin
    pub min_profit_margin: Decimal,
    /// Minimum profit margin in late window (120-300s)
    pub late_window_margin: Decimal,
    /// Interval in seconds between market discovery scans
    pub scan_interval_secs: u64,
    /// Whether to use on-chain Chainlink RPC for resolution reference price
    pub use_chainlink: bool,

    // -------------------------------------------------------------------------
    // TailEnd configuration
    // -------------------------------------------------------------------------
    /// TailEnd mode configuration.
    #[serde(default)]
    pub tailend: TailEndConfig,

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
    /// Order amount rounding configuration.
    #[serde(default)]
    pub rounding: RoundingConfig,
}

impl Default for ArbitrageConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            coins: vec!["BTC".into(), "ETH".into(), "SOL".into(), "XRP".into()],
            max_positions: 5,
            min_profit_margin: Decimal::new(3, 2),  // 0.03
            late_window_margin: Decimal::new(2, 2), // 0.02
            scan_interval_secs: 30,
            use_chainlink: true,
            tailend: TailEndConfig::default(),
            // Shared configs
            fee: FeeConfig::default(),
            spike: SpikeConfig::default(),
            order: OrderConfig::default(),
            sizing: SizingConfig::default(),
            stop_loss: StopLossConfig::default(),
            performance: PerformanceConfig::default(),
            rounding: RoundingConfig::default(),
        }
    }
}

impl ArbitrageConfig {
    /// Returns true if the strategy is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Apply environment variable overrides to the configuration.
    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(v) = std::env::var("POLY_MIN_PROFIT_MARGIN")
            && let Ok(d) = v.parse::<Decimal>()
        {
            self.min_profit_margin = d;
        }
        if let Ok(v) = std::env::var("POLY_TAILEND_MIN_SUSTAINED_SECS")
            && let Ok(secs) = v.parse::<u64>()
        {
            self.tailend.min_sustained_secs = secs;
        }
        if let Ok(v) = std::env::var("POLY_TAILEND_MIN_SUSTAINED_TICKS")
            && let Ok(ticks) = v.parse::<usize>()
        {
            self.tailend.min_sustained_ticks = ticks;
        }
        if let Ok(v) = std::env::var("POLY_TAILEND_MAX_VOLATILITY")
            && let Ok(d) = v.parse::<Decimal>()
        {
            self.tailend.max_recent_volatility = d;
        }
        if let Ok(v) = std::env::var("POLY_TAILEND_MAX_SPREAD_BPS")
            && let Ok(d) = v.parse::<Decimal>()
        {
            self.tailend.max_spread_bps = d;
        }
        if let Ok(v) = std::env::var("POLY_TAILEND_STALE_OB_SECS")
            && let Ok(secs) = v.parse::<i64>()
        {
            self.tailend.stale_ob_secs = secs;
        }
        self
    }
}
