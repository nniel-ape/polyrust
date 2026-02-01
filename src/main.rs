use std::sync::Arc;

use chrono::Utc;
use polyrust_backtest::{BacktestConfig, BacktestEngine, DataFetcher, HistoricalDataStore};
use polyrust_core::prelude::*;
use polyrust_dashboard::Dashboard;
use polyrust_execution::{FillMode, LiveBackend, PaperBackend};
use polyrust_market::{ClobFeed, DiscoveryConfig, DiscoveryFeed, MarketDataFeed, PriceFeed};
use polyrust_store::Store;
use polyrust_strategies::{
    ArbitrageConfig, ConfirmedDashboard, ConfirmedStrategy, CrossCorrDashboard, CrossCorrStrategy,
    CryptoArbBase, CryptoArbDashboard, TailEndDashboard, TailEndStrategy, TwoSidedDashboard,
    TwoSidedStrategy,
};
use serde::Deserialize;

/// Wrapper to extract backtest and arbitrage configs from TOML file.
#[derive(Debug, Deserialize, Default)]
struct ConfigWrapper {
    #[serde(default)]
    arbitrage: ArbitrageConfig,
    #[serde(default)]
    backtest: Option<BacktestConfig>,
}
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,polyrust=debug")),
        )
        .init();

    // Check for --backtest flag
    let args: Vec<String> = std::env::args().collect();
    let backtest_mode = args.contains(&"--backtest".to_string());

    if backtest_mode {
        info!("Starting in backtest mode");
        return run_backtest().await;
    }

    info!("polyrust starting");

    // Load configuration
    let (config, arb_config) = match std::fs::read_to_string("config.toml") {
        Ok(contents) => {
            let config: Config = toml::from_str(&contents)
                .map_err(|e| warn!("failed to parse config: {e}"))
                .unwrap_or_default();
            let wrapper: ConfigWrapper = toml::from_str(&contents)
                .map_err(|e| warn!("failed to parse config wrapper: {e}"))
                .unwrap_or_default();
            (config, wrapper.arbitrage)
        }
        Err(e) => {
            info!("no config file loaded ({e}), using defaults");
            (Config::default(), ArbitrageConfig::default())
        }
    };
    let config = config.with_env_overrides();

    // Initialize persistence store
    let store = Store::new(&config.store.db_path).await?;
    let store = Arc::new(store);

    // Choose execution backend based on paper trading config
    let execution_backend: Box<dyn ExecutionBackend> = if config.paper.enabled {
        info!(
            "paper trading mode enabled (initial balance: {})",
            config.paper.initial_balance
        );
        Box::new(PaperBackend::new(
            config.paper.initial_balance,
            FillMode::Immediate,
        ))
    } else {
        info!("live trading mode enabled");
        Box::new(LiveBackend::new(&config).await?)
    };

    // Create feed command channel for engine → ClobFeed communication
    let (feed_cmd_tx, feed_cmd_rx) = feed_command_channel();

    // Validate sizing configuration
    if let Err(e) = arb_config.sizing.validate() {
        return Err(anyhow::anyhow!("Invalid sizing config: {}", e));
    }

    // Validate configured coins are supported
    const SUPPORTED_COINS: &[&str] = &["BTC", "ETH", "SOL", "XRP"];
    for coin in &arb_config.coins {
        if !SUPPORTED_COINS.contains(&coin.as_str()) {
            warn!(
                coin = %coin,
                supported = ?SUPPORTED_COINS,
                "Configured coin is not supported for market discovery - will be skipped"
            );
        }
    }

    // Create shared base for all crypto arbitrage strategies
    info!(
        tailend_enabled = arb_config.tailend.enabled,
        twosided_enabled = arb_config.twosided.enabled,
        confirmed_enabled = arb_config.confirmed.enabled,
        crosscorr_enabled = arb_config.correlation.enabled,
        "Loaded arbitrage config"
    );
    let base = Arc::new(CryptoArbBase::new(
        arb_config.clone(),
        config.polymarket.rpc_urls.clone(),
    ));

    // Build engine with conditionally registered strategies based on config
    let mut builder = Engine::builder()
        .config(config.clone())
        .execution(execution_backend)
        .feed_commands(feed_cmd_tx);

    // Always register the overview dashboard (shows what's enabled/disabled)
    let overview_dashboard = CryptoArbDashboard::new(Arc::clone(&base));
    builder = builder.strategy(DashboardStrategyWrapper::new(
        "crypto-arb-overview",
        Box::new(overview_dashboard),
    ));

    // Conditionally register trading strategies based on config
    if arb_config.tailend.enabled {
        info!("TailEnd mode enabled");
        builder = builder.strategy(TailEndStrategy::new(Arc::clone(&base)));
    }

    if arb_config.twosided.enabled {
        info!("TwoSided mode enabled");
        builder = builder.strategy(TwoSidedStrategy::new(Arc::clone(&base)));
    }

    if arb_config.confirmed.enabled {
        info!("Confirmed mode enabled");
        builder = builder.strategy(ConfirmedStrategy::new(Arc::clone(&base)));
    }

    if arb_config.correlation.enabled {
        info!("CrossCorr mode enabled");
        builder = builder.strategy(CrossCorrStrategy::new(Arc::clone(&base)));
    }

    // Always register per-mode dashboards so overview links don't 404.
    // Each dashboard already renders its enabled/disabled status.
    builder = builder.strategy(DashboardStrategyWrapper::new(
        "crypto-arb-tailend-dashboard",
        Box::new(TailEndDashboard::new(Arc::clone(&base))),
    ));
    builder = builder.strategy(DashboardStrategyWrapper::new(
        "crypto-arb-twosided-dashboard",
        Box::new(TwoSidedDashboard::new(Arc::clone(&base))),
    ));
    builder = builder.strategy(DashboardStrategyWrapper::new(
        "crypto-arb-confirmed-dashboard",
        Box::new(ConfirmedDashboard::new(Arc::clone(&base))),
    ));
    builder = builder.strategy(DashboardStrategyWrapper::new(
        "crypto-arb-crosscorr-dashboard",
        Box::new(CrossCorrDashboard::new(Arc::clone(&base))),
    ));

    if !arb_config.any_mode_enabled() {
        info!("No trading modes enabled — running in dashboard-only mode");
    }

    let mut engine = builder.build().await?;

    // Start market data feeds
    let event_bus = engine.event_bus().clone();

    let mut clob_feed = ClobFeed::new().with_command_receiver(feed_cmd_rx);
    let mut price_feed = PriceFeed::new();
    let mut discovery_feed = DiscoveryFeed::new(DiscoveryConfig::default());

    let clob_bus = event_bus.clone();
    let price_bus = event_bus.clone();
    let discovery_bus = event_bus.clone();

    tokio::spawn(async move {
        if let Err(e) = clob_feed.start(clob_bus).await {
            error!("CLOB feed failed to start: {e}");
        }
    });

    tokio::spawn(async move {
        if let Err(e) = price_feed.start(price_bus).await {
            error!("price feed failed to start: {e}");
        }
    });

    tokio::spawn(async move {
        if let Err(e) = discovery_feed.start(discovery_bus).await {
            error!("discovery feed failed to start: {e}");
        }
    });

    // Start trade persistence task
    let persistence_store = Arc::clone(&store);
    let persistence_bus = event_bus.clone();
    let persistence_context = engine.context().clone();
    tokio::spawn(async move {
        let mut rx = persistence_bus.subscribe();

        loop {
            match rx.recv().await {
                Some(Event::OrderUpdate(OrderEvent::Filled {
                    order_id,
                    market_id,
                    token_id,
                    side,
                    price,
                    size,
                    strategy_name,
                })) => {
                    // Calculate realized P&L for closing trades (Sell orders)
                    let realized_pnl = if side == OrderSide::Sell {
                        let positions = persistence_context.positions.read().await;
                        // Find position by token_id
                        positions
                            .open_positions
                            .values()
                            .find(|p| p.token_id == token_id)
                            .map(|pos| (price - pos.entry_price) * size)
                    } else {
                        None
                    };

                    let trade = Trade {
                        id: Uuid::new_v4(),
                        order_id: order_id.clone(),
                        market_id: market_id.clone(),
                        token_id: token_id.clone(),
                        side,
                        price,
                        size,
                        realized_pnl,
                        strategy_name: strategy_name.clone(),
                        timestamp: Utc::now(),
                    };

                    if let Err(e) = persistence_store.insert_trade(&trade).await {
                        error!(
                            order_id = %order_id,
                            error = %e,
                            "Failed to persist trade"
                        );
                    }
                }
                Some(_) => continue, // Ignore other events
                None => {
                    error!("Trade persistence event bus closed");
                    break;
                }
            }
        }
    });

    // Start dashboard if enabled
    let dashboard_config = engine.config().dashboard.clone();
    if dashboard_config.enabled {
        let dashboard = Dashboard::new(engine.context().clone(), Arc::clone(&store), event_bus);
        tokio::spawn(async move {
            if let Err(e) = dashboard
                .serve(&dashboard_config.host, dashboard_config.port)
                .await
            {
                error!("dashboard error: {e}");
            }
        });
    }

    // Run engine (blocks until Ctrl+C)
    engine.run().await?;

    info!("polyrust shutdown complete");
    Ok(())
}

/// Wrapper strategy that provides a dashboard view without processing events.
/// Used to register dashboard view providers that aren't tied to a single strategy.
struct DashboardStrategyWrapper {
    name: &'static str,
    provider: Box<dyn DashboardViewProvider + Send + Sync>,
}

impl DashboardStrategyWrapper {
    fn new(name: &'static str, provider: Box<dyn DashboardViewProvider + Send + Sync>) -> Self {
        Self { name, provider }
    }
}

#[async_trait::async_trait]
impl Strategy for DashboardStrategyWrapper {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Dashboard provider for crypto arbitrage strategies"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        Ok(())
    }

    async fn on_event(&mut self, _event: &Event, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        // Dashboard wrapper doesn't process events
        Ok(vec![])
    }

    async fn on_stop(&mut self, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        Ok(vec![])
    }

    fn dashboard_view(&self) -> Option<&dyn DashboardViewProvider> {
        Some(self.provider.as_ref())
    }
}

async fn run_backtest() -> anyhow::Result<()> {
    use polyrust_backtest::DataFetchConfig;

    // Load backtest configuration
    let (mut backtest_config, arb_config) = match std::fs::read_to_string("config.toml") {
        Ok(contents) => {
            let wrapper: ConfigWrapper = toml::from_str(&contents)
                .map_err(|e| warn!("failed to parse config: {e}"))
                .unwrap_or_default();
            let backtest_config = wrapper
                .backtest
                .ok_or_else(|| anyhow::anyhow!("Missing [backtest] section in config.toml"))?
                .with_env_overrides()?;
            (backtest_config, wrapper.arbitrage)
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Cannot run backtest without config.toml: {}",
                e
            ));
        }
    };

    info!(
        strategy = %backtest_config.strategy_name,
        start = %backtest_config.start_date,
        end = %backtest_config.end_date,
        initial_balance = %backtest_config.initial_balance,
        "Starting backtest"
    );

    // Open persistent historical data store
    let data_store = Arc::new(HistoricalDataStore::new(&backtest_config.data_db_path).await?);
    info!(
        db_path = %backtest_config.data_db_path,
        "Opened historical data store"
    );

    // Create in-memory store for backtest results (uses existing schema)
    let results_store = Arc::new(Store::new(":memory:").await?);

    // Initialize DataFetcher
    let fetch_config = DataFetchConfig {
        fidelity_secs: backtest_config.data_fidelity_secs,
        clob_recent_days: 7,
        max_trades_per_market: backtest_config.max_trades_per_market,
    };
    let data_fetcher = DataFetcher::new(Arc::clone(&data_store), fetch_config)?;

    // Fetch or verify cached data for the backtest period
    let mut market_ids = backtest_config.market_ids.clone();

    if !market_ids.is_empty() {
        info!(
            market_count = market_ids.len(),
            "Checking cached data for configured markets"
        );
    } else {
        info!("No market_ids configured - discovering markets for configured coins");

        // Discover markets for each coin in the arbitrage config
        for coin in &arb_config.coins {
            info!(coin, "Discovering markets for coin");
            let markets = data_fetcher
                .discover_expired_markets(
                    coin,
                    backtest_config.start_date,
                    backtest_config.end_date,
                    backtest_config.market_duration_secs,
                )
                .await?;

            info!(
                coin,
                market_count = markets.len(),
                "Discovered {} markets for coin", markets.len()
            );

            // Add market IDs to our list
            for market in markets {
                market_ids.push(market.market_id.clone());

                // Cache the market metadata
                data_store.insert_historical_market(market).await?;
            }
        }

        if market_ids.is_empty() {
            return Err(anyhow::anyhow!(
                "No markets found for configured coins in the specified date range"
            ));
        }

        info!(
            total_markets = market_ids.len(),
            "Discovered {} total markets", market_ids.len()
        );
    }

    // Fetch market data for all markets concurrently (bounded by fetch_concurrency)
    let total_markets = market_ids.len();
    let concurrency = backtest_config.fetch_concurrency;
    let fetcher = Arc::new(data_fetcher);
    let mut tasks = tokio::task::JoinSet::new();
    let mut completed = 0usize;

    info!(
        total_markets,
        concurrency,
        "Fetching market data concurrently"
    );

    for market_id in &market_ids {
        // If at capacity, wait for one to finish before spawning
        while tasks.len() >= concurrency {
            if let Some(result) = tasks.join_next().await {
                result??;
                completed += 1;
                info!(
                    progress = format!("[{}/{}]", completed, total_markets),
                    "Market data fetched"
                );
            }
        }
        let f = Arc::clone(&fetcher);
        let id = market_id.clone();
        let start = backtest_config.start_date;
        let end = backtest_config.end_date;
        tasks.spawn(async move { f.fetch_market_data(&id, start, end).await });
    }
    // Drain remaining tasks
    while let Some(result) = tasks.join_next().await {
        result??;
        completed += 1;
        info!(
            progress = format!("[{}/{}]", completed, total_markets),
            "Market data fetched"
        );
    }

    // Update backtest config with discovered/configured market_ids
    backtest_config.market_ids = market_ids;

    // Instantiate strategy based on strategy_name
    let strategy: Box<dyn Strategy> = match backtest_config.strategy_name.as_str() {
        "crypto-arb-tailend" => {
            let base = Arc::new(CryptoArbBase::new(arb_config.clone(), vec![]));
            Box::new(TailEndStrategy::new(base))
        }
        "crypto-arb-twosided" => {
            let base = Arc::new(CryptoArbBase::new(arb_config.clone(), vec![]));
            Box::new(TwoSidedStrategy::new(base))
        }
        "crypto-arb-confirmed" => {
            let base = Arc::new(CryptoArbBase::new(arb_config.clone(), vec![]));
            Box::new(ConfirmedStrategy::new(base))
        }
        "crypto-arb-crosscorr" => {
            let base = Arc::new(CryptoArbBase::new(arb_config.clone(), vec![]));
            Box::new(CrossCorrStrategy::new(base))
        }
        other => {
            return Err(anyhow::anyhow!("Unknown strategy name: {}", other));
        }
    };

    // Create and run backtest engine
    info!("Initializing backtest engine");
    let start_time = backtest_config.start_date;
    let end_time = backtest_config.end_date;
    let initial_balance = backtest_config.initial_balance;

    let mut engine = BacktestEngine::new(
        backtest_config.clone(),
        strategy,
        Arc::clone(&data_store),
        Arc::clone(&results_store),
    )
    .await;

    info!("Running backtest simulation");
    let _trades = engine.run().await?;

    // Generate report from stored results
    use polyrust_backtest::BacktestReport;
    let report =
        BacktestReport::from_engine_results(results_store, initial_balance, start_time, end_time)
            .await?;

    // Print report summary
    println!("\n{}", report.summary());

    info!("Backtest complete");
    Ok(())
}
