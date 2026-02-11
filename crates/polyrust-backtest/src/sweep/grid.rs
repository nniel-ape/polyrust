use std::collections::BTreeMap;

use rust_decimal::Decimal;

use polyrust_strategies::ArbitrageConfig;

use super::config::SweepConfig;

/// A single parameter value in a combination.
#[derive(Debug, Clone)]
pub enum ParamValue {
    Decimal(Decimal),
    U64(u64),
}

impl std::fmt::Display for ParamValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamValue::Decimal(d) => write!(f, "{}", d),
            ParamValue::U64(v) => write!(f, "{}", v),
        }
    }
}

/// A named axis in the parameter grid.
#[derive(Debug, Clone)]
struct Axis {
    name: String,
    values: Vec<ParamValue>,
}

/// One specific combination of parameter values.
#[derive(Debug, Clone)]
pub struct ParameterCombination {
    pub index: usize,
    pub params: Vec<(String, ParamValue)>,
}

impl ParameterCombination {
    /// Apply this combination's parameter values to an ArbitrageConfig.
    pub fn apply_to(&self, config: &mut ArbitrageConfig) {
        // Collect dynamic_thresholds entries separately
        let mut threshold_entries: BTreeMap<u64, Decimal> = BTreeMap::new();
        let mut has_threshold_params = false;

        for (name, value) in &self.params {
            match name.as_str() {
                // TailEnd params
                "tailend.max_spread_bps" => {
                    if let ParamValue::Decimal(v) = value {
                        config.tailend.max_spread_bps = *v;
                    }
                }
                "tailend.min_sustained_secs" => {
                    if let ParamValue::U64(v) = value {
                        config.tailend.min_sustained_secs = *v;
                    }
                }
                "tailend.max_recent_volatility" => {
                    if let ParamValue::Decimal(v) = value {
                        config.tailend.max_recent_volatility = *v;
                    }
                }
                "tailend.fok_cooldown_secs" => {
                    if let ParamValue::U64(v) = value {
                        config.tailend.fok_cooldown_secs = *v;
                    }
                }
                "tailend.stale_ob_secs" => {
                    if let ParamValue::U64(v) = value {
                        config.tailend.stale_ob_secs = *v as i64;
                    }
                }
                // Sizing params
                "sizing.kelly_multiplier" => {
                    if let ParamValue::Decimal(v) = value {
                        config.sizing.kelly_multiplier = *v;
                    }
                }
                "sizing.min_size" => {
                    if let ParamValue::Decimal(v) = value {
                        config.sizing.min_size = *v;
                    }
                }
                "sizing.max_size" => {
                    if let ParamValue::Decimal(v) = value {
                        config.sizing.max_size = *v;
                    }
                }
                // StopLoss params
                "stop_loss.reversal_pct" => {
                    if let ParamValue::Decimal(v) = value {
                        config.stop_loss.reversal_pct = *v;
                    }
                }
                "stop_loss.min_drop" => {
                    if let ParamValue::Decimal(v) = value {
                        config.stop_loss.min_drop = *v;
                    }
                }
                "stop_loss.trailing_distance" => {
                    if let ParamValue::Decimal(v) = value {
                        config.stop_loss.trailing_distance = *v;
                    }
                }
                "stop_loss.min_remaining_secs" => {
                    if let ParamValue::U64(v) = value {
                        config.stop_loss.min_remaining_secs = *v as i64;
                    }
                }
                // TailEnd post-entry params
                "tailend.post_entry_exit_drop" => {
                    if let ParamValue::Decimal(v) = value {
                        config.tailend.post_entry_exit_drop = *v;
                    }
                }
                "tailend.post_entry_window_secs" => {
                    if let ParamValue::U64(v) = value {
                        config.tailend.post_entry_window_secs = *v as i64;
                    }
                }
                // Dynamic threshold params: "tailend.dynamic_thresholds.{secs}"
                other if other.starts_with("tailend.dynamic_thresholds.") => {
                    has_threshold_params = true;
                    let secs_str = other.strip_prefix("tailend.dynamic_thresholds.").unwrap();
                    if let (Ok(secs), ParamValue::Decimal(v)) = (secs_str.parse::<u64>(), value) {
                        threshold_entries.insert(secs, *v);
                    }
                }
                _ => {
                    tracing::warn!(param = name, "Unknown sweep parameter, ignoring");
                }
            }
        }

        // Merge swept dynamic_thresholds into base config (preserve non-swept buckets)
        if has_threshold_params {
            let mut merged: BTreeMap<u64, Decimal> = config
                .tailend
                .dynamic_thresholds
                .iter()
                .copied()
                .collect();
            for (secs, val) in threshold_entries {
                merged.insert(secs, val);
            }
            let mut thresholds: Vec<(u64, Decimal)> = merged.into_iter().collect();
            // Sort descending by time (largest bucket first)
            thresholds.sort_by(|a, b| b.0.cmp(&a.0));

            // Auto-derive time_threshold_secs as max bucket time
            if let Some(&(max_secs, _)) = thresholds.first() {
                config.tailend.time_threshold_secs = max_secs;
            }
            config.tailend.dynamic_thresholds = thresholds;
        }
    }

    /// Get parameter values as sorted string map for display.
    pub fn params_map(&self) -> BTreeMap<String, String> {
        self.params
            .iter()
            .map(|(name, value)| (name.clone(), value.to_string()))
            .collect()
    }
}

/// Generates a grid of all parameter combinations from a SweepConfig.
pub struct ParameterGrid {
    axes: Vec<Axis>,
}

impl ParameterGrid {
    /// Build the grid from sweep config.
    pub fn from_config(config: &SweepConfig) -> Self {
        let mut axes = Vec::new();

        // TailEnd scalar params
        if let Some(ref range) = config.tailend.max_spread_bps {
            axes.push(Axis {
                name: "tailend.max_spread_bps".to_string(),
                values: range.expand().into_iter().map(ParamValue::Decimal).collect(),
            });
        }
        if let Some(ref range) = config.tailend.min_sustained_secs {
            axes.push(Axis {
                name: "tailend.min_sustained_secs".to_string(),
                values: range.expand().into_iter().map(ParamValue::U64).collect(),
            });
        }
        if let Some(ref range) = config.tailend.max_recent_volatility {
            axes.push(Axis {
                name: "tailend.max_recent_volatility".to_string(),
                values: range.expand().into_iter().map(ParamValue::Decimal).collect(),
            });
        }
        if let Some(ref range) = config.tailend.fok_cooldown_secs {
            axes.push(Axis {
                name: "tailend.fok_cooldown_secs".to_string(),
                values: range.expand().into_iter().map(ParamValue::U64).collect(),
            });
        }
        if let Some(ref range) = config.tailend.stale_ob_secs {
            axes.push(Axis {
                name: "tailend.stale_ob_secs".to_string(),
                values: range.expand().into_iter().map(ParamValue::U64).collect(),
            });
        }

        if let Some(ref range) = config.tailend.post_entry_exit_drop {
            axes.push(Axis {
                name: "tailend.post_entry_exit_drop".to_string(),
                values: range.expand().into_iter().map(ParamValue::Decimal).collect(),
            });
        }
        if let Some(ref range) = config.tailend.post_entry_window_secs {
            axes.push(Axis {
                name: "tailend.post_entry_window_secs".to_string(),
                values: range.expand().into_iter().map(ParamValue::U64).collect(),
            });
        }

        // Dynamic thresholds: each bucket becomes a separate axis
        if let Some(ref dt) = config.tailend.dynamic_thresholds {
            for (secs_str, range) in dt {
                // Validate that key parses as u64
                if secs_str.parse::<u64>().is_err() {
                    tracing::warn!(key = secs_str, "Ignoring non-numeric dynamic_thresholds key");
                    continue;
                }
                axes.push(Axis {
                    name: format!("tailend.dynamic_thresholds.{}", secs_str),
                    values: range.expand().into_iter().map(ParamValue::Decimal).collect(),
                });
            }
        }

        // Sizing params
        if let Some(ref range) = config.sizing.kelly_multiplier {
            axes.push(Axis {
                name: "sizing.kelly_multiplier".to_string(),
                values: range.expand().into_iter().map(ParamValue::Decimal).collect(),
            });
        }
        if let Some(ref range) = config.sizing.min_size {
            axes.push(Axis {
                name: "sizing.min_size".to_string(),
                values: range.expand().into_iter().map(ParamValue::Decimal).collect(),
            });
        }
        if let Some(ref range) = config.sizing.max_size {
            axes.push(Axis {
                name: "sizing.max_size".to_string(),
                values: range.expand().into_iter().map(ParamValue::Decimal).collect(),
            });
        }

        // StopLoss params
        if let Some(ref range) = config.stop_loss.reversal_pct {
            axes.push(Axis {
                name: "stop_loss.reversal_pct".to_string(),
                values: range.expand().into_iter().map(ParamValue::Decimal).collect(),
            });
        }
        if let Some(ref range) = config.stop_loss.min_drop {
            axes.push(Axis {
                name: "stop_loss.min_drop".to_string(),
                values: range.expand().into_iter().map(ParamValue::Decimal).collect(),
            });
        }
        if let Some(ref range) = config.stop_loss.trailing_distance {
            axes.push(Axis {
                name: "stop_loss.trailing_distance".to_string(),
                values: range.expand().into_iter().map(ParamValue::Decimal).collect(),
            });
        }
        if let Some(ref range) = config.stop_loss.min_remaining_secs {
            axes.push(Axis {
                name: "stop_loss.min_remaining_secs".to_string(),
                values: range.expand().into_iter().map(ParamValue::U64).collect(),
            });
        }

        Self { axes }
    }

    /// Total number of combinations (cartesian product size).
    pub fn total_combinations(&self) -> usize {
        if self.axes.is_empty() {
            return 1; // One run with base config
        }
        self.axes.iter().map(|a| a.values.len()).product()
    }

    /// Generate all combinations via iterative cartesian product.
    pub fn combinations(&self) -> Vec<ParameterCombination> {
        let total = self.total_combinations();
        if self.axes.is_empty() {
            return vec![ParameterCombination {
                index: 0,
                params: Vec::new(),
            }];
        }

        let mut result = Vec::with_capacity(total);

        // Iterative cartesian product using modular arithmetic
        for idx in 0..total {
            let mut params = Vec::with_capacity(self.axes.len());
            let mut remainder = idx;

            for axis in self.axes.iter().rev() {
                let axis_idx = remainder % axis.values.len();
                remainder /= axis.values.len();
                params.push((axis.name.clone(), axis.values[axis_idx].clone()));
            }

            params.reverse();
            result.push(ParameterCombination {
                index: idx,
                params,
            });
        }

        result
    }

    /// List of axis names for display.
    pub fn axis_names(&self) -> Vec<&str> {
        self.axes.iter().map(|a| a.name.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep::config::{ParamRange, SweepConfig, TailEndSweepParams};
    use rust_decimal_macros::dec;

    #[test]
    fn grid_empty_config_produces_one_combination() {
        let config = SweepConfig::default();
        let grid = ParameterGrid::from_config(&config);
        assert_eq!(grid.total_combinations(), 1);
        let combos = grid.combinations();
        assert_eq!(combos.len(), 1);
        assert!(combos[0].params.is_empty());
    }

    #[test]
    fn grid_single_axis() {
        let config = SweepConfig {
            tailend: TailEndSweepParams {
                max_spread_bps: Some(ParamRange::Values(vec![
                    dec!(100),
                    dec!(200),
                    dec!(300),
                ])),
                ..Default::default()
            },
            ..Default::default()
        };
        let grid = ParameterGrid::from_config(&config);
        assert_eq!(grid.total_combinations(), 3);
        let combos = grid.combinations();
        assert_eq!(combos.len(), 3);
        assert_eq!(combos[0].index, 0);
        assert_eq!(combos[2].index, 2);
    }

    #[test]
    fn grid_two_axes_cartesian_product() {
        let config = SweepConfig {
            tailend: TailEndSweepParams {
                max_spread_bps: Some(ParamRange::Values(vec![dec!(100), dec!(200)])),
                min_sustained_secs: Some(crate::sweep::config::IntParamRange::Values(vec![
                    3, 5,
                ])),
                ..Default::default()
            },
            ..Default::default()
        };
        let grid = ParameterGrid::from_config(&config);
        assert_eq!(grid.total_combinations(), 4); // 2 * 2

        let combos = grid.combinations();
        assert_eq!(combos.len(), 4);

        // Verify all combinations are unique
        let param_strings: Vec<String> = combos
            .iter()
            .map(|c| {
                c.params
                    .iter()
                    .map(|(n, v)| format!("{}={}", n, v))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect();
        let unique: std::collections::HashSet<_> = param_strings.iter().collect();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn grid_dynamic_thresholds_cartesian() {
        let mut dt = BTreeMap::new();
        dt.insert(
            "120".to_string(),
            ParamRange::Values(vec![dec!(0.88), dec!(0.90)]),
        );
        dt.insert("60".to_string(), ParamRange::Values(vec![dec!(0.91), dec!(0.93)]));

        let config = SweepConfig {
            tailend: TailEndSweepParams {
                dynamic_thresholds: Some(dt),
                ..Default::default()
            },
            ..Default::default()
        };
        let grid = ParameterGrid::from_config(&config);
        assert_eq!(grid.total_combinations(), 4); // 2 * 2

        let combos = grid.combinations();
        // Each combo should have 2 params (one per bucket)
        for combo in &combos {
            assert_eq!(combo.params.len(), 2);
        }
    }

    #[test]
    fn apply_to_modifies_config() {
        let combo = ParameterCombination {
            index: 0,
            params: vec![
                ("tailend.max_spread_bps".to_string(), ParamValue::Decimal(dec!(150))),
                ("sizing.kelly_multiplier".to_string(), ParamValue::Decimal(dec!(0.30))),
                ("stop_loss.reversal_pct".to_string(), ParamValue::Decimal(dec!(0.007))),
            ],
        };

        let mut config = ArbitrageConfig::default();
        combo.apply_to(&mut config);

        assert_eq!(config.tailend.max_spread_bps, dec!(150));
        assert_eq!(config.sizing.kelly_multiplier, dec!(0.30));
        assert_eq!(config.stop_loss.reversal_pct, dec!(0.007));
    }

    #[test]
    fn apply_to_assembles_dynamic_thresholds() {
        let combo = ParameterCombination {
            index: 0,
            params: vec![
                ("tailend.dynamic_thresholds.120".to_string(), ParamValue::Decimal(dec!(0.88))),
                ("tailend.dynamic_thresholds.60".to_string(), ParamValue::Decimal(dec!(0.93))),
                ("tailend.dynamic_thresholds.30".to_string(), ParamValue::Decimal(dec!(0.97))),
            ],
        };

        let mut config = ArbitrageConfig::default();
        combo.apply_to(&mut config);

        assert_eq!(config.tailend.time_threshold_secs, 120);
        // Default has 4 buckets (120,90,60,30); swept 3 override, bucket 90 preserved
        assert_eq!(config.tailend.dynamic_thresholds.len(), 4);
        // Sorted descending by time
        assert_eq!(config.tailend.dynamic_thresholds[0], (120, dec!(0.88)));
        assert_eq!(config.tailend.dynamic_thresholds[1], (90, dec!(0.92))); // preserved from default
        assert_eq!(config.tailend.dynamic_thresholds[2], (60, dec!(0.93)));
        assert_eq!(config.tailend.dynamic_thresholds[3], (30, dec!(0.97)));
    }

    #[test]
    fn apply_to_merges_dynamic_thresholds_with_base() {
        // Base config has 4 buckets
        let mut config = ArbitrageConfig::default();
        config.tailend.dynamic_thresholds = vec![
            (120, dec!(0.90)),
            (90, dec!(0.92)),
            (60, dec!(0.95)),
            (30, dec!(0.95)),
        ];
        config.tailend.time_threshold_secs = 120;

        // Sweep only overrides bucket 60
        let combo = ParameterCombination {
            index: 0,
            params: vec![(
                "tailend.dynamic_thresholds.60".to_string(),
                ParamValue::Decimal(dec!(0.93)),
            )],
        };
        combo.apply_to(&mut config);

        // All 4 buckets preserved, only bucket 60 changed
        assert_eq!(config.tailend.dynamic_thresholds.len(), 4);
        assert_eq!(config.tailend.time_threshold_secs, 120); // NOT 60
        assert_eq!(config.tailend.dynamic_thresholds[0], (120, dec!(0.90)));
        assert_eq!(config.tailend.dynamic_thresholds[1], (90, dec!(0.92)));
        assert_eq!(config.tailend.dynamic_thresholds[2], (60, dec!(0.93))); // overridden
        assert_eq!(config.tailend.dynamic_thresholds[3], (30, dec!(0.95)));
    }
}
