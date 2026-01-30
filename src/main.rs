use std::sync::Arc;

use chrono::Utc;
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

/// Wrapper to extract arbitrage config from TOML file.
#[derive(Debug, Deserialize, Default)]
struct ConfigWithArbitrage {
    #[serde(default)]
    arbitrage: ArbitrageConfig,
}
use tracing::{error, info};
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

    info!("polyrust starting");

    // Load configuration
    let (config, arb_config) = match std::fs::read_to_string("config.toml") {
        Ok(contents) => {
            let config: Config = toml::from_str(&contents)
                .map_err(|e| info!("failed to parse config: {e}"))
                .unwrap_or_default();
            let arb_wrapper: ConfigWithArbitrage = toml::from_str(&contents)
                .map_err(|e| info!("failed to parse arbitrage config: {e}"))
                .unwrap_or_default();
            (config, arb_wrapper.arbitrage)
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
