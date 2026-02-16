use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
    pub opening_trades: usize,
    pub closing_trades: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub expired_worthless: usize,
    pub strategy_exits: usize,
    pub strategy_wins: usize,
    pub strategy_losses: usize,
    pub settled_trades: usize,
    pub settled_wins: usize,
    pub settled_worthless: usize,
    pub force_closed_trades: usize,
    pub force_closed_wins: usize,
    pub force_closed_worthless: usize,
    pub markets_traded: usize,
    // --- Prediction accuracy ---
    pub prediction_correct: usize,
    pub prediction_wrong: usize,
    pub prediction_unknown: usize,
    pub prediction_accuracy: Decimal,
    // --- Stop-loss analysis (strategy exits only) ---
    pub premature_exits: usize,
    pub correct_stops: usize,
    pub premature_exit_cost: Decimal,
    pub correct_stop_savings: Decimal,
    // --- Per-trigger breakdown (keyed by trigger short_name) ---
    pub premature_by_trigger: HashMap<String, usize>,
    pub correct_by_trigger: HashMap<String, usize>,
    pub premature_cost_by_trigger: HashMap<String, Decimal>,
    pub correct_savings_by_trigger: HashMap<String, Decimal>,
    // --- Hedge analysis ---
    pub hedge_attempts: usize,
    pub hedge_pnl: Decimal,
    // --- Re-entry tracking ---
    pub reentry_count: usize,
    pub start_balance: Decimal,
    pub end_balance: Decimal,
    pub duration: Duration,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
}

pub use crate::engine::CloseReason;

/// Intermediate struct for prediction accuracy computation.
struct PredictionMetrics {
    correct: usize,
    wrong: usize,
    unknown: usize,
    accuracy: Decimal,
    premature_exits: usize,
    correct_stops: usize,
    premature_exit_cost: Decimal,
    correct_stop_savings: Decimal,
    premature_by_trigger: HashMap<String, usize>,
    correct_by_trigger: HashMap<String, usize>,
    premature_cost_by_trigger: HashMap<String, Decimal>,
    correct_savings_by_trigger: HashMap<String, Decimal>,
}

/// A trade record from the backtest with P&L information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestTrade {
    pub timestamp: DateTime<Utc>,
    pub token_id: String,
    pub market_id: String,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub realized_pnl: Option<Decimal>,
    /// None for buys, Some(reason) for sells
    pub close_reason: Option<CloseReason>,
    /// Entry price at time of sell (for counterfactual analysis)
    pub entry_price: Option<Decimal>,
    /// What the market settled at ($0 or $1), for counterfactual analysis.
    /// Some for strategy exits (filled from settlement_outcomes), None for buys.
    pub counterfactual_settlement: Option<Decimal>,
    /// Whether this trade was a proactive hedge (opposite-side buy during stop-loss exit)
    pub is_hedge: bool,
    /// Which stop-loss trigger caused this exit (None for buys, settlements, hedges)
    pub exit_trigger: Option<String>,
}

impl BacktestReport {
    /// Create a backtest report from engine results.
    ///
    /// `engine_trades` are the trades returned by `BacktestEngine::run()` which
    /// carry `close_reason` metadata not stored in the DB.
    pub async fn from_engine_results(
        store: Arc<Store>,
        engine_trades: Vec<crate::engine::BacktestTrade>,
        settlement_outcomes: &HashMap<String, Option<Decimal>>,
        start_balance: Decimal,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
    ) -> BacktestResult<Self> {
        // Query all trades from Store (canonical source for prices/pnl)
        let stored_trades = store
            .list_trades(None, 10000)
            .await
            .map_err(|e| crate::error::BacktestError::Database(e.to_string()))?;

        // Build a lookup for close_reason + extra fields from engine trades
        // Key: "token_id|side|timestamp" to avoid Hash requirement on OrderSide
        struct EngineMeta {
            close_reason: Option<CloseReason>,
            market_id: String,
            entry_price: Option<Decimal>,
            is_hedge: bool,
            exit_trigger: Option<String>,
        }
        let mut meta_lookup: HashMap<String, EngineMeta> = HashMap::new();
        for et in &engine_trades {
            let key = format!("{}|{:?}|{}", et.token_id, et.side, et.timestamp.timestamp());
            meta_lookup.insert(
                key,
                EngineMeta {
                    close_reason: et.close_reason,
                    market_id: et.market_id.clone(),
                    entry_price: et.entry_price,
                    is_hedge: et.is_hedge,
                    exit_trigger: et.exit_trigger.clone(),
                },
            );
        }

        // Convert to BacktestTrade format
        let trades: Vec<BacktestTrade> = stored_trades
            .iter()
            .map(|t| {
                let key = format!("{}|{:?}|{}", t.token_id, t.side, t.timestamp.timestamp());
                let meta = meta_lookup.get(&key);
                let close_reason = meta.and_then(|m| m.close_reason);
                let market_id = meta.map(|m| m.market_id.clone()).unwrap_or_default();
                let entry_price = meta.and_then(|m| m.entry_price);

                // Counterfactual settlement: for strategy exits, look up what market settled at
                // settlement_outcomes values are Option<Decimal>: None = ambiguous
                let counterfactual_settlement = match close_reason {
                    Some(CloseReason::Strategy) => {
                        settlement_outcomes.get(&t.token_id).copied().flatten()
                    }
                    Some(CloseReason::Settlement | CloseReason::ForceClose) => {
                        Some(t.price) // Already the settlement price
                    }
                    _ => None,
                };

                let is_hedge = meta.map(|m| m.is_hedge).unwrap_or(false);
                let exit_trigger = meta.and_then(|m| m.exit_trigger.clone());

                BacktestTrade {
                    timestamp: t.timestamp,
                    token_id: t.token_id.clone(),
                    market_id,
                    side: t.side,
                    price: t.price,
                    size: t.size,
                    realized_pnl: t.realized_pnl,
                    close_reason,
                    entry_price,
                    counterfactual_settlement,
                    is_hedge,
                    exit_trigger,
                }
            })
            .collect();

        // Compute hedge metrics
        let (hedge_attempts, hedge_pnl) = Self::compute_hedge_metrics(&trades);

        // Compute realized P&L (sum of all closing trades)
        let realized_pnl: Decimal = trades.iter().filter_map(|t| t.realized_pnl).sum();

        // For now, we don't track unrealized P&L in backtest (all positions are closed)
        let unrealized_pnl = Decimal::ZERO;

        let total_pnl = realized_pnl + unrealized_pnl;

        // Compute trade breakdown
        let total_trades = trades.len();
        let opening_trades = trades.iter().filter(|t| t.side == OrderSide::Buy).count();
        let closing_trades_list: Vec<_> = trades
            .iter()
            .filter(|t| t.side == OrderSide::Sell)
            .collect();

        let closing_trades = closing_trades_list.len();
        let winning_trades = closing_trades_list
            .iter()
            .filter(|t| t.realized_pnl.unwrap_or(Decimal::ZERO) > Decimal::ZERO)
            .count();
        let losing_trades = closing_trades_list
            .iter()
            .filter(|t| t.realized_pnl.unwrap_or(Decimal::ZERO) < Decimal::ZERO)
            .count();
        // Expired worthless = sell at price $0 (total loss on loser token)
        let expired_worthless = closing_trades_list
            .iter()
            .filter(|t| t.price == Decimal::ZERO)
            .count();

        // Per close-reason breakdown
        let is_win = |t: &&BacktestTrade| t.realized_pnl.unwrap_or(Decimal::ZERO) > Decimal::ZERO;
        let is_worthless = |t: &&BacktestTrade| t.price == Decimal::ZERO;

        let strategy_list: Vec<_> = closing_trades_list
            .iter()
            .filter(|t| t.close_reason == Some(CloseReason::Strategy))
            .collect();
        let strategy_exits = strategy_list.len();
        let strategy_wins = strategy_list.iter().filter(|t| is_win(t)).count();
        let strategy_losses = strategy_list
            .iter()
            .filter(|t| t.realized_pnl.unwrap_or(Decimal::ZERO) < Decimal::ZERO)
            .count();

        let settled_list: Vec<_> = closing_trades_list
            .iter()
            .filter(|t| t.close_reason == Some(CloseReason::Settlement))
            .collect();
        let settled_trades = settled_list.len();
        let settled_wins = settled_list.iter().filter(|t| is_win(t)).count();
        let settled_worthless = settled_list.iter().filter(|t| is_worthless(t)).count();

        let force_list: Vec<_> = closing_trades_list
            .iter()
            .filter(|t| t.close_reason == Some(CloseReason::ForceClose))
            .collect();
        let force_closed_trades = force_list.len();
        let force_closed_wins = force_list.iter().filter(|t| is_win(t)).count();
        let force_closed_worthless = force_list.iter().filter(|t| is_worthless(t)).count();

        // Count unique token_ids traded
        let markets_traded = trades
            .iter()
            .map(|t| &t.token_id)
            .collect::<HashSet<_>>()
            .len();

        let win_rate = if closing_trades > 0 {
            Decimal::from(winning_trades as u64) / Decimal::from(closing_trades as u64)
        } else {
            Decimal::ZERO
        };

        // Compute prediction + stop-loss metrics
        let prediction = Self::compute_prediction_metrics(&closing_trades_list);

        // Compute max drawdown
        let max_drawdown = Self::compute_max_drawdown(&trades, start_balance);

        // Compute Sharpe ratio (if enough data)
        let sharpe_ratio = Self::compute_sharpe_ratio(&trades);

        // Compute end balance
        let end_balance = start_balance + total_pnl;

        // Re-entry count
        let reentry_count = Self::compute_reentry_count(&trades);

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
            opening_trades,
            closing_trades,
            winning_trades,
            losing_trades,
            expired_worthless,
            strategy_exits,
            strategy_wins,
            strategy_losses,
            settled_trades,
            settled_wins,
            settled_worthless,
            force_closed_trades,
            force_closed_wins,
            force_closed_worthless,
            markets_traded,
            prediction_correct: prediction.correct,
            prediction_wrong: prediction.wrong,
            prediction_unknown: prediction.unknown,
            prediction_accuracy: prediction.accuracy,
            premature_exits: prediction.premature_exits,
            correct_stops: prediction.correct_stops,
            premature_exit_cost: prediction.premature_exit_cost,
            correct_stop_savings: prediction.correct_stop_savings,
            premature_by_trigger: prediction.premature_by_trigger,
            correct_by_trigger: prediction.correct_by_trigger,
            premature_cost_by_trigger: prediction.premature_cost_by_trigger,
            correct_savings_by_trigger: prediction.correct_savings_by_trigger,
            hedge_attempts,
            hedge_pnl,
            reentry_count,
            start_balance,
            end_balance,
            duration,
            start_time,
            end_time,
        })
    }

    /// Create a backtest report directly from engine trades (no SQLite).
    ///
    /// Used in sweep mode to avoid per-run Store overhead. The engine's
    /// returned `Vec<BacktestTrade>` already carries close_reason and realized_pnl.
    pub fn from_trades(
        engine_trades: Vec<crate::engine::BacktestTrade>,
        settlement_outcomes: &HashMap<String, Option<Decimal>>,
        start_balance: Decimal,
        start_time: DateTime<Utc>,
        end_time: DateTime<Utc>,
    ) -> Self {
        // Convert engine trades to report trades
        let trades: Vec<BacktestTrade> = engine_trades
            .into_iter()
            .map(|t| {
                let counterfactual_settlement = match t.close_reason {
                    Some(CloseReason::Strategy) => {
                        settlement_outcomes.get(&t.token_id).copied().flatten()
                    }
                    Some(CloseReason::Settlement | CloseReason::ForceClose) => {
                        Some(t.price) // Already the settlement price
                    }
                    _ => None,
                };
                BacktestTrade {
                    timestamp: t.timestamp,
                    token_id: t.token_id,
                    market_id: t.market_id,
                    side: t.side,
                    price: t.price,
                    size: t.size,
                    realized_pnl: t.realized_pnl,
                    close_reason: t.close_reason,
                    entry_price: t.entry_price,
                    counterfactual_settlement,
                    is_hedge: t.is_hedge,
                    exit_trigger: t.exit_trigger,
                }
            })
            .collect();

        // Compute hedge metrics
        let (hedge_attempts, hedge_pnl) = Self::compute_hedge_metrics(&trades);

        // Compute realized P&L
        let realized_pnl: Decimal = trades.iter().filter_map(|t| t.realized_pnl).sum();
        let unrealized_pnl = Decimal::ZERO;
        let total_pnl = realized_pnl + unrealized_pnl;

        // Trade breakdown
        let total_trades = trades.len();
        let opening_trades = trades.iter().filter(|t| t.side == OrderSide::Buy).count();
        let closing_trades_list: Vec<_> = trades
            .iter()
            .filter(|t| t.side == OrderSide::Sell)
            .collect();

        let closing_trades = closing_trades_list.len();
        let winning_trades = closing_trades_list
            .iter()
            .filter(|t| t.realized_pnl.unwrap_or(Decimal::ZERO) > Decimal::ZERO)
            .count();
        let losing_trades = closing_trades_list
            .iter()
            .filter(|t| t.realized_pnl.unwrap_or(Decimal::ZERO) < Decimal::ZERO)
            .count();
        let expired_worthless = closing_trades_list
            .iter()
            .filter(|t| t.price == Decimal::ZERO)
            .count();

        // Per close-reason breakdown
        let is_win = |t: &&BacktestTrade| t.realized_pnl.unwrap_or(Decimal::ZERO) > Decimal::ZERO;
        let is_worthless = |t: &&BacktestTrade| t.price == Decimal::ZERO;

        let strategy_list: Vec<_> = closing_trades_list
            .iter()
            .filter(|t| t.close_reason == Some(CloseReason::Strategy))
            .collect();
        let strategy_exits = strategy_list.len();
        let strategy_wins = strategy_list.iter().filter(|t| is_win(t)).count();
        let strategy_losses = strategy_list
            .iter()
            .filter(|t| t.realized_pnl.unwrap_or(Decimal::ZERO) < Decimal::ZERO)
            .count();

        let settled_list: Vec<_> = closing_trades_list
            .iter()
            .filter(|t| t.close_reason == Some(CloseReason::Settlement))
            .collect();
        let settled_trades = settled_list.len();
        let settled_wins = settled_list.iter().filter(|t| is_win(t)).count();
        let settled_worthless = settled_list.iter().filter(|t| is_worthless(t)).count();

        let force_list: Vec<_> = closing_trades_list
            .iter()
            .filter(|t| t.close_reason == Some(CloseReason::ForceClose))
            .collect();
        let force_closed_trades = force_list.len();
        let force_closed_wins = force_list.iter().filter(|t| is_win(t)).count();
        let force_closed_worthless = force_list.iter().filter(|t| is_worthless(t)).count();

        let markets_traded = trades
            .iter()
            .map(|t| &t.token_id)
            .collect::<HashSet<_>>()
            .len();

        let win_rate = if closing_trades > 0 {
            Decimal::from(winning_trades as u64) / Decimal::from(closing_trades as u64)
        } else {
            Decimal::ZERO
        };

        // Compute prediction + stop-loss metrics
        let prediction = Self::compute_prediction_metrics(&closing_trades_list);

        let max_drawdown = Self::compute_max_drawdown(&trades, start_balance);
        let sharpe_ratio = Self::compute_sharpe_ratio(&trades);
        let end_balance = start_balance + total_pnl;
        let reentry_count = Self::compute_reentry_count(&trades);
        let duration = end_time - start_time;

        BacktestReport {
            trades,
            total_pnl,
            realized_pnl,
            unrealized_pnl,
            win_rate,
            max_drawdown,
            sharpe_ratio,
            total_trades,
            opening_trades,
            closing_trades,
            winning_trades,
            losing_trades,
            expired_worthless,
            strategy_exits,
            strategy_wins,
            strategy_losses,
            settled_trades,
            settled_wins,
            settled_worthless,
            force_closed_trades,
            force_closed_wins,
            force_closed_worthless,
            markets_traded,
            prediction_correct: prediction.correct,
            prediction_wrong: prediction.wrong,
            prediction_unknown: prediction.unknown,
            prediction_accuracy: prediction.accuracy,
            premature_exits: prediction.premature_exits,
            correct_stops: prediction.correct_stops,
            premature_exit_cost: prediction.premature_exit_cost,
            correct_stop_savings: prediction.correct_stop_savings,
            premature_by_trigger: prediction.premature_by_trigger,
            correct_by_trigger: prediction.correct_by_trigger,
            premature_cost_by_trigger: prediction.premature_cost_by_trigger,
            correct_savings_by_trigger: prediction.correct_savings_by_trigger,
            hedge_attempts,
            hedge_pnl,
            reentry_count,
            start_balance,
            end_balance,
            duration,
            start_time,
            end_time,
        }
    }

    /// Compute prediction accuracy and stop-loss analysis from closing trades.
    fn compute_prediction_metrics(closing_trades: &[&BacktestTrade]) -> PredictionMetrics {
        let mut correct = 0usize;
        let mut wrong = 0usize;
        let mut unknown = 0usize;
        let mut premature_exits = 0usize;
        let mut correct_stops = 0usize;
        let mut premature_exit_cost = Decimal::ZERO;
        let mut correct_stop_savings = Decimal::ZERO;
        let mut premature_by_trigger: HashMap<String, usize> = HashMap::new();
        let mut correct_by_trigger: HashMap<String, usize> = HashMap::new();
        let mut premature_cost_by_trigger: HashMap<String, Decimal> = HashMap::new();
        let mut correct_savings_by_trigger: HashMap<String, Decimal> = HashMap::new();

        for t in closing_trades {
            match t.counterfactual_settlement {
                Some(sp) if sp == Decimal::ONE => {
                    correct += 1;
                    // If strategy exited early on a correct prediction → premature exit
                    if t.close_reason == Some(CloseReason::Strategy) {
                        premature_exits += 1;
                        let trigger_key = t
                            .exit_trigger
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string());
                        *premature_by_trigger.entry(trigger_key.clone()).or_default() += 1;
                        // Opportunity cost: what we would have earned at $1 minus what we actually earned
                        if let (Some(entry), Some(pnl)) = (t.entry_price, t.realized_pnl) {
                            let counterfactual_profit = (Decimal::ONE - entry) * t.size;
                            let cost = counterfactual_profit - pnl;
                            premature_exit_cost += cost;
                            *premature_cost_by_trigger.entry(trigger_key).or_default() += cost;
                        }
                    }
                }
                Some(sp) if sp == Decimal::ZERO => {
                    wrong += 1;
                    // If strategy exited early on a wrong prediction → correct stop
                    if t.close_reason == Some(CloseReason::Strategy) {
                        correct_stops += 1;
                        let trigger_key = t
                            .exit_trigger
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string());
                        *correct_by_trigger.entry(trigger_key.clone()).or_default() += 1;
                        // Savings: loss we would have had at $0 minus actual loss
                        if let (Some(entry), Some(pnl)) = (t.entry_price, t.realized_pnl) {
                            let counterfactual_loss = entry * t.size;
                            let savings = counterfactual_loss - pnl.abs();
                            correct_stop_savings += savings;
                            *correct_savings_by_trigger.entry(trigger_key).or_default() += savings;
                        }
                    }
                }
                _ => {
                    unknown += 1;
                }
            }
        }

        let total_known = correct + wrong;
        let accuracy = if total_known > 0 {
            Decimal::from(correct as u64) / Decimal::from(total_known as u64)
        } else {
            Decimal::ZERO
        };

        PredictionMetrics {
            correct,
            wrong,
            unknown,
            accuracy,
            premature_exits,
            correct_stops,
            premature_exit_cost,
            correct_stop_savings,
            premature_by_trigger,
            correct_by_trigger,
            premature_cost_by_trigger,
            correct_savings_by_trigger,
        }
    }

    /// Compute hedge metrics: (hedge_attempts, hedge_pnl).
    ///
    /// hedge_attempts = count of Buy trades with is_hedge=true
    /// hedge_pnl = sum of realized_pnl from sells that close hedge positions
    /// (identified by token_id matching a hedge buy)
    fn compute_hedge_metrics(trades: &[BacktestTrade]) -> (usize, Decimal) {
        let mut hedge_token_ids: HashSet<&str> = HashSet::new();
        let mut hedge_attempts = 0usize;

        // First pass: collect token_ids from hedge buys
        for t in trades {
            if t.is_hedge && t.side == OrderSide::Buy {
                hedge_attempts += 1;
                hedge_token_ids.insert(&t.token_id);
            }
        }

        // Second pass: sum realized_pnl from sells closing hedge positions
        let hedge_pnl: Decimal = trades
            .iter()
            .filter(|t| t.side == OrderSide::Sell && hedge_token_ids.contains(t.token_id.as_str()))
            .filter_map(|t| t.realized_pnl)
            .sum();

        (hedge_attempts, hedge_pnl)
    }

    /// Count tokens that were entered more than once (re-entries after stop-loss).
    fn compute_reentry_count(trades: &[BacktestTrade]) -> usize {
        let mut buy_counts: HashMap<&str, usize> = HashMap::new();
        for t in trades {
            if t.side == OrderSide::Buy {
                *buy_counts.entry(&t.token_id).or_default() += 1;
            }
        }
        buy_counts.values().filter(|&&count| count > 1).count()
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
        let pnls: Vec<Decimal> = trades.iter().filter_map(|t| t.realized_pnl).collect();

        if pnls.len() < 2 {
            return None;
        }

        // Compute mean return
        let mean: Decimal =
            pnls.iter().copied().sum::<Decimal>() / Decimal::from(pnls.len() as u64);

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
        s.push_str(&format!(
            "Period: {} to {}\n",
            self.start_time.format("%Y-%m-%d %H:%M:%S"),
            self.end_time.format("%Y-%m-%d %H:%M:%S")
        ));
        s.push_str(&format!("Duration: {} hours\n", self.duration.num_hours()));
        s.push('\n');

        s.push_str("--- Balance ---\n");
        s.push_str(&format!("Start balance: ${:.2}\n", self.start_balance));
        s.push_str(&format!("End balance:   ${:.2}\n", self.end_balance));

        let pnl_pct = if self.start_balance > Decimal::ZERO {
            format!(
                "{:.2}%",
                self.total_pnl / self.start_balance * Decimal::from(100)
            )
        } else {
            "N/A".to_string()
        };
        s.push_str(&format!(
            "Total P&L:     ${:.2} ({})\n",
            self.total_pnl, pnl_pct
        ));
        s.push_str(&format!("Realized P&L:  ${:.2}\n", self.realized_pnl));
        s.push_str(&format!("Unrealized P&L: ${:.2}\n", self.unrealized_pnl));
        s.push('\n');

        s.push_str("--- Trade Statistics ---\n");
        s.push_str(&format!(
            "Total orders:     {}  ({} buys, {} sells)\n",
            self.total_trades, self.opening_trades, self.closing_trades
        ));
        s.push_str(&format!(
            "Closing trades:   {}  ({} wins, {} losses, {} expired worthless)\n",
            self.closing_trades, self.winning_trades, self.losing_trades, self.expired_worthless
        ));
        if self.strategy_exits > 0 {
            s.push_str(&format!(
                "  Strategy exits: {}  ({} wins, {} losses)\n",
                self.strategy_exits, self.strategy_wins, self.strategy_losses
            ));
        }
        if self.settled_trades > 0 {
            s.push_str(&format!(
                "  Settled (expiry): {}  ({} wins, {} expired worthless)\n",
                self.settled_trades, self.settled_wins, self.settled_worthless
            ));
        }
        if self.force_closed_trades > 0 {
            s.push_str(&format!(
                "  Force-closed:   {}  ({} wins, {} expired worthless)\n",
                self.force_closed_trades, self.force_closed_wins, self.force_closed_worthless
            ));
        }
        s.push_str(&format!(
            "Win rate:         {:.2}%\n",
            self.win_rate * Decimal::from(100)
        ));
        s.push_str(&format!("Markets traded:   {}\n", self.markets_traded));
        s.push('\n');

        s.push_str("--- Prediction Accuracy ---\n");
        let total_known = self.prediction_correct + self.prediction_wrong;
        if total_known > 0 {
            s.push_str(&format!(
                "Prediction correct:  {}/{} ({:.2}%)\n",
                self.prediction_correct,
                total_known,
                self.prediction_accuracy * Decimal::from(100)
            ));
            s.push_str(&format!(
                "Prediction wrong:    {}/{} ({:.2}%)\n",
                self.prediction_wrong,
                total_known,
                (Decimal::ONE - self.prediction_accuracy) * Decimal::from(100)
            ));
        }
        if self.prediction_unknown > 0 {
            s.push_str(&format!(
                "Prediction unknown:  {} (no settlement data)\n",
                self.prediction_unknown
            ));
        }
        s.push('\n');

        s.push_str("--- Stop-Loss Analysis ---\n");
        let total_sl = self.premature_exits + self.correct_stops;
        if total_sl > 0 {
            s.push_str(&format!(
                "Strategy exits:      {}  ({} premature, {} correct stops)\n",
                total_sl, self.premature_exits, self.correct_stops
            ));
            s.push_str(&format!(
                "Premature exit cost: ${:.2}  (correct prediction, exited early)\n",
                self.premature_exit_cost
            ));
            s.push_str(&format!(
                "Correct stop savings: ${:.2}  (wrong prediction, saved by stop)\n",
                self.correct_stop_savings
            ));
            let net_value = self.correct_stop_savings - self.premature_exit_cost;
            s.push_str(&format!("Net stop-loss value: ${:.2}\n", net_value));

            // Per-trigger breakdown
            let mut all_triggers: std::collections::BTreeSet<&str> =
                std::collections::BTreeSet::new();
            for k in self.premature_by_trigger.keys() {
                all_triggers.insert(k.as_str());
            }
            for k in self.correct_by_trigger.keys() {
                all_triggers.insert(k.as_str());
            }
            if !all_triggers.is_empty() {
                s.push_str("  By trigger:\n");
                for trigger in &all_triggers {
                    let prem = self
                        .premature_by_trigger
                        .get(*trigger)
                        .copied()
                        .unwrap_or(0);
                    let corr = self.correct_by_trigger.get(*trigger).copied().unwrap_or(0);
                    let prem_cost = self
                        .premature_cost_by_trigger
                        .get(*trigger)
                        .copied()
                        .unwrap_or(Decimal::ZERO);
                    let corr_sav = self
                        .correct_savings_by_trigger
                        .get(*trigger)
                        .copied()
                        .unwrap_or(Decimal::ZERO);
                    let net = corr_sav - prem_cost;
                    s.push_str(&format!(
                        "    {:<13} {:>2} premature (${:.2} cost), {:>2} correct (${:.2} saved), net ${:.2}\n",
                        trigger, prem, prem_cost, corr, corr_sav, net
                    ));
                }
            }
        } else {
            s.push_str("No strategy exits with settlement data.\n");
        }
        s.push('\n');

        if self.reentry_count > 0 {
            s.push_str("--- Re-Entry ---\n");
            s.push_str(&format!("Tokens re-entered:   {}\n", self.reentry_count));
            s.push('\n');
        }

        if self.hedge_attempts > 0 {
            s.push_str("--- Hedge Analysis ---\n");
            s.push_str(&format!("Hedge attempts:      {}\n", self.hedge_attempts));
            s.push_str(&format!("Hedge P&L:           ${:.2}\n", self.hedge_pnl));
            s.push('\n');
        }

        s.push_str("--- Risk Metrics ---\n");
        s.push_str(&format!(
            "Max drawdown:   {:.2}%\n",
            self.max_drawdown * Decimal::from(100)
        ));
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
                close_reason: None,
                market_id: String::new(),
                entry_price: None,
                counterfactual_settlement: None,
                is_hedge: false,
                exit_trigger: None,
            },
            BacktestTrade {
                timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.5),
                size: dec!(10),
                realized_pnl: Some(dec!(-30)), // Balance: 1010 - 30 = 980 (trough)
                close_reason: None,
                market_id: String::new(),
                entry_price: None,
                counterfactual_settlement: None,
                is_hedge: false,
                exit_trigger: None,
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
                close_reason: None,
                market_id: String::new(),
                entry_price: None,
                counterfactual_settlement: None,
                is_hedge: false,
                exit_trigger: None,
            },
            BacktestTrade {
                timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.7),
                size: dec!(10),
                realized_pnl: Some(dec!(15)),
                close_reason: None,
                market_id: String::new(),
                entry_price: None,
                counterfactual_settlement: None,
                is_hedge: false,
                exit_trigger: None,
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
                close_reason: None,
                market_id: String::new(),
                entry_price: None,
                counterfactual_settlement: None,
                is_hedge: false,
                exit_trigger: None,
            },
            BacktestTrade {
                timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.7),
                size: dec!(10),
                realized_pnl: Some(dec!(15)),
                close_reason: None,
                market_id: String::new(),
                entry_price: None,
                counterfactual_settlement: None,
                is_hedge: false,
                exit_trigger: None,
            },
            BacktestTrade {
                timestamp: DateTime::from_timestamp(3000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.65),
                size: dec!(10),
                realized_pnl: Some(dec!(12)),
                close_reason: None,
                market_id: String::new(),
                entry_price: None,
                counterfactual_settlement: None,
                is_hedge: false,
                exit_trigger: None,
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
        let trades = vec![BacktestTrade {
            timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
            token_id: "token1".to_string(),
            side: OrderSide::Sell,
            price: dec!(0.6),
            size: dec!(10),
            realized_pnl: Some(dec!(10)),
            close_reason: None,
            market_id: String::new(),
            entry_price: None,
            counterfactual_settlement: None,
            is_hedge: false,
            exit_trigger: None,
        }];

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
                close_reason: None,
                market_id: String::new(),
                entry_price: None,
                counterfactual_settlement: None,
                is_hedge: false,
                exit_trigger: None,
            },
            BacktestTrade {
                timestamp: DateTime::from_timestamp(2000, 0).unwrap(),
                token_id: "token1".to_string(),
                side: OrderSide::Sell,
                price: dec!(0.6),
                size: dec!(10),
                realized_pnl: Some(dec!(10)),
                close_reason: None,
                market_id: String::new(),
                entry_price: None,
                counterfactual_settlement: None,
                is_hedge: false,
                exit_trigger: None,
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
            fee: None,
            order_type: None,
            entry_price: None,
            close_reason: None,
            orderbook_snapshot: None,
        };
        store.insert_trade(&trade).await.unwrap();

        let report = BacktestReport::from_engine_results(
            store,
            vec![],
            &HashMap::new(),
            dec!(1000),
            start_time,
            end_time,
        )
        .await
        .unwrap();

        let summary = report.summary();

        // Check key summary components
        assert!(summary.contains("Backtest Report"));
        assert!(summary.contains("Start balance: $1000.00"));
        assert!(summary.contains("End balance:   $1005.50"));
        assert!(summary.contains("Total P&L:     $5.50"));
        assert!(summary.contains("Total orders:     1"));
        assert!(summary.contains("Closing trades:   1"));
        assert!(summary.contains("Win rate:"));
    }

    #[tokio::test]
    async fn backtest_report_json_serialization() {
        let store = Arc::new(Store::new(":memory:").await.unwrap());

        let start_time = DateTime::from_timestamp(1000, 0).unwrap();
        let end_time = DateTime::from_timestamp(5000, 0).unwrap();

        let report = BacktestReport::from_engine_results(
            store,
            vec![],
            &HashMap::new(),
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
