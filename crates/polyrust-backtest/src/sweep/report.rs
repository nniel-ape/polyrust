use std::collections::BTreeMap;
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
    pub duration_secs: f64,
}

/// Aggregated sweep results.
#[derive(Debug, Clone)]
pub struct SweepReport {
    pub results: Vec<SweepResult>,
    pub total_combinations: usize,
    pub total_wall_time_secs: f64,
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
            .map(|n| {
                n.rsplit('.')
                    .next()
                    .unwrap_or(n)
                    .to_string()
            })
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
            results
                .first()
                .map(|_| "metric")
                .unwrap_or("none")
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
        let mut file = std::fs::File::create(path)
            .map_err(|e| crate::error::BacktestError::Engine(format!("Failed to create CSV: {}", e)))?;

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
                result
                    .sharpe_ratio
                    .map_or(String::new(), |s| s.to_string()),
                result.win_rate.to_string(),
                result.max_drawdown.to_string(),
                result.total_trades.to_string(),
                result.closing_trades.to_string(),
                result.end_balance.to_string(),
                format!("{:.2}", result.duration_secs),
            ]);
            writeln!(file, "{}", row.join(","))
                .map_err(|e| crate::error::BacktestError::Engine(format!("CSV write error: {}", e)))?;
        }

        tracing::info!(path, rows = self.results.len(), "Exported sweep results to CSV");
        Ok(())
    }

    /// Export results to JSON file.
    pub fn export_json(&self, path: &str) -> BacktestResult<()> {
        let json = serde_json::json!({
            "total_combinations": self.total_combinations,
            "total_wall_time_secs": self.total_wall_time_secs,
            "results": self.results,
        });

        let file = std::fs::File::create(path)
            .map_err(|e| crate::error::BacktestError::Engine(format!("Failed to create JSON: {}", e)))?;
        serde_json::to_writer_pretty(file, &json)
            .map_err(|e| crate::error::BacktestError::Engine(format!("JSON write error: {}", e)))?;

        tracing::info!(path, rows = self.results.len(), "Exported sweep results to JSON");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn make_result(pnl: Decimal, sharpe: Option<Decimal>, win_rate: Decimal) -> SweepResult {
        SweepResult {
            combination_index: 0,
            params: BTreeMap::new(),
            total_pnl: pnl,
            sharpe_ratio: sharpe,
            win_rate,
            max_drawdown: dec!(0.05),
            total_trades: 10,
            closing_trades: 5,
            end_balance: dec!(1000) + pnl,
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
        };
        report.sort_by("pnl");
        assert_eq!(report.results[0].total_pnl, dec!(30));
        assert_eq!(report.results[2].total_pnl, dec!(10));
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
        };
        report.sort_by("drawdown");
        assert_eq!(report.results[0].max_drawdown, dec!(0.02));
    }
}
