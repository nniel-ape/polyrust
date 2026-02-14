mod verify;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use polyrust_backtest::{BacktestConfig, BacktestEngine, DataFetcher, HistoricalDataStore};
use polyrust_core::prelude::*;
use polyrust_dashboard::Dashboard;
use polyrust_execution::{FillMode, LiveBackend, PaperBackend};
use polyrust_market::{
    BinanceFeed, ClobFeed, CoinbaseFeed, DiscoveryConfig, DiscoveryFeed, MarketDataFeed, PriceFeed,
};
use polyrust_store::Store;
use polyrust_strategies::{
    ArbitrageConfig, CryptoArbBase, CryptoArbDashboard, ReferenceQualityLevel, TailEndStrategy,
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

/// Extract CLI argument value by key.
#[allow(dead_code)]
fn cli_arg(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1).cloned())
}

use indicatif::{ProgressBar, ProgressStyle};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

/// Writer that buffers bytes and flushes complete lines through a progress bar
/// (via `pb.println()`) or falls back to stderr when no bar is active.
struct PbWriter {
    buf: Vec<u8>,
}

impl std::io::Write for PbWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        // Flush each complete line
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buf[..pos]).to_string();
            self.buf.drain(..=pos);
            if let Some(pb) = polyrust_backtest::progress::active_progress_bar() {
                pb.println(&line);
            } else {
                eprintln!("{line}");
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).to_string();
            self.buf.clear();
            if let Some(pb) = polyrust_backtest::progress::active_progress_bar() {
                pb.println(&line);
            } else {
                eprint!("{line}");
            }
        }
        Ok(())
    }
}

/// MakeWriter that creates PbWriter instances for each tracing event.
struct PbMakeWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for PbMakeWriter {
    type Writer = PbWriter;
    fn make_writer(&'a self) -> Self::Writer {
        PbWriter { buf: Vec::new() }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Check for CLI flags before tracing init so we can adjust the log filter.
    let args: Vec<String> = std::env::args().collect();
    let backtest_mode = args.contains(&"--backtest".to_string());
    let backtest_sweep_mode = args.contains(&"--backtest-sweep".to_string());
    let verify_mode = args.contains(&"--verify".to_string());

    // Sweep mode: suppress per-run strategy/engine noise so the progress bar stays visible.
    // RUST_LOG still overrides if the user wants verbose output.
    let default_filter = if backtest_sweep_mode {
        "warn,polyrust_backtest=info"
    } else {
        "info,polyrust=debug"
    };

    // Initialize tracing — route output through PbMakeWriter so log lines
    // print cleanly above any active indicatif progress bar.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter)),
        )
        .with_writer(PbMakeWriter)
        .init();

    if verify_mode {
        info!("Starting in verify mode");
        return verify::run_verify().await;
    }

    if backtest_sweep_mode {
        info!("Starting in backtest sweep mode");
        return run_backtest_sweep().await;
    }

    if backtest_mode {
        info!("Starting in backtest mode");
        return run_backtest().await;
    }

    info!("polyrust starting");

    // Load configuration — parse errors are fatal (silent defaults are dangerous for live trading)
    let (config, arb_config) = match std::fs::read_to_string("config.toml") {
        Ok(contents) => {
            let config: Config = toml::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("failed to parse config.toml: {e}"))?;
            let wrapper: ConfigWrapper = toml::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("failed to parse config.toml: {e}"))?;
            (config, wrapper.arbitrage)
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "config.toml not found ({e}). Copy config.example.toml to config.toml and customize it."
            ));
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

    // Validate full arbitrage configuration (sizing, stop-loss, cross-config)
    if let Err(e) = arb_config.validate() {
        return Err(anyhow::anyhow!("Invalid arbitrage config: {}", e));
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
    info!(enabled = arb_config.enabled, "Loaded arbitrage config");
    let base = Arc::new(CryptoArbBase::new(
        arb_config.clone(),
        config.polymarket.rpc_urls.clone(),
    ));
    base.warm_up().await;

    // Build engine with conditionally registered strategies based on config
    let mut builder = Engine::builder()
        .config(config.clone())
        .execution(execution_backend)
        .feed_commands(feed_cmd_tx);

    // Conditionally register strategy based on config
    if arb_config.enabled {
        info!("Arbitrage strategy enabled");
        builder = builder.strategy(TailEndStrategy::new(Arc::clone(&base)));
    } else {
        info!("Arbitrage strategy disabled — running in dashboard-only mode");
    }

    // Always register dashboard
    builder = builder.strategy(DashboardStrategyWrapper::new(
        "crypto-arb-dashboard",
        Box::new(CryptoArbDashboard::new(Arc::clone(&base))),
    ));

    let mut engine = builder.build().await?;

    // Collect background task handles for clean shutdown
    let mut bg_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // Start auto-claim monitor if enabled
    if config.auto_claim.enabled {
        info!(
            "Auto-claim enabled (poll interval: {}s)",
            config.auto_claim.poll_interval_secs
        );
        let claim_monitor = Arc::new(ClaimMonitor::new(
            config.auto_claim.clone(),
            engine.event_bus().clone(),
            engine.execution(),
            engine.context().clone(),
        ));
        bg_handles.push(tokio::spawn(async move {
            if let Err(e) = claim_monitor.run().await {
                error!("ClaimMonitor task failed: {e}");
            }
        }));
    } else {
        info!("Auto-claim disabled");
    }

    // Start market data feeds
    let event_bus = engine.event_bus().clone();

    let mut clob_feed = ClobFeed::new().with_command_receiver(feed_cmd_rx);
    let mut price_feed = PriceFeed::new();
    let mut discovery_feed = DiscoveryFeed::new(DiscoveryConfig {
        coins: arb_config.coins.clone(),
        ..DiscoveryConfig::default()
    });
    let mut binance_feed = BinanceFeed::new(arb_config.coins.clone());
    let mut coinbase_feed = CoinbaseFeed::new(arb_config.coins.clone());

    // Start all feeds in main scope (NOT spawned). Each feed's start() spawns
    // internal tasks and returns immediately — wrapping in tokio::spawn would
    // complete instantly and prevent calling stop() on shutdown.
    if let Err(e) = clob_feed.start(event_bus.clone()).await {
        error!("CLOB feed failed to start: {e}");
    }
    if let Err(e) = price_feed.start(event_bus.clone()).await {
        error!("price feed failed to start: {e}");
    }
    if let Err(e) = discovery_feed.start(event_bus.clone()).await {
        error!("discovery feed failed to start: {e}");
    }
    if let Err(e) = binance_feed.start(event_bus.clone()).await {
        error!("Binance feed failed to start: {e}");
    }
    if let Err(e) = coinbase_feed.start(event_bus.clone()).await {
        error!("Coinbase feed failed to start: {e}");
    }

    // Start trade persistence task
    let persistence_store = Arc::clone(&store);
    let persistence_bus = event_bus.clone();
    let persistence_context = engine.context().clone();
    bg_handles.push(tokio::spawn(async move {
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
                    realized_pnl: event_pnl,
                    fee: event_fee,
                    order_type,
                    orderbook_snapshot,
                })) => {
                    // Use strategy-provided P&L if available, else compute from position state
                    // with fee deduction and timeout-based locking
                    let realized_pnl = if event_pnl.is_some() {
                        event_pnl
                    } else if side == OrderSide::Sell {
                        match tokio::time::timeout(
                            Duration::from_millis(200),
                            persistence_context.positions.read(),
                        )
                        .await
                        {
                            Ok(positions) => positions
                                .open_positions
                                .values()
                                .find(|p| p.token_id == token_id)
                                .map(|pos| {
                                    let gross = (price - pos.entry_price) * size;
                                    // Deduct sell-side fee if known
                                    gross - event_fee.unwrap_or(Decimal::ZERO)
                                }),
                            Err(_) => {
                                warn!(
                                    order_id = %order_id,
                                    token_id = %token_id,
                                    "Sell trade P&L: position lock timeout, P&L will be None"
                                );
                                None
                            }
                        }
                    } else {
                        None
                    };

                    // Capture entry_price for closing (sell) trades
                    let entry_price = if side == OrderSide::Sell {
                        match tokio::time::timeout(
                            Duration::from_millis(50),
                            persistence_context.positions.read(),
                        )
                        .await
                        {
                            Ok(positions) => positions
                                .open_positions
                                .values()
                                .find(|p| p.token_id == token_id)
                                .map(|p| p.entry_price),
                            Err(_) => None,
                        }
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
                        fee: event_fee,
                        order_type,
                        entry_price,
                        close_reason: None,
                        orderbook_snapshot,
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
    }));

    // Start dashboard if enabled
    let dashboard_config = engine.config().dashboard.clone();
    if dashboard_config.enabled {
        let dashboard = Dashboard::new(
            engine.context().clone(),
            Arc::clone(&store),
            event_bus,
            Utc::now(),
        );
        bg_handles.push(tokio::spawn(async move {
            if let Err(e) = dashboard
                .serve(&dashboard_config.host, dashboard_config.port)
                .await
            {
                error!("dashboard error: {e}");
            }
        }));
    }

    // Run engine (blocks until Ctrl+C)
    engine.run().await?;

    // Stop all feeds gracefully (sends shutdown signal to internal loops)
    let _ = clob_feed.stop().await;
    let _ = price_feed.stop().await;
    let _ = discovery_feed.stop().await;
    let _ = binance_feed.stop().await;
    let _ = coinbase_feed.stop().await;

    // Abort all background tasks for clean shutdown
    info!(tasks = bg_handles.len(), "Aborting background tasks");
    for handle in &bg_handles {
        handle.abort();
    }
    // Give tasks a brief window to finish cleanup
    for handle in bg_handles {
        let _ = handle.await;
    }

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

/// Fetch market data concurrently with a progress bar, returning successfully-fetched market IDs.
async fn fetch_markets_with_progress(
    market_ids: &[String],
    fetcher: Arc<DataFetcher>,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    concurrency: usize,
) -> (Vec<String>, usize) {
    let total_markets = market_ids.len();
    let pb = ProgressBar::new(total_markets as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} markets ({eta}) {msg}",
        )
        .unwrap(),
    );
    pb.set_message("fetching");
    let _pb_guard = polyrust_backtest::ProgressBarGuard::register(&pb);

    let mut tasks = tokio::task::JoinSet::new();
    let mut successful_ids: Vec<String> = Vec::new();
    let mut skipped = 0usize;

    for market_id in market_ids {
        // If at capacity, wait for one to finish before spawning
        while tasks.len() >= concurrency {
            if let Some(result) = tasks.join_next().await {
                match result {
                    Ok(Ok(id)) => {
                        successful_ids.push(id);
                    }
                    Ok(Err(e)) => {
                        skipped += 1;
                        pb.println(format!("Skipping market: {e}"));
                    }
                    Err(e) => {
                        skipped += 1;
                        pb.println(format!("Task panic: {e}"));
                    }
                }
                pb.inc(1);
            }
        }
        let f = Arc::clone(&fetcher);
        let id = market_id.clone();
        tasks.spawn(async move {
            f.fetch_market_data(&id, start, end).await?;
            Ok::<String, polyrust_backtest::error::BacktestError>(id)
        });
    }

    // Drain remaining tasks
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(id)) => {
                successful_ids.push(id);
            }
            Ok(Err(e)) => {
                skipped += 1;
                pb.println(format!("Skipping market: {e}"));
            }
            Err(e) => {
                skipped += 1;
                pb.println(format!("Task panic: {e}"));
            }
        }
        pb.inc(1);
    }

    let completed = successful_ids.len();
    pb.finish_with_message(format!("{completed} ok, {skipped} skipped"));

    if skipped > 0 {
        warn!(
            skipped,
            completed, "Some markets failed to fetch and were skipped"
        );
    }

    (successful_ids, skipped)
}

async fn run_backtest() -> anyhow::Result<()> {
    use polyrust_backtest::DataFetchConfig;

    // Load backtest configuration — parse errors are fatal
    let (mut backtest_config, mut arb_config) = match std::fs::read_to_string("config.toml") {
        Ok(contents) => {
            let wrapper: ConfigWrapper = toml::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("failed to parse config.toml: {e}"))?;
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
    };
    let data_fetcher = DataFetcher::new(Arc::clone(&data_store), fetch_config)?;

    // Fetch or verify cached data for the backtest period
    let mut market_ids = backtest_config.market_ids.clone();

    if backtest_config.offline {
        // Offline mode: use only cached data, no network requests
        if market_ids.is_empty() {
            info!("Offline mode: loading cached markets from backtest_data.db");
            let cached = data_store
                .list_cached_markets(backtest_config.start_date, backtest_config.end_date)
                .await?;
            market_ids = cached.into_iter().map(|m| m.market_id).collect();
        }

        if market_ids.is_empty() {
            return Err(anyhow::anyhow!(
                "Offline mode: no cached markets found for the configured date range"
            ));
        }

        info!(
            total_markets = market_ids.len(),
            "Offline mode: using {} cached markets",
            market_ids.len()
        );
    } else {
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
                    "Discovered {} markets for coin",
                    markets.len()
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
                "Discovered {} total markets",
                market_ids.len()
            );
        }

        // Fetch market data for all markets concurrently (bounded by fetch_concurrency)
        let concurrency = backtest_config.fetch_concurrency;
        let fetcher = Arc::new(data_fetcher);

        let (successful_ids, _skipped) = fetch_markets_with_progress(
            &market_ids,
            fetcher,
            backtest_config.start_date,
            backtest_config.end_date,
            concurrency,
        )
        .await;

        market_ids = successful_ids;
    }

    // Update backtest config with successfully-fetched market_ids
    backtest_config.market_ids = market_ids;

    // Fetch historical Binance klines for real crypto prices
    // In online mode: fetch from Binance API and cache to DB
    // In offline mode: skip (engine will use whatever is already cached)
    if !backtest_config.offline {
        info!("Fetching historical Binance klines for configured coins");
        let crypto_fetcher = DataFetcher::new(
            Arc::clone(&data_store),
            DataFetchConfig {
                fidelity_secs: backtest_config.data_fidelity_secs,
            },
        )?;
        crypto_fetcher
            .fetch_crypto_prices(
                &arb_config.coins,
                backtest_config.start_date,
                backtest_config.end_date,
            )
            .await?;
    } else {
        info!("Offline mode: skipping Binance klines fetch (will use cached data if available)");
    }

    // Backtest can't produce Historical quality (record_price uses wall clock)
    arb_config.tailend.min_reference_quality = ReferenceQualityLevel::Current;
    arb_config.use_chainlink = false; // No RPC in backtest
    arb_config.tailend.stale_ob_secs = i64::MAX; // Staleness meaningless in backtest
    arb_config.tailend.use_composite_price = false; // Composite price gating meaningless with deterministic data
    arb_config.stop_loss.sl_max_dispersion_bps = Decimal::new(10000, 0); // Dispersion check disabled in backtest
    arb_config.stop_loss.min_remaining_secs = 0; // Allow stop-loss evaluation until expiry (live default=45 suppresses most of the short position lifetime)

    // Apply backtest-specific sizing overrides (if configured)
    if let Some(ref sizing_override) = backtest_config.sizing {
        sizing_override.apply_to(&mut arb_config.sizing);
    }

    // Instantiate strategy based on strategy_name
    let strategy: Box<dyn Strategy> = match backtest_config.strategy_name.as_str() {
        "crypto-arb-tailend" => {
            let base = Arc::new(CryptoArbBase::new(arb_config.clone(), vec![]));
            Box::new(TailEndStrategy::new(base))
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
    let trades = engine.run().await?;

    // Generate report from stored results
    use polyrust_backtest::BacktestReport;
    let report = BacktestReport::from_engine_results(
        results_store,
        trades,
        initial_balance,
        start_time,
        end_time,
    )
    .await?;

    // Print report summary
    println!("\n{}", report.summary());

    info!("Backtest complete");
    Ok(())
}

async fn run_backtest_sweep() -> anyhow::Result<()> {
    use polyrust_backtest::{DataFetchConfig, SweepRunner};

    // Load configuration — parse errors are fatal
    let (mut backtest_config, mut arb_config) = match std::fs::read_to_string("config.toml") {
        Ok(contents) => {
            let wrapper: ConfigWrapper = toml::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("failed to parse config.toml: {e}"))?;
            let backtest_config = wrapper
                .backtest
                .ok_or_else(|| anyhow::anyhow!("Missing [backtest] section in config.toml"))?
                .with_env_overrides()?;
            (backtest_config, wrapper.arbitrage)
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Cannot run backtest sweep without config.toml: {}",
                e
            ));
        }
    };

    let sweep_config = backtest_config
        .sweep
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Missing [backtest.sweep] section in config.toml"))?;

    info!(
        strategy = %backtest_config.strategy_name,
        start = %backtest_config.start_date,
        end = %backtest_config.end_date,
        "Starting backtest parameter sweep"
    );

    // Open persistent historical data store
    let data_store = Arc::new(HistoricalDataStore::new(&backtest_config.data_db_path).await?);

    // Handle data fetching (same logic as run_backtest)
    let mut market_ids = backtest_config.market_ids.clone();

    if backtest_config.offline {
        if market_ids.is_empty() {
            info!("Offline mode: loading cached markets from backtest_data.db");
            let cached = data_store
                .list_cached_markets(backtest_config.start_date, backtest_config.end_date)
                .await?;
            market_ids = cached.into_iter().map(|m| m.market_id).collect();
        }

        if market_ids.is_empty() {
            return Err(anyhow::anyhow!(
                "Offline mode: no cached markets found for the configured date range"
            ));
        }

        info!(
            total_markets = market_ids.len(),
            "Offline mode: using {} cached markets",
            market_ids.len()
        );
    } else {
        // Online mode: discover and fetch markets
        let fetch_config = DataFetchConfig {
            fidelity_secs: backtest_config.data_fidelity_secs,
        };
        let data_fetcher =
            polyrust_backtest::DataFetcher::new(Arc::clone(&data_store), fetch_config)?;

        if market_ids.is_empty() {
            info!("No market_ids configured - discovering markets for configured coins");
            for coin in &arb_config.coins {
                let markets = data_fetcher
                    .discover_expired_markets(
                        coin,
                        backtest_config.start_date,
                        backtest_config.end_date,
                        backtest_config.market_duration_secs,
                    )
                    .await?;

                for market in markets {
                    market_ids.push(market.market_id.clone());
                    data_store.insert_historical_market(market).await?;
                }
            }

            if market_ids.is_empty() {
                return Err(anyhow::anyhow!(
                    "No markets found for configured coins in the specified date range"
                ));
            }
        }

        // Fetch market data concurrently with progress bar
        let concurrency = backtest_config.fetch_concurrency;
        let fetcher = Arc::new(data_fetcher);

        let (successful_ids, _skipped) = fetch_markets_with_progress(
            &market_ids,
            fetcher,
            backtest_config.start_date,
            backtest_config.end_date,
            concurrency,
        )
        .await;
        market_ids = successful_ids;

        // Fetch Binance klines
        let crypto_fetcher = polyrust_backtest::DataFetcher::new(
            Arc::clone(&data_store),
            DataFetchConfig {
                fidelity_secs: backtest_config.data_fidelity_secs,
            },
        )?;
        crypto_fetcher
            .fetch_crypto_prices(
                &arb_config.coins,
                backtest_config.start_date,
                backtest_config.end_date,
            )
            .await?;
    }

    backtest_config.market_ids = market_ids;

    // Backtest can't produce Historical quality
    arb_config.tailend.min_reference_quality = ReferenceQualityLevel::Current;
    arb_config.tailend.use_composite_price = false; // Composite price gating meaningless with deterministic data
    arb_config.stop_loss.sl_max_dispersion_bps = Decimal::new(10000, 0); // Dispersion check disabled in backtest
    arb_config.stop_loss.min_remaining_secs = 0; // Allow stop-loss evaluation until expiry (live default=45 suppresses most of the short position lifetime)

    // Run sweep
    let rank_by = sweep_config
        .rank_by
        .clone()
        .unwrap_or_else(|| "sharpe".to_string());
    let top_n = sweep_config.top_n.unwrap_or(20);
    let output_dir = sweep_config
        .output_dir
        .clone()
        .unwrap_or_else(|| "sweep_results".to_string());

    let runner = SweepRunner::new(sweep_config, backtest_config, arb_config, data_store);
    let mut report = runner.run().await?;

    // Sort and display
    report.sort_by(&rank_by);
    report.print_table(top_n);

    // Sensitivity analysis
    let sensitivity = report.sensitivity_analysis();
    sensitivity.print_table();

    // Export to timestamped subdirectory
    let timestamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let run_dir = format!("{}/{}", output_dir, timestamp);
    std::fs::create_dir_all(&run_dir)?;

    report.export_csv(&format!("{}/results.csv", run_dir))?;
    report.export_json(&format!("{}/results.json", run_dir))?;
    sensitivity.export_csv(&format!("{}/sensitivity.csv", run_dir))?;
    sensitivity.export_json(&format!("{}/sensitivity.json", run_dir))?;

    info!("Sweep results exported to {}", run_dir);
    Ok(())
}
