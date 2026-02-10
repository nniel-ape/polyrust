use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};

use polyrust_strategies::{ArbitrageConfig, CryptoArbBase, ReferenceQualityLevel, TailEndStrategy};

use crate::config::BacktestConfig;
use crate::data::store::HistoricalDataStore;
use crate::engine::{BacktestEngine, HistoricalEvent};
use crate::error::{BacktestError, BacktestResult};
use crate::report::BacktestReport;

use super::config::SweepConfig;
use super::grid::ParameterGrid;
use super::report::{SweepReport, SweepResult};

/// Orchestrates parallel parameter sweep across many backtest runs.
pub struct SweepRunner {
    sweep_config: SweepConfig,
    backtest_config: BacktestConfig,
    arb_config: ArbitrageConfig,
    data_store: Arc<HistoricalDataStore>,
}

impl SweepRunner {
    pub fn new(
        sweep_config: SweepConfig,
        backtest_config: BacktestConfig,
        arb_config: ArbitrageConfig,
        data_store: Arc<HistoricalDataStore>,
    ) -> Self {
        Self {
            sweep_config,
            backtest_config,
            arb_config,
            data_store,
        }
    }

    /// Run the parameter sweep.
    pub async fn run(&self) -> BacktestResult<SweepReport> {
        let wall_start = Instant::now();

        // Generate parameter combinations
        let grid = ParameterGrid::from_config(&self.sweep_config);
        let combinations = grid.combinations();
        let total = combinations.len();

        info!(total_combinations = total, "Generated parameter grid");

        // Safeguards
        let force = self.sweep_config.force.unwrap_or(false);
        if total > 50_000 && !force {
            return Err(BacktestError::Config(format!(
                "Too many combinations ({total}). Set force = true in [backtest.sweep] to override."
            )));
        }
        if total > 5_000 && !force {
            warn!(
                total,
                "Large sweep: {total} combinations. Consider reducing parameter ranges."
            );
        }

        // Pre-load events ONCE
        info!("Pre-loading historical events (shared across all runs)...");
        let mut loader_engine = BacktestEngine::new_without_store(
            self.backtest_config.clone(),
            // Dummy strategy just for loading events — won't be used for replay
            Box::new(NoOpStrategy),
            Arc::clone(&self.data_store),
        )
        .await;
        let events = Arc::new(
            loader_engine
                .load_events()
                .await
                .map_err(|e| BacktestError::Engine(e.to_string()))?,
        );
        info!(event_count = events.len(), "Events pre-loaded");

        // Determine parallelism
        let parallelism = self
            .sweep_config
            .parallelism
            .unwrap_or_else(num_cpus::get)
            .max(1);
        info!(parallelism, "Starting sweep with bounded parallelism");

        let mut results: Vec<SweepResult> = Vec::with_capacity(total);
        let mut join_set: tokio::task::JoinSet<BacktestResult<SweepResult>> =
            tokio::task::JoinSet::new();
        let mut completed = 0usize;
        let mut first_duration: Option<f64> = None;

        for combo in combinations {
            // Bounded parallelism: wait for a slot
            while join_set.len() >= parallelism {
                if let Some(result) = join_set.join_next().await {
                    match result {
                        Ok(Ok(sweep_result)) => {
                            completed += 1;
                            if first_duration.is_none() {
                                first_duration = Some(sweep_result.duration_secs);
                                let eta_secs = sweep_result.duration_secs * (total - 1) as f64
                                    / parallelism as f64;
                                info!(
                                    first_run_secs = format!("{:.1}", sweep_result.duration_secs),
                                    eta_secs = format!("{:.0}", eta_secs),
                                    "First run complete. Estimated remaining time: {:.0}s",
                                    eta_secs
                                );
                            }
                            log_progress(completed, total, &sweep_result);
                            results.push(sweep_result);
                        }
                        Ok(Err(e)) => {
                            completed += 1;
                            warn!(error = %e, "Sweep run failed, skipping");
                        }
                        Err(e) => {
                            completed += 1;
                            warn!(error = %e, "Sweep task panicked, skipping");
                        }
                    }
                }
            }

            // Spawn this combination
            let events = Arc::clone(&events);
            let backtest_config = self.backtest_config.clone();
            let mut arb_config = self.arb_config.clone();
            let data_store = Arc::clone(&self.data_store);
            let params_map = combo.params_map();

            // Apply sweep params to config
            combo.apply_to(&mut arb_config);

            // Backtest can't produce Historical quality
            arb_config.tailend.min_reference_quality = ReferenceQualityLevel::Current;

            let combo_index = combo.index;

            join_set.spawn(async move {
                run_single(
                    combo_index,
                    params_map,
                    events,
                    backtest_config,
                    arb_config,
                    data_store,
                )
                .await
            });
        }

        // Drain remaining tasks
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok(sweep_result)) => {
                    completed += 1;
                    if first_duration.is_none() {
                        first_duration = Some(sweep_result.duration_secs);
                    }
                    log_progress(completed, total, &sweep_result);
                    results.push(sweep_result);
                }
                Ok(Err(e)) => {
                    completed += 1;
                    warn!(error = %e, "Sweep run failed, skipping");
                }
                Err(e) => {
                    completed += 1;
                    warn!(error = %e, "Sweep task panicked, skipping");
                }
            }
        }

        let total_wall_time_secs = wall_start.elapsed().as_secs_f64();
        info!(
            completed = results.len(),
            total,
            wall_time_secs = format!("{:.1}", total_wall_time_secs),
            "Sweep complete"
        );

        Ok(SweepReport {
            results,
            total_combinations: total,
            total_wall_time_secs,
        })
    }
}

/// Run a single backtest combination.
async fn run_single(
    combo_index: usize,
    params_map: std::collections::BTreeMap<String, String>,
    events: Arc<Vec<HistoricalEvent>>,
    backtest_config: BacktestConfig,
    arb_config: ArbitrageConfig,
    data_store: Arc<HistoricalDataStore>,
) -> BacktestResult<SweepResult> {
    let run_start = Instant::now();

    // Create fresh strategy
    let base = Arc::new(CryptoArbBase::new(arb_config, vec![]));
    let strategy: Box<dyn polyrust_core::strategy::Strategy> =
        Box::new(TailEndStrategy::new(base));

    // Create engine without Store (no SQLite overhead)
    let start_balance = backtest_config.initial_balance;
    let start_time = backtest_config.start_date;
    let end_time = backtest_config.end_date;

    let mut engine =
        BacktestEngine::new_without_store(backtest_config, strategy, data_store).await;

    // Call on_start before replay
    engine
        .strategy_on_start()
        .await
        .map_err(|e| BacktestError::Strategy(e.to_string()))?;

    // Run with shared events
    let trades = engine
        .run_with_events(&events)
        .await
        .map_err(|e| BacktestError::Engine(e.to_string()))?;

    // Compute report directly from trades (no SQLite)
    let report = BacktestReport::from_trades(trades, start_balance, start_time, end_time);

    let duration_secs = run_start.elapsed().as_secs_f64();

    Ok(SweepResult {
        combination_index: combo_index,
        params: params_map,
        total_pnl: report.total_pnl,
        sharpe_ratio: report.sharpe_ratio,
        win_rate: report.win_rate,
        max_drawdown: report.max_drawdown,
        total_trades: report.total_trades,
        closing_trades: report.closing_trades,
        end_balance: report.end_balance,
        duration_secs,
    })
}

fn log_progress(completed: usize, total: usize, result: &SweepResult) {
    let pct = (completed as f64 / total as f64) * 100.0;
    info!(
        progress = format!("[{}/{}] {:.0}%", completed, total, pct),
        pnl = %result.total_pnl,
        sharpe = result.sharpe_ratio.map_or("N/A".to_string(), |s| format!("{:.4}", s)),
        "Sweep run complete"
    );
}

/// No-op strategy used only for event loading.
struct NoOpStrategy;

#[async_trait::async_trait]
impl polyrust_core::strategy::Strategy for NoOpStrategy {
    fn name(&self) -> &str {
        "noop-loader"
    }

    fn description(&self) -> &str {
        "No-op strategy for event loading"
    }

    async fn on_event(
        &mut self,
        _event: &polyrust_core::events::Event,
        _ctx: &polyrust_core::context::StrategyContext,
    ) -> polyrust_core::error::Result<Vec<polyrust_core::actions::Action>> {
        Ok(vec![])
    }
}
