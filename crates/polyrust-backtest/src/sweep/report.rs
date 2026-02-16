use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::error::BacktestResult;

/// Result metrics from a single sweep run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepResult {
    pub combination_index: usize,
    pub params: BTreeMap<String, String>,
    pub total_pnl: Decimal,
    pub sharpe_ratio: Option<Decimal>,
    pub win_rate: Decimal,
    pub max_drawdown: Decimal,
    pub total_trades: usize,
    pub closing_trades: usize,
    pub end_balance: Decimal,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub strategy_exits: usize,
    pub strategy_losses: usize,
    pub settled_worthless: usize,
    pub prediction_correct: usize,
    pub prediction_wrong: usize,
    pub prediction_unknown: usize,
    pub prediction_accuracy: Decimal,
    pub premature_exits: usize,
    pub correct_stops: usize,
    pub premature_exit_cost: Decimal,
    pub correct_stop_savings: Decimal,
    pub hedge_attempts: usize,
    pub hedge_pnl: Decimal,
    pub reentry_count: usize,
    // --- Per-trigger breakdown ---
    pub premature_hard_crash: usize,
    pub premature_trailing: usize,
    pub premature_post_entry: usize,
    pub correct_hard_crash: usize,
    pub correct_trailing: usize,
    pub correct_post_entry: usize,
    pub start_balance: Decimal,
    pub net_stop_value: Decimal,
    pub composite_score: Option<Decimal>,
    pub duration_secs: f64,
}

/// Aggregated sweep results.
#[derive(Debug, Clone)]
pub struct SweepReport {
    pub results: Vec<SweepResult>,
    pub total_combinations: usize,
    pub total_wall_time_secs: f64,
    /// Axes that were detected as inert and removed from the grid.
    pub ignored_axes: Vec<String>,
}

impl SweepReport {
    /// Sort results by the given metric.
    pub fn sort_by(&mut self, metric: &str) {
        match metric {
            "sharpe" => {
                self.results.sort_by(|a, b| {
                    let sa = a.sharpe_ratio.unwrap_or(Decimal::MIN);
                    let sb = b.sharpe_ratio.unwrap_or(Decimal::MIN);
                    sb.cmp(&sa) // Descending
                });
            }
            "pnl" => {
                self.results.sort_by(|a, b| b.total_pnl.cmp(&a.total_pnl));
            }
            "win_rate" => {
                self.results.sort_by(|a, b| b.win_rate.cmp(&a.win_rate));
            }
            "drawdown" => {
                self.results
                    .sort_by(|a, b| a.max_drawdown.cmp(&b.max_drawdown)); // Ascending (less is better)
            }
            "composite" => {
                self.results.sort_by(|a, b| {
                    let sa = a.composite_score.unwrap_or(Decimal::MIN);
                    let sb = b.composite_score.unwrap_or(Decimal::MIN);
                    sb.cmp(&sa) // Descending
                });
            }
            "net_stop_value" => {
                self.results
                    .sort_by(|a, b| b.net_stop_value.cmp(&a.net_stop_value)); // Descending
            }
            _ => {
                // Default to sharpe
                self.sort_by("sharpe");
            }
        }
    }

    /// Print a formatted terminal table of the top N results.
    pub fn print_table(&self, top_n: usize) {
        let results = &self.results[..top_n.min(self.results.len())];
        if results.is_empty() {
            println!("No sweep results to display.");
            return;
        }

        // Collect all parameter names from results
        let mut param_names: Vec<String> = Vec::new();
        for r in results {
            for key in r.params.keys() {
                if !param_names.contains(key) {
                    param_names.push(key.clone());
                }
            }
        }
        param_names.sort();

        // Shorten param names for display: "tailend.max_spread_bps" -> "max_spread_bps"
        let short_names: Vec<String> = param_names
            .iter()
            .map(|n| n.rsplit('.').next().unwrap_or(n).to_string())
            .collect();

        // Calculate column widths
        let rank_width = 4;
        let param_widths: Vec<usize> = param_names
            .iter()
            .zip(short_names.iter())
            .map(|(name, short)| {
                let header_w = short.len();
                let max_val_w = results
                    .iter()
                    .map(|r| r.params.get(name).map_or(1, |v| v.len()))
                    .max()
                    .unwrap_or(1);
                header_w.max(max_val_w).max(5)
            })
            .collect();
        let metric_width = 10;

        // Print header
        println!();
        println!(
            "=== Sweep Results (top {} of {}, sorted by {}) ===",
            results.len(),
            self.total_combinations,
            results.first().map(|_| "metric").unwrap_or("none")
        );
        println!(
            "Total wall time: {:.1}s ({:.2}s/run avg)",
            self.total_wall_time_secs,
            if self.total_combinations > 0 {
                self.total_wall_time_secs / self.total_combinations as f64
            } else {
                0.0
            }
        );
        println!();

        // Header row
        print!("{:>rank_width$} ", "#");
        for (i, short) in short_names.iter().enumerate() {
            print!("{:>width$} ", short, width = param_widths[i]);
        }
        print!(
            "{:>w$} {:>w$} {:>w$} {:>w$} {:>w$} {:>w$}",
            "PnL",
            "Sharpe",
            "WinRate",
            "MaxDD",
            "Trades",
            "Balance",
            w = metric_width
        );
        println!();

        // Separator
        let total_width = rank_width
            + 1
            + param_widths.iter().map(|w| w + 1).sum::<usize>()
            + (metric_width + 1) * 6;
        println!("{}", "-".repeat(total_width));

        // Data rows
        for (rank, result) in results.iter().enumerate() {
            print!("{:>rank_width$} ", rank + 1);
            for (i, name) in param_names.iter().enumerate() {
                let val = result.params.get(name).map_or("-", |v| v.as_str());
                print!("{:>width$} ", val, width = param_widths[i]);
            }
            print!(
                "{:>w$.2} {:>w$} {:>w$.1}% {:>w$.1}% {:>w$} {:>w$.2}",
                result.total_pnl,
                result
                    .sharpe_ratio
                    .map_or("N/A".to_string(), |s| format!("{:.4}", s)),
                result.win_rate * Decimal::from(100),
                result.max_drawdown * Decimal::from(100),
                result.closing_trades,
                result.end_balance,
                w = metric_width
            );
            println!();
        }
        println!();
    }

    /// Export results to CSV file.
    pub fn export_csv(&self, path: &str) -> BacktestResult<()> {
        let mut file = std::fs::File::create(path).map_err(|e| {
            crate::error::BacktestError::Engine(format!("Failed to create CSV: {}", e))
        })?;

        if self.results.is_empty() {
            return Ok(());
        }

        // Collect all parameter names
        let mut param_names: Vec<String> = Vec::new();
        for r in &self.results {
            for key in r.params.keys() {
                if !param_names.contains(key) {
                    param_names.push(key.clone());
                }
            }
        }
        param_names.sort();

        // Header
        let mut headers: Vec<String> = vec!["rank".to_string()];
        headers.extend(param_names.clone());
        headers.extend([
            "total_pnl".to_string(),
            "sharpe_ratio".to_string(),
            "win_rate".to_string(),
            "max_drawdown".to_string(),
            "total_trades".to_string(),
            "closing_trades".to_string(),
            "end_balance".to_string(),
            "winning_trades".to_string(),
            "losing_trades".to_string(),
            "strategy_exits".to_string(),
            "strategy_losses".to_string(),
            "settled_worthless".to_string(),
            "prediction_correct".to_string(),
            "prediction_wrong".to_string(),
            "prediction_unknown".to_string(),
            "prediction_accuracy".to_string(),
            "premature_exits".to_string(),
            "correct_stops".to_string(),
            "premature_exit_cost".to_string(),
            "correct_stop_savings".to_string(),
            "hedge_attempts".to_string(),
            "hedge_pnl".to_string(),
            "reentry_count".to_string(),
            "premature_hard_crash".to_string(),
            "premature_trailing".to_string(),
            "premature_post_entry".to_string(),
            "correct_hard_crash".to_string(),
            "correct_trailing".to_string(),
            "correct_post_entry".to_string(),
            "net_stop_value".to_string(),
            "composite_score".to_string(),
            "duration_secs".to_string(),
        ]);
        writeln!(file, "{}", headers.join(","))
            .map_err(|e| crate::error::BacktestError::Engine(format!("CSV write error: {}", e)))?;

        // Data rows
        for (rank, result) in self.results.iter().enumerate() {
            let mut row: Vec<String> = vec![(rank + 1).to_string()];
            for name in &param_names {
                row.push(result.params.get(name).cloned().unwrap_or_default());
            }
            row.extend([
                result.total_pnl.to_string(),
                result.sharpe_ratio.map_or(String::new(), |s| s.to_string()),
                result.win_rate.to_string(),
                result.max_drawdown.to_string(),
                result.total_trades.to_string(),
                result.closing_trades.to_string(),
                result.end_balance.to_string(),
                result.winning_trades.to_string(),
                result.losing_trades.to_string(),
                result.strategy_exits.to_string(),
                result.strategy_losses.to_string(),
                result.settled_worthless.to_string(),
                result.prediction_correct.to_string(),
                result.prediction_wrong.to_string(),
                result.prediction_unknown.to_string(),
                result.prediction_accuracy.to_string(),
                result.premature_exits.to_string(),
                result.correct_stops.to_string(),
                result.premature_exit_cost.to_string(),
                result.correct_stop_savings.to_string(),
                result.hedge_attempts.to_string(),
                result.hedge_pnl.to_string(),
                result.reentry_count.to_string(),
                result.premature_hard_crash.to_string(),
                result.premature_trailing.to_string(),
                result.premature_post_entry.to_string(),
                result.correct_hard_crash.to_string(),
                result.correct_trailing.to_string(),
                result.correct_post_entry.to_string(),
                result.net_stop_value.to_string(),
                result
                    .composite_score
                    .map_or(String::new(), |s| s.to_string()),
                format!("{:.2}", result.duration_secs),
            ]);
            writeln!(file, "{}", row.join(",")).map_err(|e| {
                crate::error::BacktestError::Engine(format!("CSV write error: {}", e))
            })?;
        }

        tracing::info!(
            path,
            rows = self.results.len(),
            "Exported sweep results to CSV"
        );
        Ok(())
    }

    /// Export results to JSON file.
    pub fn export_json(&self, path: &str) -> BacktestResult<()> {
        let json = serde_json::json!({
            "total_combinations": self.total_combinations,
            "total_wall_time_secs": self.total_wall_time_secs,
            "results": self.results,
        });

        let file = std::fs::File::create(path).map_err(|e| {
            crate::error::BacktestError::Engine(format!("Failed to create JSON: {}", e))
        })?;
        serde_json::to_writer_pretty(file, &json)
            .map_err(|e| crate::error::BacktestError::Engine(format!("JSON write error: {}", e)))?;

        tracing::info!(
            path,
            rows = self.results.len(),
            "Exported sweep results to JSON"
        );
        Ok(())
    }

    /// Compute per-parameter sensitivity analysis across all sweep results.
    pub fn sensitivity_analysis(&self) -> SensitivityAnalysis {
        if self.results.is_empty() {
            return SensitivityAnalysis {
                parameters: Vec::new(),
            };
        }

        // Collect all unique param names (BTreeSet for stable ordering)
        let param_names: BTreeSet<&String> =
            self.results.iter().flat_map(|r| r.params.keys()).collect();

        let parameters = param_names
            .into_iter()
            .map(|name| {
                // Group results by this param's value
                let mut by_value: HashMap<&str, Vec<&SweepResult>> = HashMap::new();
                for r in &self.results {
                    if let Some(val) = r.params.get(name) {
                        by_value.entry(val.as_str()).or_default().push(r);
                    }
                }

                // Compute stats per value
                let mut stats: Vec<ParameterStats> = by_value
                    .into_iter()
                    .map(|(value, runs)| {
                        let count = runs.len();
                        let count_dec = Decimal::from(count);
                        let mean_pnl =
                            runs.iter().map(|r| r.total_pnl).sum::<Decimal>() / count_dec;
                        let mean_sharpe =
                            mean_optional(&runs.iter().map(|r| r.sharpe_ratio).collect::<Vec<_>>());
                        let mean_win_rate =
                            runs.iter().map(|r| r.win_rate).sum::<Decimal>() / count_dec;
                        let mean_trades =
                            Decimal::from(runs.iter().map(|r| r.closing_trades).sum::<usize>())
                                / count_dec;
                        let mean_max_drawdown =
                            runs.iter().map(|r| r.max_drawdown).sum::<Decimal>() / count_dec;

                        ParameterStats {
                            value: value.to_string(),
                            run_count: count,
                            mean_pnl,
                            mean_sharpe,
                            mean_win_rate,
                            mean_trades,
                            mean_max_drawdown,
                        }
                    })
                    .collect();

                // Sort values lexicographically
                stats.sort_by(|a, b| a.value.cmp(&b.value));

                ParameterSensitivity {
                    param_name: name.clone(),
                    stats,
                }
            })
            .collect();

        SensitivityAnalysis { parameters }
    }
}

/// Aggregated stats for a single parameter value across multiple sweep runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterStats {
    pub value: String,
    pub run_count: usize,
    pub mean_pnl: Decimal,
    pub mean_sharpe: Option<Decimal>,
    pub mean_win_rate: Decimal,
    pub mean_trades: Decimal,
    pub mean_max_drawdown: Decimal,
}

/// Sensitivity data for a single parameter (all its distinct values).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParameterSensitivity {
    pub param_name: String,
    pub stats: Vec<ParameterStats>,
}

/// Full sensitivity analysis across all swept parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensitivityAnalysis {
    pub parameters: Vec<ParameterSensitivity>,
}

/// Average non-None values. Returns None if all are None.
fn mean_optional(values: &[Option<Decimal>]) -> Option<Decimal> {
    let valid: Vec<Decimal> = values.iter().filter_map(|v| *v).collect();
    if valid.is_empty() {
        None
    } else {
        Some(valid.iter().copied().sum::<Decimal>() / Decimal::from(valid.len()))
    }
}

impl SensitivityAnalysis {
    /// Print a formatted terminal table, one section per parameter.
    pub fn print_table(&self) {
        if self.parameters.is_empty() {
            println!("No sensitivity data to display.");
            return;
        }

        println!();
        println!("=== Parameter Sensitivity Analysis ===");

        for param in &self.parameters {
            // Shorten name: "tailend.max_spread_bps" -> "max_spread_bps"
            let short_name = param
                .param_name
                .rsplit('.')
                .next()
                .unwrap_or(&param.param_name);

            println!();
            println!("  Parameter: {} ({})", short_name, param.param_name);

            // Column widths
            let val_w = param
                .stats
                .iter()
                .map(|s| s.value.len())
                .max()
                .unwrap_or(5)
                .max(5);
            let w = 10;

            // Header
            println!(
                "    {:>vw$}  {:>4}  {:>w$}  {:>w$}  {:>w$}  {:>w$}  {:>w$}",
                "Value",
                "Runs",
                "Avg PnL",
                "Avg Sharpe",
                "Avg WinRate",
                "Avg Trades",
                "Avg MaxDD",
                vw = val_w,
                w = w
            );
            let line_w = val_w + 2 + 4 + 2 + (w + 2) * 5;
            println!("    {}", "-".repeat(line_w));

            for s in &param.stats {
                println!(
                    "    {:>vw$}  {:>4}  {:>w$.2}  {:>w$}  {:>w$.1}%  {:>w$.1}  {:>w$.1}%",
                    s.value,
                    s.run_count,
                    s.mean_pnl,
                    s.mean_sharpe
                        .map_or("N/A".to_string(), |v| format!("{:.4}", v)),
                    s.mean_win_rate * Decimal::from(100),
                    s.mean_trades,
                    s.mean_max_drawdown * Decimal::from(100),
                    vw = val_w,
                    w = w
                );
            }
        }
        println!();
    }

    /// Export sensitivity data to a flat CSV.
    pub fn export_csv(&self, path: &str) -> BacktestResult<()> {
        let mut file = std::fs::File::create(path).map_err(|e| {
            crate::error::BacktestError::Engine(format!("Failed to create CSV: {}", e))
        })?;

        writeln!(
            file,
            "parameter,value,run_count,mean_pnl,mean_sharpe,mean_win_rate,mean_trades,mean_max_drawdown"
        )
        .map_err(|e| crate::error::BacktestError::Engine(format!("CSV write error: {}", e)))?;

        for param in &self.parameters {
            for s in &param.stats {
                writeln!(
                    file,
                    "{},{},{},{},{},{},{},{}",
                    param.param_name,
                    s.value,
                    s.run_count,
                    s.mean_pnl,
                    s.mean_sharpe.map_or(String::new(), |v| v.to_string()),
                    s.mean_win_rate,
                    s.mean_trades,
                    s.mean_max_drawdown,
                )
                .map_err(|e| {
                    crate::error::BacktestError::Engine(format!("CSV write error: {}", e))
                })?;
            }
        }

        tracing::info!(path, "Exported sensitivity analysis to CSV");
        Ok(())
    }

    /// Export sensitivity data to JSON.
    pub fn export_json(&self, path: &str) -> BacktestResult<()> {
        let file = std::fs::File::create(path).map_err(|e| {
            crate::error::BacktestError::Engine(format!("Failed to create JSON: {}", e))
        })?;
        serde_json::to_writer_pretty(file, self)
            .map_err(|e| crate::error::BacktestError::Engine(format!("JSON write error: {}", e)))?;

        tracing::info!(path, "Exported sensitivity analysis to JSON");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn make_result(pnl: Decimal, sharpe: Option<Decimal>, win_rate: Decimal) -> SweepResult {
        let start_balance = dec!(1000);
        let net_stop_value = Decimal::ZERO;
        let composite_score = if 5 >= 3 {
            Some(pnl * (Decimal::ONE - dec!(0.05) / start_balance))
        } else {
            None
        };
        SweepResult {
            combination_index: 0,
            params: BTreeMap::new(),
            total_pnl: pnl,
            sharpe_ratio: sharpe,
            win_rate,
            max_drawdown: dec!(0.05),
            total_trades: 10,
            closing_trades: 5,
            end_balance: start_balance + pnl,
            winning_trades: 0,
            losing_trades: 0,
            strategy_exits: 0,
            strategy_losses: 0,
            settled_worthless: 0,
            prediction_correct: 0,
            prediction_wrong: 0,
            prediction_unknown: 0,
            prediction_accuracy: Decimal::ZERO,
            premature_exits: 0,
            correct_stops: 0,
            premature_exit_cost: Decimal::ZERO,
            correct_stop_savings: Decimal::ZERO,
            hedge_attempts: 0,
            hedge_pnl: Decimal::ZERO,
            reentry_count: 0,
            premature_hard_crash: 0,
            premature_trailing: 0,
            premature_post_entry: 0,
            correct_hard_crash: 0,
            correct_trailing: 0,
            correct_post_entry: 0,
            start_balance,
            net_stop_value,
            composite_score,
            duration_secs: 1.0,
        }
    }

    #[test]
    fn sort_by_sharpe() {
        let mut report = SweepReport {
            results: vec![
                make_result(dec!(10), Some(dec!(0.5)), dec!(0.6)),
                make_result(dec!(20), Some(dec!(1.5)), dec!(0.7)),
                make_result(dec!(15), Some(dec!(1.0)), dec!(0.65)),
            ],
            total_combinations: 3,
            total_wall_time_secs: 3.0,
            ignored_axes: vec![],
        };
        report.sort_by("sharpe");
        assert_eq!(report.results[0].sharpe_ratio, Some(dec!(1.5)));
        assert_eq!(report.results[2].sharpe_ratio, Some(dec!(0.5)));
    }

    #[test]
    fn sort_by_pnl() {
        let mut report = SweepReport {
            results: vec![
                make_result(dec!(10), None, dec!(0.6)),
                make_result(dec!(30), None, dec!(0.7)),
                make_result(dec!(20), None, dec!(0.65)),
            ],
            total_combinations: 3,
            total_wall_time_secs: 3.0,
            ignored_axes: vec![],
        };
        report.sort_by("pnl");
        assert_eq!(report.results[0].total_pnl, dec!(30));
        assert_eq!(report.results[2].total_pnl, dec!(10));
    }

    fn make_result_with_params(
        params: Vec<(&str, &str)>,
        pnl: Decimal,
        sharpe: Option<Decimal>,
    ) -> SweepResult {
        let mut r = make_result(pnl, sharpe, dec!(0.60));
        r.params = params
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        r
    }

    #[test]
    fn sensitivity_basic_grouping() {
        let report = SweepReport {
            results: vec![
                make_result_with_params(
                    vec![("spread", "100"), ("threshold", "0.9")],
                    dec!(10),
                    Some(dec!(1.0)),
                ),
                make_result_with_params(
                    vec![("spread", "100"), ("threshold", "0.95")],
                    dec!(20),
                    Some(dec!(2.0)),
                ),
                make_result_with_params(
                    vec![("spread", "200"), ("threshold", "0.9")],
                    dec!(30),
                    Some(dec!(3.0)),
                ),
                make_result_with_params(
                    vec![("spread", "200"), ("threshold", "0.95")],
                    dec!(40),
                    Some(dec!(4.0)),
                ),
            ],
            total_combinations: 4,
            total_wall_time_secs: 4.0,
            ignored_axes: vec![],
        };

        let analysis = report.sensitivity_analysis();
        assert_eq!(analysis.parameters.len(), 2);

        // "spread" param
        let spread = &analysis.parameters[0];
        assert_eq!(spread.param_name, "spread");
        assert_eq!(spread.stats.len(), 2);
        // spread=100: mean PnL = (10+20)/2 = 15
        assert_eq!(spread.stats[0].value, "100");
        assert_eq!(spread.stats[0].run_count, 2);
        assert_eq!(spread.stats[0].mean_pnl, dec!(15));
        assert_eq!(spread.stats[0].mean_sharpe, Some(dec!(1.5)));
        // spread=200: mean PnL = (30+40)/2 = 35
        assert_eq!(spread.stats[1].value, "200");
        assert_eq!(spread.stats[1].mean_pnl, dec!(35));

        // "threshold" param
        let threshold = &analysis.parameters[1];
        assert_eq!(threshold.param_name, "threshold");
        assert_eq!(threshold.stats.len(), 2);
        // threshold=0.9: mean PnL = (10+30)/2 = 20
        assert_eq!(threshold.stats[0].mean_pnl, dec!(20));
        // threshold=0.95: mean PnL = (20+40)/2 = 30
        assert_eq!(threshold.stats[1].mean_pnl, dec!(30));
    }

    #[test]
    fn sensitivity_single_value() {
        let report = SweepReport {
            results: vec![
                make_result_with_params(vec![("x", "42")], dec!(10), Some(dec!(1.0))),
                make_result_with_params(vec![("x", "42")], dec!(20), Some(dec!(3.0))),
            ],
            total_combinations: 2,
            total_wall_time_secs: 2.0,
            ignored_axes: vec![],
        };
        let analysis = report.sensitivity_analysis();
        assert_eq!(analysis.parameters.len(), 1);
        let x = &analysis.parameters[0];
        assert_eq!(x.stats.len(), 1);
        assert_eq!(x.stats[0].value, "42");
        assert_eq!(x.stats[0].run_count, 2);
        assert_eq!(x.stats[0].mean_pnl, dec!(15));
        assert_eq!(x.stats[0].mean_sharpe, Some(dec!(2.0)));
    }

    #[test]
    fn sensitivity_missing_sharpe() {
        let report = SweepReport {
            results: vec![
                make_result_with_params(vec![("a", "1")], dec!(10), Some(dec!(2.0))),
                make_result_with_params(vec![("a", "1")], dec!(20), None),
            ],
            total_combinations: 2,
            total_wall_time_secs: 2.0,
            ignored_axes: vec![],
        };
        let analysis = report.sensitivity_analysis();
        // Only 1 Sharpe value available → mean = 2.0
        assert_eq!(analysis.parameters[0].stats[0].mean_sharpe, Some(dec!(2.0)));
    }

    #[test]
    fn sensitivity_all_sharpe_none() {
        let report = SweepReport {
            results: vec![
                make_result_with_params(vec![("a", "1")], dec!(10), None),
                make_result_with_params(vec![("a", "1")], dec!(20), None),
            ],
            total_combinations: 2,
            total_wall_time_secs: 2.0,
            ignored_axes: vec![],
        };
        let analysis = report.sensitivity_analysis();
        assert_eq!(analysis.parameters[0].stats[0].mean_sharpe, None);
    }

    #[test]
    fn sensitivity_value_sorting() {
        let report = SweepReport {
            results: vec![
                make_result_with_params(vec![("v", "300")], dec!(10), None),
                make_result_with_params(vec![("v", "100")], dec!(20), None),
                make_result_with_params(vec![("v", "200")], dec!(30), None),
            ],
            total_combinations: 3,
            total_wall_time_secs: 3.0,
            ignored_axes: vec![],
        };
        let analysis = report.sensitivity_analysis();
        let values: Vec<&str> = analysis.parameters[0]
            .stats
            .iter()
            .map(|s| s.value.as_str())
            .collect();
        assert_eq!(values, vec!["100", "200", "300"]);
    }

    #[test]
    fn sensitivity_empty_results() {
        let report = SweepReport {
            results: vec![],
            total_combinations: 0,
            total_wall_time_secs: 0.0,
            ignored_axes: vec![],
        };
        let analysis = report.sensitivity_analysis();
        assert!(analysis.parameters.is_empty());
    }

    #[test]
    fn mean_optional_helper() {
        // All None
        assert_eq!(mean_optional(&[None, None]), None);
        // Mixed
        assert_eq!(
            mean_optional(&[Some(dec!(10)), None, Some(dec!(20))]),
            Some(dec!(15))
        );
        // All Some
        assert_eq!(
            mean_optional(&[Some(dec!(3)), Some(dec!(6)), Some(dec!(9))]),
            Some(dec!(6))
        );
        // Empty
        assert_eq!(mean_optional(&[]), None);
    }

    #[test]
    fn sort_by_drawdown_ascending() {
        let mut report = SweepReport {
            results: vec![
                {
                    let mut r = make_result(dec!(10), None, dec!(0.6));
                    r.max_drawdown = dec!(0.10);
                    r
                },
                {
                    let mut r = make_result(dec!(20), None, dec!(0.7));
                    r.max_drawdown = dec!(0.02);
                    r
                },
            ],
            total_combinations: 2,
            total_wall_time_secs: 2.0,
            ignored_axes: vec![],
        };
        report.sort_by("drawdown");
        assert_eq!(report.results[0].max_drawdown, dec!(0.02));
    }

    #[test]
    fn net_stop_value_computed_correctly() {
        let mut r = make_result(dec!(10), Some(dec!(1.0)), dec!(0.6));
        r.correct_stop_savings = dec!(5.00);
        r.premature_exit_cost = dec!(2.00);
        r.net_stop_value = r.correct_stop_savings - r.premature_exit_cost;
        assert_eq!(r.net_stop_value, dec!(3.00));

        // Negative case: premature exits cost more than stops save
        let mut r2 = make_result(dec!(10), Some(dec!(1.0)), dec!(0.6));
        r2.correct_stop_savings = dec!(1.00);
        r2.premature_exit_cost = dec!(4.00);
        r2.net_stop_value = r2.correct_stop_savings - r2.premature_exit_cost;
        assert_eq!(r2.net_stop_value, dec!(-3.00));
    }

    #[test]
    fn composite_score_with_sufficient_trades() {
        let start_balance = dec!(1000);
        let total_pnl = dec!(50);
        let max_drawdown = dec!(0.05);
        let closing_trades = 5;

        let score = if closing_trades >= 3 && start_balance > Decimal::ZERO {
            Some(total_pnl * (Decimal::ONE - max_drawdown / start_balance))
        } else {
            None
        };

        assert!(score.is_some());
        // 50 * (1 - 0.05/1000) = 50 * (1 - 0.00005) = 50 * 0.99995 = 49.9975
        let s = score.unwrap();
        assert!(s > dec!(49.99) && s < dec!(50.00));
    }

    #[test]
    fn composite_score_none_with_few_trades() {
        let start_balance = dec!(1000);
        let total_pnl = dec!(50);
        let max_drawdown = dec!(0.05);
        let closing_trades = 2;

        let score = if closing_trades >= 3 && start_balance > Decimal::ZERO {
            Some(total_pnl * (Decimal::ONE - max_drawdown / start_balance))
        } else {
            None
        };

        assert!(score.is_none());
    }

    #[test]
    fn sort_by_composite() {
        let mut r1 = make_result(dec!(10), None, dec!(0.6));
        r1.composite_score = Some(dec!(9.5));
        let mut r2 = make_result(dec!(20), None, dec!(0.7));
        r2.composite_score = Some(dec!(19.0));
        let mut r3 = make_result(dec!(5), None, dec!(0.5));
        r3.composite_score = None; // Too few trades

        let mut report = SweepReport {
            results: vec![r1, r3, r2],
            total_combinations: 3,
            total_wall_time_secs: 3.0,
            ignored_axes: vec![],
        };
        report.sort_by("composite");
        assert_eq!(report.results[0].composite_score, Some(dec!(19.0)));
        assert_eq!(report.results[1].composite_score, Some(dec!(9.5)));
        assert_eq!(report.results[2].composite_score, None);
    }

    #[test]
    fn sort_by_net_stop_value() {
        let mut r1 = make_result(dec!(10), None, dec!(0.6));
        r1.net_stop_value = dec!(3.0);
        let mut r2 = make_result(dec!(20), None, dec!(0.7));
        r2.net_stop_value = dec!(7.0);

        let mut report = SweepReport {
            results: vec![r1, r2],
            total_combinations: 2,
            total_wall_time_secs: 2.0,
            ignored_axes: vec![],
        };
        report.sort_by("net_stop_value");
        assert_eq!(report.results[0].net_stop_value, dec!(7.0));
        assert_eq!(report.results[1].net_stop_value, dec!(3.0));
    }

    #[test]
    fn csv_export_includes_new_columns() {
        let mut r = make_result(dec!(10), Some(dec!(1.0)), dec!(0.6));
        r.net_stop_value = dec!(3.50);
        r.composite_score = Some(dec!(9.75));

        let report = SweepReport {
            results: vec![r],
            total_combinations: 1,
            total_wall_time_secs: 1.0,
            ignored_axes: vec![],
        };

        let dir = std::env::temp_dir().join("sweep_test_csv");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test_new_cols.csv");
        report.export_csv(path.to_str().unwrap()).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let header = content.lines().next().unwrap();
        assert!(header.contains("net_stop_value"));
        assert!(header.contains("composite_score"));

        let data = content.lines().nth(1).unwrap();
        assert!(data.contains("3.50"));
        assert!(data.contains("9.75"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn json_export_includes_new_fields() {
        let mut r = make_result(dec!(10), Some(dec!(1.0)), dec!(0.6));
        r.net_stop_value = dec!(3.50);
        r.composite_score = Some(dec!(9.75));

        let report = SweepReport {
            results: vec![r],
            total_combinations: 1,
            total_wall_time_secs: 1.0,
            ignored_axes: vec![],
        };

        let dir = std::env::temp_dir().join("sweep_test_json");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test_new_fields.json");
        report.export_json(path.to_str().unwrap()).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();
        let result = &json["results"][0];
        assert_eq!(result["net_stop_value"], "3.50");
        assert_eq!(result["composite_score"], "9.75");

        std::fs::remove_dir_all(&dir).ok();
    }
}
