use crate::actions::Action;
use crate::config::Config;
use crate::context::{StrategyContext, StrategyHandle};
use crate::error::{PolyError, Result};
use crate::event_bus::EventBus;
use crate::events::{Event, MarketDataEvent, OrderEvent, SignalEvent, SystemEvent};
use crate::execution::ExecutionBackend;
use crate::strategy::Strategy;
use crate::types::{MarketId, MarketInfo, TokenId};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

/// Commands sent from the engine to market data feeds (e.g., ClobFeed).
#[derive(Debug)]
pub enum FeedCommand {
    Subscribe(MarketInfo),
    Unsubscribe(String),
}

pub type FeedCommandSender = mpsc::UnboundedSender<FeedCommand>;
pub type FeedCommandReceiver = mpsc::UnboundedReceiver<FeedCommand>;

/// Create a channel for sending feed commands from the engine to market feeds.
pub fn feed_command_channel() -> (FeedCommandSender, FeedCommandReceiver) {
    mpsc::unbounded_channel()
}

/// Main engine that orchestrates strategies, execution, and event routing.
pub struct Engine {
    config: Config,
    event_bus: EventBus,
    strategies: Vec<StrategyHandle>,
    execution: Arc<dyn ExecutionBackend>,
    context: StrategyContext,
    start_time: Option<Instant>,
    feed_command_tx: Option<FeedCommandSender>,
}

/// Builder for constructing an Engine instance.
pub struct EngineBuilder {
    config: Option<Config>,
    strategies: Vec<Box<dyn Strategy>>,
    execution: Option<Arc<dyn ExecutionBackend>>,
    feed_command_tx: Option<FeedCommandSender>,
}

impl EngineBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            strategies: Vec::new(),
            execution: None,
            feed_command_tx: None,
        }
    }

    pub fn config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    pub fn strategy(mut self, strategy: impl Strategy + 'static) -> Self {
        self.strategies.push(Box::new(strategy));
        self
    }

    pub fn execution(mut self, backend: impl ExecutionBackend + 'static) -> Self {
        self.execution = Some(Arc::new(backend));
        self
    }

    pub fn feed_commands(mut self, tx: FeedCommandSender) -> Self {
        self.feed_command_tx = Some(tx);
        self
    }

    pub async fn build(self) -> Result<Engine> {
        let config = self.config.unwrap_or_default();
        let execution = self
            .execution
            .ok_or_else(|| PolyError::Config("Execution backend is required".into()))?;

        let event_bus = EventBus::with_capacity(config.engine.event_bus_capacity);
        let context = StrategyContext::new();

        // Set initial balance from execution backend
        {
            let balance = execution.get_balance().await.unwrap_or_default();
            let mut state = context.balance.write().await;
            state.available_usdc = balance;
        }

        let strategy_count = self.strategies.len();
        let strategies: Vec<StrategyHandle> = self
            .strategies
            .into_iter()
            .map(|s| Arc::new(RwLock::new(s)))
            .collect();

        context
            .strategy_count
            .store(strategy_count, std::sync::atomic::Ordering::Relaxed);

        // Collect strategies that provide dashboard views
        {
            let mut views = context.strategy_views.write().await;
            for strategy in &strategies {
                let s = strategy.read().await;
                if let Some(provider) = s.dashboard_view() {
                    let view_name = provider.view_name().to_string();
                    if views.contains_key(&view_name) {
                        tracing::warn!(
                            view_name = %view_name,
                            strategy = %s.name(),
                            "Duplicate dashboard view name — overwriting previous registration"
                        );
                    }
                    views.insert(view_name, Arc::clone(strategy));
                }
            }
        }

        Ok(Engine {
            config,
            event_bus,
            strategies,
            execution,
            context,
            start_time: None,
            feed_command_tx: self.feed_command_tx,
        })
    }
}

impl Default for EngineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn builder() -> EngineBuilder {
        EngineBuilder::new()
    }

    /// Access the event bus (for market feeds and other producers to publish).
    pub fn event_bus(&self) -> &EventBus {
        &self.event_bus
    }

    /// Access shared context (for dashboard to read).
    pub fn context(&self) -> &StrategyContext {
        &self.context
    }

    /// Access config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Run the engine. Blocks until shutdown signal (Ctrl+C).
    pub async fn run(&mut self) -> Result<()> {
        self.start_time = Some(Instant::now());
        info!("engine starting");

        // Start all strategies
        for strategy in &self.strategies {
            let mut s = strategy.write().await;
            let name = s.name().to_string();
            info!(strategy = %name, "starting strategy");
            if let Err(e) = s.on_start(&self.context).await {
                error!(strategy = %name, error = %e, "strategy failed to start");
                return Err(e);
            }
            self.event_bus
                .publish(Event::System(SystemEvent::StrategyStarted(name)));
        }

        self.event_bus
            .publish(Event::System(SystemEvent::EngineStarted));

        // Spawn context-update task: updates shared state from external price events
        let ctx_update_handle = {
            let mut ctx_subscriber = self.event_bus.subscribe();
            let context = self.context.clone();
            tokio::spawn(async move {
                loop {
                    let event = match ctx_subscriber.recv().await {
                        Some(e) => e,
                        None => break,
                    };
                    if matches!(&event, Event::System(SystemEvent::EngineStopping)) {
                        break;
                    }
                    match &event {
                        Event::MarketData(MarketDataEvent::ExternalPrice {
                            symbol, price, ..
                        }) => {
                            let mut md = context.market_data.write().await;
                            md.external_prices.insert(symbol.clone(), *price);
                            debug!(symbol = %symbol, price = %price, "Updated external price in context");
                        }
                        Event::MarketData(MarketDataEvent::MarketDiscovered(info)) => {
                            let mut md = context.market_data.write().await;
                            debug!(market_id = %info.id, question = %info.question, "Stored discovered market in context");
                            md.markets.insert(info.id.clone(), info.clone());
                        }
                        Event::MarketData(MarketDataEvent::MarketExpired(id)) => {
                            let mut md = context.market_data.write().await;
                            md.markets.remove(id);
                            debug!(market_id = %id, "Removed expired market from context");
                        }
                        _ => {}
                    }
                }
            })
        };

        // Spawn strategy event loops
        let mut strategy_handles = Vec::new();
        for strategy in &self.strategies {
            let strategy = Arc::clone(strategy);
            let mut subscriber = self.event_bus.subscribe();
            let context = self.context.clone();
            let execution = Arc::clone(&self.execution);
            let event_bus = self.event_bus.clone();
            let feed_tx = self.feed_command_tx.clone();

            let handle = tokio::spawn(async move {
                loop {
                    let event = match subscriber.recv().await {
                        Some(e) => e,
                        None => break, // Channel closed
                    };

                    // Stop on engine shutdown event
                    if matches!(&event, Event::System(SystemEvent::EngineStopping)) {
                        break;
                    }

                    let mut s = strategy.write().await;
                    let name = s.name().to_string();

                    match s.on_event(&event, &context).await {
                        Ok(actions) => {
                            for action in actions {
                                if let Err(e) = execute_action(
                                    &action,
                                    &execution,
                                    &event_bus,
                                    &context,
                                    &name,
                                    feed_tx.as_ref(),
                                )
                                .await
                                {
                                    error!(
                                        strategy = %name,
                                        error = %e,
                                        "failed to execute action"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            error!(strategy = %name, error = %e, "strategy error on event");
                        }
                    }
                }
            });
            strategy_handles.push(handle);
        }

        // Wait for shutdown signal
        tokio::signal::ctrl_c()
            .await
            .map_err(|e| PolyError::Other(e.into()))?;

        info!("shutdown signal received, stopping engine");
        self.event_bus
            .publish(Event::System(SystemEvent::EngineStopping));

        // Stop all strategies and execute shutdown actions
        for strategy in &self.strategies {
            let mut s = strategy.write().await;
            let name = s.name().to_string();
            info!(strategy = %name, "stopping strategy");
            match s.on_stop(&self.context).await {
                Ok(actions) => {
                    for action in &actions {
                        if let Err(e) = execute_action(
                            action,
                            &self.execution,
                            &self.event_bus,
                            &self.context,
                            &name,
                            self.feed_command_tx.as_ref(),
                        )
                        .await
                        {
                            warn!(strategy = %name, error = %e, "failed to execute shutdown action");
                        }
                    }
                }
                Err(e) => {
                    warn!(strategy = %name, error = %e, "strategy error on stop");
                }
            }
            self.event_bus
                .publish(Event::System(SystemEvent::StrategyStopped(name)));
        }

        // Wait for strategy tasks and context-update task to finish
        for handle in strategy_handles {
            match handle.await {
                Ok(()) => {}
                Err(e) => {
                    error!(error = %e, "strategy task panicked during shutdown");
                }
            }
        }
        match ctx_update_handle.await {
            Ok(()) => {}
            Err(e) => {
                error!(error = %e, "context-update task panicked during shutdown");
            }
        }

        info!("engine stopped");
        Ok(())
    }
}

/// Helper function to look up market_id from token_id using market data state.
/// Public to allow usage from main.rs for trade persistence.
pub async fn find_market_id_for_token(
    context: &StrategyContext,
    token_id: &TokenId,
) -> Option<MarketId> {
    let market_data = context.market_data.read().await;
    for (market_id, market_info) in market_data.markets.iter() {
        if market_info.token_ids.outcome_a == *token_id
            || market_info.token_ids.outcome_b == *token_id
        {
            return Some(market_id.clone());
        }
    }
    None
}

/// Execute a single action from a strategy.
async fn execute_action(
    action: &Action,
    execution: &Arc<dyn ExecutionBackend>,
    event_bus: &EventBus,
    context: &StrategyContext,
    strategy_name: &str,
    feed_command_tx: Option<&FeedCommandSender>,
) -> Result<()> {
    match action {
        Action::PlaceOrder(req) => {
            match execution.place_order(req).await {
                Ok(result) => {
                    // Sync balance from execution backend to shared context
                    if let Ok(balance) = execution.get_balance().await {
                        let mut bal = context.balance.write().await;
                        bal.available_usdc = balance;
                    }
                    if result.success {
                        event_bus.publish(Event::OrderUpdate(OrderEvent::Placed(result.clone())));

                        // If order was immediately filled, publish Filled event for trade persistence
                        if result.status.as_deref() == Some("Filled")
                            && let Some(ref order_id) = result.order_id
                        {
                            // Look up market_id from token_id
                            if let Some(market_id) =
                                find_market_id_for_token(context, &result.token_id).await
                            {
                                event_bus.publish(Event::OrderUpdate(OrderEvent::Filled {
                                    order_id: order_id.clone(),
                                    market_id,
                                    token_id: result.token_id.clone(),
                                    side: result.side,
                                    price: result.price,
                                    size: result.size,
                                    strategy_name: strategy_name.to_string(),
                                }));
                            } else {
                                warn!(
                                    token_id = %result.token_id,
                                    "Cannot publish Filled event: market_id not found for token"
                                );
                            }
                        }
                    } else {
                        // Backend returned Ok but the order was rejected (e.g. validation
                        // failure, insufficient balance). Publish as Rejected so consumers
                        // don't misinterpret it as a live order.
                        warn!(
                            token_id = %result.token_id,
                            message = %result.message,
                            "Order rejected by backend"
                        );
                        event_bus.publish(Event::OrderUpdate(OrderEvent::Rejected {
                            order_id: result.order_id,
                            reason: result.message,
                            token_id: Some(result.token_id),
                        }));
                    }
                }
                Err(e) => {
                    warn!(
                        token_id = %req.token_id,
                        error = %e,
                        "Order placement failed, publishing rejection"
                    );
                    event_bus.publish(Event::OrderUpdate(OrderEvent::Rejected {
                        order_id: None,
                        reason: e.to_string(),
                        token_id: Some(req.token_id.clone()),
                    }));
                    return Err(e);
                }
            }
        }
        Action::PlaceBatchOrder(requests) => {
            match execution.place_batch_orders(requests).await {
                Ok(results) => {
                    // Sync balance once after the whole batch
                    if let Ok(balance) = execution.get_balance().await {
                        let mut bal = context.balance.write().await;
                        bal.available_usdc = balance;
                    }
                    for result in results {
                        if result.success {
                            event_bus.publish(Event::OrderUpdate(OrderEvent::Placed(result)));
                        } else {
                            warn!(
                                token_id = %result.token_id,
                                message = %result.message,
                                "Batch order rejected by backend"
                            );
                            event_bus.publish(Event::OrderUpdate(OrderEvent::Rejected {
                                order_id: result.order_id,
                                reason: result.message,
                                token_id: Some(result.token_id),
                            }));
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        count = requests.len(),
                        error = %e,
                        "Batch order placement failed"
                    );
                    // Publish rejection for each request in the batch
                    for req in requests {
                        event_bus.publish(Event::OrderUpdate(OrderEvent::Rejected {
                            order_id: None,
                            reason: e.to_string(),
                            token_id: Some(req.token_id.clone()),
                        }));
                    }
                    return Err(e);
                }
            }
        }
        Action::CancelOrder(id) => {
            execution.cancel_order(id).await?;
            if let Ok(balance) = execution.get_balance().await {
                let mut bal = context.balance.write().await;
                bal.available_usdc = balance;
            }
            event_bus.publish(Event::OrderUpdate(OrderEvent::Cancelled(id.clone())));
        }
        Action::CancelAllOrders => {
            execution.cancel_all_orders().await?;
            if let Ok(balance) = execution.get_balance().await {
                let mut bal = context.balance.write().await;
                bal.available_usdc = balance;
            }
        }
        Action::Log { level, message } => match level {
            crate::actions::LogLevel::Debug => {
                tracing::debug!(strategy = %strategy_name, "{message}")
            }
            crate::actions::LogLevel::Info => {
                tracing::info!(strategy = %strategy_name, "{message}")
            }
            crate::actions::LogLevel::Warn => {
                tracing::warn!(strategy = %strategy_name, "{message}")
            }
            crate::actions::LogLevel::Error => {
                tracing::error!(strategy = %strategy_name, "{message}")
            }
        },
        Action::EmitSignal {
            signal_type,
            payload,
        } => {
            event_bus.publish(Event::Signal(SignalEvent {
                strategy_name: strategy_name.to_string(),
                signal_type: signal_type.clone(),
                payload: payload.clone(),
                timestamp: chrono::Utc::now(),
            }));
        }
        Action::SubscribeMarket(id) => {
            if let Some(tx) = feed_command_tx {
                let md = context.market_data.read().await;
                if let Some(info) = md.markets.get(id) {
                    let info = info.clone();
                    drop(md);
                    if let Err(e) = tx.send(FeedCommand::Subscribe(info)) {
                        warn!(market_id = %id, error = %e, "failed to send subscribe command to feed");
                    } else {
                        info!(market_id = %id, strategy = %strategy_name, "sent subscribe command to feed");
                    }
                } else {
                    warn!(market_id = %id, "SubscribeMarket: market not found in context");
                }
            } else {
                warn!(market_id = %id, "SubscribeMarket: no feed command channel configured");
            }
        }
        Action::UnsubscribeMarket(id) => {
            if let Some(tx) = feed_command_tx {
                if let Err(e) = tx.send(FeedCommand::Unsubscribe(id.clone())) {
                    warn!(market_id = %id, error = %e, "failed to send unsubscribe command to feed");
                } else {
                    info!(market_id = %id, strategy = %strategy_name, "sent unsubscribe command to feed");
                }
            } else {
                warn!(market_id = %id, "UnsubscribeMarket: no feed command channel configured");
            }
        }
    }
    Ok(())
}
