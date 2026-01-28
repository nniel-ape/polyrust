use std::sync::Arc;

use chrono::Utc;
use polyrust_core::prelude::*;
use polyrust_dashboard::Dashboard;
use polyrust_execution::{FillMode, LiveBackend, PaperBackend};
use polyrust_market::{ClobFeed, DiscoveryConfig, DiscoveryFeed, MarketDataFeed, PriceFeed};
use polyrust_store::Store;
use polyrust_strategies::CryptoArbitrageStrategy;
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
    let config = match Config::from_file("config.toml") {
        Ok(c) => c,
        Err(e) => {
            info!("no config file loaded ({e}), using defaults");
            Config::default()
        }
    }
    .with_env_overrides();

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

    // Build engine with crypto arbitrage strategy
    let strategy =
        CryptoArbitrageStrategy::new(Default::default(), config.polymarket.rpc_urls.clone());
    let mut engine = Engine::builder()
        .config(config.clone())
        .strategy(strategy)
        .execution(execution_backend)
        .feed_commands(feed_cmd_tx)
        .build()
        .await?;

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
