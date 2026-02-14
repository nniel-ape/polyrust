use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A range specification for Decimal parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ParamRange {
    /// Explicit list of values: [0.90, 0.92, 0.95]
    Values(Vec<Decimal>),
    /// Min/max/step range: { min = 0.01, max = 0.03, step = 0.01 }
    Range {
        min: Decimal,
        max: Decimal,
        step: Decimal,
    },
}

impl ParamRange {
    /// Expand the range into a list of concrete values.
    pub fn expand(&self) -> Vec<Decimal> {
        match self {
            ParamRange::Values(v) => v.clone(),
            ParamRange::Range { min, max, step } => {
                if *step <= Decimal::ZERO || *min > *max {
                    return vec![*min];
                }
                let mut values = Vec::new();
                let mut current = *min;
                while current <= *max {
                    values.push(current);
                    current += step;
                }
                values
            }
        }
    }
}

/// A range specification for u64 parameters (time thresholds, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IntParamRange {
    /// Explicit list: [3, 5, 10]
    Values(Vec<u64>),
    /// Min/max/step range: { min = 1, max = 10, step = 1 }
    Range { min: u64, max: u64, step: u64 },
}

impl IntParamRange {
    /// Expand to concrete values.
    pub fn expand(&self) -> Vec<u64> {
        match self {
            IntParamRange::Values(v) => v.clone(),
            IntParamRange::Range { min, max, step } => {
                if *step == 0 || *min > *max {
                    return vec![*min];
                }
                let mut values = Vec::new();
                let mut current = *min;
                while current <= *max {
                    values.push(current);
                    current += step;
                }
                values
            }
        }
    }
}

/// Per-bucket dynamic threshold sweep.
/// Maps time_bucket (secs as string key) to ask threshold ranges.
/// TOML keys are always strings, so we use String keys and parse to u64 at grid generation.
pub type DynamicThresholdsSweep = BTreeMap<String, ParamRange>;

/// Sweep parameters for TailEnd strategy fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TailEndSweepParams {
    pub max_spread_bps: Option<ParamRange>,
    pub min_sustained_secs: Option<IntParamRange>,
    pub max_recent_volatility: Option<ParamRange>,
    #[serde(alias = "fok_cooldown_secs")]
    pub rejection_cooldown_secs: Option<IntParamRange>,
    pub stale_ob_secs: Option<IntParamRange>,
    /// Per-bucket dynamic threshold sweep.
    /// Keys are time bucket seconds (e.g., 120, 90, 60, 30).
    /// Values are threshold ranges for each bucket.
    /// time_threshold_secs is auto-derived as max bucket time.
    pub dynamic_thresholds: Option<DynamicThresholdsSweep>,
    /// Post-entry exit drop threshold sweep.
    pub post_entry_exit_drop: Option<ParamRange>,
    /// Post-entry window seconds sweep.
    pub post_entry_window_secs: Option<IntParamRange>,
    /// Min strike distance (crypto % from strike) sweep.
    pub min_strike_distance_pct: Option<ParamRange>,
    /// Seconds after entry before any sell is allowed.
    pub min_sell_delay_secs: Option<IntParamRange>,
}

/// Sweep parameters for sizing config fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SizingSweepParams {
    /// base_size omitted — it's a linear scaler that doesn't affect % metrics.
    pub kelly_multiplier: Option<ParamRange>,
    pub min_size: Option<ParamRange>,
    pub max_size: Option<ParamRange>,
    /// Depth cap factor: fraction of visible orderbook depth to cap order size.
    pub depth_cap_factor: Option<ParamRange>,
}

/// Sweep parameters for stop-loss config fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StopLossSweepParams {
    pub reversal_pct: Option<ParamRange>,
    pub min_drop: Option<ParamRange>,
    pub trailing_distance: Option<ParamRange>,
    /// Minimum seconds remaining for stop-loss to be active.
    pub min_remaining_secs: Option<IntParamRange>,
    /// Hard crash absolute bid drop threshold.
    pub hard_drop_abs: Option<ParamRange>,
    /// Hard crash external price reversal threshold.
    pub hard_reversal_pct: Option<ParamRange>,
    /// Consecutive ticks dual-trigger must hold (hysteresis).
    pub dual_trigger_consecutive_ticks: Option<IntParamRange>,
    /// Distance from entry price to arm the trailing stop.
    pub trailing_arm_distance: Option<ParamRange>,
    /// Floor on effective trailing distance near expiry.
    pub trailing_min_distance: Option<ParamRange>,
    /// Max combined cost for opposite-side set completion.
    pub recovery_max_set_cost: Option<ParamRange>,
    /// Cooldown seconds after recovery before re-entry.
    pub reentry_cooldown_secs: Option<IntParamRange>,
}

/// Top-level sweep configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SweepConfig {
    /// Max concurrent backtest runs (default: num_cpus).
    pub parallelism: Option<usize>,
    /// Base directory for sweep output (default: "sweep_results").
    /// Each run creates a timestamped subfolder with results.csv, results.json,
    /// sensitivity.csv, and sensitivity.json.
    pub output_dir: Option<String>,
    /// Metric to rank by: "sharpe" (default), "pnl", "win_rate", "drawdown".
    pub rank_by: Option<String>,
    /// Number of top results to display (default: 20).
    pub top_n: Option<usize>,
    /// Force run even with >5000 combinations.
    pub force: Option<bool>,

    #[serde(default)]
    pub tailend: TailEndSweepParams,
    #[serde(default)]
    pub sizing: SizingSweepParams,
    #[serde(default)]
    pub stop_loss: StopLossSweepParams,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn param_range_expand_values() {
        let range = ParamRange::Values(vec![dec!(0.90), dec!(0.92), dec!(0.95)]);
        assert_eq!(range.expand(), vec![dec!(0.90), dec!(0.92), dec!(0.95)]);
    }

    #[test]
    fn param_range_expand_range() {
        let range = ParamRange::Range {
            min: dec!(0.01),
            max: dec!(0.03),
            step: dec!(0.01),
        };
        assert_eq!(range.expand(), vec![dec!(0.01), dec!(0.02), dec!(0.03)]);
    }

    #[test]
    fn param_range_expand_range_non_exact() {
        let range = ParamRange::Range {
            min: dec!(0.01),
            max: dec!(0.025),
            step: dec!(0.01),
        };
        assert_eq!(range.expand(), vec![dec!(0.01), dec!(0.02)]);
    }

    #[test]
    fn int_param_range_expand() {
        let range = IntParamRange::Values(vec![3, 5, 10]);
        assert_eq!(range.expand(), vec![3, 5, 10]);

        let range = IntParamRange::Range {
            min: 1,
            max: 5,
            step: 2,
        };
        assert_eq!(range.expand(), vec![1, 3, 5]);
    }

    #[test]
    fn sweep_config_toml_parsing() {
        let toml = r#"
            parallelism = 4
            output_dir = "sweep_results"
            rank_by = "sharpe"
            top_n = 20

            [tailend]
            max_spread_bps = [100, 200, 300]
            min_sustained_secs = [3, 5, 10]
            max_recent_volatility = { min = "0.01", max = "0.03", step = "0.01" }

            [tailend.dynamic_thresholds]
            120 = ["0.88", "0.90", "0.92"]
            90 = ["0.90", "0.92", "0.94"]
            60 = ["0.91", "0.93", "0.95"]
            30 = ["0.93", "0.95", "0.97"]

            [sizing]
            kelly_multiplier = ["0.15", "0.25", "0.35"]

            [stop_loss]
            reversal_pct = ["0.003", "0.005", "0.008"]
        "#;
        let config: SweepConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.parallelism, Some(4));
        assert_eq!(config.output_dir, Some("sweep_results".to_string()));
        assert!(config.tailend.dynamic_thresholds.is_some());
        let dt = config.tailend.dynamic_thresholds.unwrap();
        assert_eq!(dt.len(), 4);
        assert!(dt.contains_key("120"));
        assert!(dt.contains_key("30"));
    }
}
