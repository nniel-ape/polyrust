use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use polyrust_core::types::OrderSide;
use polyrust_store::Store;

use crate::error::BacktestResult;

/// Comprehensive backtest performance report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestReport {
    pub trades: Vec<BacktestTrade>,
    pub total_pnl: Decimal,
    pub realized_pnl: Decimal,
    pub unrealized_pnl: Decimal,
    pub win_rate: Decimal,
    pub max_drawdown: Decimal,
    pub sharpe_ratio: Option<Decimal>,
    pub total_trades: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub start_balance: Decimal,
    pub end_balance: Decimal,
    pub duration: Duration,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
}

/// A trade record from the backtest with P&L information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestTrade {
    pub timestamp: DateTime<Utc>,
    pub token_id: String,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub realized_pnl: Option<Decimal>,
}

impl BacktestReport {
    /// Create a backtest report from engine results.
    ///
    /// Extracts trades from the Store, computes all metrics, and returns
    /// a comprehensive report.
    pub async fn from_engine_results(
        store: Arc<Store>,
        start_balance: Decimal,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
    ) -> BacktestResult<Self> {
        // Query all trades from Store
        let stored_trades = store
            .list_trades(None, 10000)
            .await
            .map_err(|e| crate::error::BacktestError::Database(e.to_string()))?;

        // Convert to BacktestTrade format
        let trades: Vec<BacktestTrade> = stored_trades
            .iter()
            .map(|t| BacktestTrade {
                timestamp: t.timestamp,
                token_id: t.token_id.clone(),
                side: t.side,
                price: t.price,
                size: t.size,
                realized_pnl: t.realized_pnl,
            })
            .collect();

        // Compute realized P&L (sum of all closing trades)
        let realized_pnl: Decimal = trades
            .iter()
            .filter_map(|t| t.realized_pnl)
            .sum();

        // For now, we don't track unrealized P&L in backtest (all positions are closed)
        let unrealized_pnl = Decimal::ZERO;

        let total_pnl = realized_pnl + unrealized_pnl;

        // Compute win/loss statistics
        let winning_trades = trades
            .iter()
            .filter(|t| t.realized_pnl.unwrap_or(Decimal::ZERO) > Decimal::ZERO)
            .count();

        let losing_trades = trades
            .iter()
            .filter(|t| t.realized_pnl.unwrap_or(Decimal::ZERO) < Decimal::ZERO)
            .count();

        let total_trades = trades.len();

        let win_rate = if total_trades > 0 {
            Decimal::from(winning_trades as u64) / Decimal::from(total_trades as u64)
        } else {
            Decimal::ZERO
        };

        // Compute max drawdown
        let max_drawdown = Self::compute_max_drawdown(&trades, start_balance);

        // Compute Sharpe ratio (if enough data)
        let sharpe_ratio = Self::compute_sharpe_ratio(&trades);

        // Compute end balance
        let end_balance = start_balance + total_pnl;

        let duration = end_time - start_time;

        Ok(BacktestReport {
            trades,
            total_pnl,
            realized_pnl,
            unrealized_pnl,
            win_rate,
            max_drawdown,
            sharpe_ratio,
            total_trades,
            winning_trades,
            losing_trades,
            start_balance,
            end_balance,
            duration,
            start_time,
            end_time,
        })
    }

    /// Compute maximum drawdown from peak equity.
    ///
    /// Returns the largest percentage decline from a peak balance.
    fn compute_max_drawdown(trades: &[BacktestTrade], start_balance: Decimal) -> Decimal {
        if trades.is_empty() {
            return Decimal::ZERO;
        }

        let mut current_balance = start_balance;
        let mut peak_balance = start_balance;
        let mut max_drawdown = Decimal::ZERO;

        for trade in trades {
            if let Some(pnl) = trade.realized_pnl {
                current_balance += pnl;

                if current_balance > peak_balance {
                    peak_balance = current_balance;
                } else if peak_balance > Decimal::ZERO {
                    let drawdown = (peak_balance - current_balance) / peak_balance;
                    if drawdown > max_drawdown {
                        max_drawdown = drawdown;
                    }
                }
            }
        }

        max_drawdown
    }

    /// Compute Sharpe ratio (risk-adjusted return metric).
    ///
    /// Returns None if insufficient data (<2 trades with P&L).
    fn compute_sharpe_ratio(trades: &[BacktestTrade]) -> Option<Decimal> {
        let pnls: Vec<Decimal> = trades
            .iter()
            .filter_map(|t| t.realized_pnl)
            .collect();

        if pnls.len() < 2 {
            return None;
        }

        // Compute mean return
        let mean: Decimal = pnls.iter().copied().sum::<Decimal>() / Decimal::from(pnls.len() as u64);

        // Compute standard deviation
        let variance: Decimal = pnls
            .iter()
            .map(|&pnl| {
                let diff = pnl - mean;
                diff * diff
            })
            .sum::<Decimal>()
            / Decimal::from((pnls.len() - 1) as u64);

        if variance <= Decimal::ZERO {
            return Some(Decimal::ZERO);
        }

        // Standard deviation (sqrt approximation using Newton's method)
        let std_dev = sqrt_decimal(variance)?;

        if std_dev == Decimal::ZERO {
            return Some(Decimal::ZERO);
        }

        // Sharpe ratio = mean / std_dev (assuming risk-free rate = 0)
        Some(mean / std_dev)
    }

    /// Generate a human-readable terminal summary.
    pub fn summary(&self) -> String {
        let mut s = String::new();

        s.push_str("=== Backtest Report ===\n");
        s.push_str(&format!("Period: {} to {}\n",
            self.start_time.format("%Y-%m-%d %H:%M:%S"),
            self.end_time.format("%Y-%m-%d %H:%M:%S")));
        s.push_str(&format!("Duration: {} hours\n", self.duration.num_hours()));
        s.push('\n');

        s.push_str("--- Balance ---\n");
        s.push_str(&format!("Start balance: ${}\n", self.start_balance));
        s.push_str(&format!("End balance:   ${}\n", self.end_balance));
        s.push_str(&format!("Total P&L:     ${} ({:.2}%)\n",
            self.total_pnl,
            self.total_pnl / self.start_balance * Decimal::from(100)));
        s.push_str(&format!("Realized P&L:  ${}\n", self.realized_pnl));
        s.push_str(&format!("Unrealized P&L: ${}\n", self.unrealized_pnl));
        s.push('\n');

        s.push_str("--- Trade Statistics ---\n");
        s.push_str(&format!("Total trades:   {}\n", self.total_trades));
        s.push_str(&format!("Winning trades: {}\n", self.winning_trades));
        s.push_str(&format!("Losing trades:  {}\n", self.losing_trades));
        s.push_str(&format!("Win rate:       {:.2}%\n", self.win_rate * Decimal::from(100)));
        s.push('\n');

        s.push_str("--- Risk Metrics ---\n");
        s.push_str(&format!("Max drawdown:   {:.2}%\n", self.max_drawdown * Decimal::from(100)));
        if let Some(sharpe) = self.sharpe_ratio {
            s.push_str(&format!("Sharpe ratio:   {:.4}\n", sharpe));
        } else {
            s.push_str("Sharpe ratio:   N/A (insufficient data)\n");
        }

        s
    }

    /// Serialize report to JSON.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }
}

/// Approximate square root using Newton's method for Decimal.
fn sqrt_decimal(x: Decimal) -> Option<Decimal> {
    if x < Decimal::ZERO {
        return None;
    }
    if x == Decimal::ZERO {
        return Some(Decimal::ZERO);
    }

    let two = Decimal::TWO;
    let mut guess = x / two;
    let tolerance = Decimal::new(1, 10); // 0.0000000001

    for _ in 0..50 {
        let next_guess = (guess + x / guess) / two;
        if (next_guess - guess).abs() < tolerance {
            return Some(next_guess);
        }
        guess = next_guess;
    }

    Some(guess)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polyrust_core::types::Trade;
    use rust_decimal_macros::dec;

    #[test]
    fn compute_max_drawdown_with_peak_to_trough() {
        let trades = vec![
            BacktestTrade {
                timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.6),
                size: dec!(10),
                realized_pnl: Some(dec!(10)), // Balance: 1000 + 10 = 1010 (peak)
            },
            BacktestTrade {
                timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.5),
                size: dec!(10),
                realized_pnl: Some(dec!(-30)), // Balance: 1010 - 30 = 980 (trough)
            },
        ];

        let drawdown = BacktestReport::compute_max_drawdown(&trades, dec!(1000));

        // Peak: 1010, Trough: 980, Drawdown: (1010 - 980) / 1010 = 30 / 1010 ≈ 0.0297
        assert!(drawdown > dec!(0.029) && drawdown < dec!(0.030));
    }

    #[test]
    fn compute_max_drawdown_no_drawdown() {
        let trades = vec![
            BacktestTrade {
                timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.6),
                size: dec!(10),
                realized_pnl: Some(dec!(10)),
            },
            BacktestTrade {
                timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.7),
                size: dec!(10),
                realized_pnl: Some(dec!(15)),
            },
        ];

        let drawdown = BacktestReport::compute_max_drawdown(&trades, dec!(1000));
        assert_eq!(drawdown, Decimal::ZERO);
    }

    #[test]
    fn compute_sharpe_ratio_positive_returns() {
        let trades = vec![
            BacktestTrade {
                timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.6),
                size: dec!(10),
                realized_pnl: Some(dec!(10)),
            },
            BacktestTrade {
                timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.7),
                size: dec!(10),
                realized_pnl: Some(dec!(15)),
            },
            BacktestTrade {
                timestamp: DateTime::from_timestamp(3000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.65),
                size: dec!(10),
                realized_pnl: Some(dec!(12)),
            },
        ];

        let sharpe = BacktestReport::compute_sharpe_ratio(&trades);
        assert!(sharpe.is_some());

        // Mean = (10 + 15 + 12) / 3 = 12.33
        // Positive mean, positive std dev -> positive Sharpe ratio
        assert!(sharpe.unwrap() > Decimal::ZERO);
    }

    #[test]
    fn compute_sharpe_ratio_insufficient_data() {
        let trades = vec![
            BacktestTrade {
                timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.6),
                size: dec!(10),
                realized_pnl: Some(dec!(10)),
            },
        ];

        let sharpe = BacktestReport::compute_sharpe_ratio(&trades);
        assert!(sharpe.is_none());
    }

    #[test]
    fn compute_sharpe_ratio_zero_variance() {
        let trades = vec![
            BacktestTrade {
                timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.6),
                size: dec!(10),
                realized_pnl: Some(dec!(10)),
            },
            BacktestTrade {
                timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.6),
                size: dec!(10),
                realized_pnl: Some(dec!(10)),
            },
        ];

        let sharpe = BacktestReport::compute_sharpe_ratio(&trades);
        assert!(sharpe.is_some());
        assert_eq!(sharpe.unwrap(), Decimal::ZERO);
    }

    #[test]
    fn sqrt_decimal_basic_cases() {
        assert_eq!(sqrt_decimal(dec!(0)), Some(dec!(0)));
        assert_eq!(sqrt_decimal(dec!(1)), Some(dec!(1)));

        let sqrt4 = sqrt_decimal(dec!(4)).unwrap();
        assert!(sqrt4 > dec!(1.99) && sqrt4 < dec!(2.01));

        let sqrt9 = sqrt_decimal(dec!(9)).unwrap();
        assert!(sqrt9 > dec!(2.99) && sqrt9 < dec!(3.01));

        assert!(sqrt_decimal(dec!(-1)).is_none());
    }

    #[tokio::test]
    async fn backtest_report_summary_format() {
        let store = Arc::new(Store::new(":memory:").await.unwrap());

        let start_time = DateTime::from_timestamp(1000, 0).unwrap();
        let end_time = DateTime::from_timestamp(5000, 0).unwrap();

        // Insert a test trade
        let trade = Trade {
            id: uuid::Uuid::new_v4(),
            order_id: "order1".to_string(),
            market_id: "market1".to_string(),
            token_id: "token1".to_string(),
            side: OrderSide::Sell,
            price: dec!(0.6),
            size: dec!(10),
            realized_pnl: Some(dec!(5.5)),
            strategy_name: "test-strategy".to_string(),
            timestamp: start_time,
        };
        store.insert_trade(&trade).await.unwrap();

        let report = BacktestReport::from_engine_results(
            store,
            dec!(1000),
            start_time,
            end_time,
        )
        .await
        .unwrap();

        let summary = report.summary();

        // Check key summary components
        assert!(summary.contains("Backtest Report"));
        assert!(summary.contains("Start balance: $1000"));
        assert!(summary.contains("End balance:   $1005.5"));
        assert!(summary.contains("Total P&L:     $5.5"));
        assert!(summary.contains("Total trades:   1"));
        assert!(summary.contains("Win rate:"));
    }

    #[tokio::test]
    async fn backtest_report_json_serialization() {
        let store = Arc::new(Store::new(":memory:").await.unwrap());

        let start_time = DateTime::from_timestamp(1000, 0).unwrap();
        let end_time = DateTime::from_timestamp(5000, 0).unwrap();

        let report = BacktestReport::from_engine_results(
            store,
            dec!(1000),
            start_time,
            end_time,
        )
        .await
        .unwrap();

        let json = report.to_json();

        assert!(json.is_object());
        assert!(json.get("total_pnl").is_some());
        assert!(json.get("win_rate").is_some());
        assert!(json.get("trades").is_some());
    }
}
