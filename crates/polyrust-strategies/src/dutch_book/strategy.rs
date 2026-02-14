use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use rust_decimal::Decimal;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use polyrust_core::prelude::*;

use super::analyzer::ArbitrageAnalyzer;
use super::config::DutchBookConfig;
use super::scanner::GammaScanner;
use super::types::{
    ArbitrageOpportunity, DutchBookState, ExecutionState, FilledSide, PairedOrder, PairedPosition,
};

/// Dutch Book arbitrage strategy.
///
/// Exploits mispricing across prediction market outcomes: when the combined ask
/// price of YES + NO tokens is less than $1.00, buying both sides locks in a
/// guaranteed profit upon market resolution.
pub struct DutchBookStrategy {
    config: DutchBookConfig,
    pub(crate) analyzer: ArbitrageAnalyzer,
    /// New markets discovered by the scanner, awaiting subscription.
    pub(crate) pending_subscriptions: Arc<Mutex<Vec<MarketInfo>>>,
    /// Market IDs already known to the scanner (prevents re-discovery).
    known_market_ids: Arc<Mutex<HashSet<String>>>,
    /// Active paired order executions being tracked.
    pub(crate) active_executions: HashMap<MarketId, PairedOrder>,
    /// Open paired positions awaiting market resolution.
    pub(crate) open_positions: HashMap<MarketId, PairedPosition>,
    /// Reverse lookup: order_id → market_id (for routing fill/cancel events).
    pub(crate) order_to_market: HashMap<OrderId, MarketId>,
    /// Handle to the background scanner task.
    scanner_handle: Option<JoinHandle<()>>,
    /// Shared dashboard state (read by DutchBookDashboard).
    pub(crate) shared_state: Arc<RwLock<DutchBookState>>,
}

impl DutchBookStrategy {
    pub fn new(config: DutchBookConfig) -> Self {
        Self::with_shared_state(config, Arc::new(RwLock::new(DutchBookState::new())))
    }

    pub fn with_shared_state(
        config: DutchBookConfig,
        shared_state: Arc<RwLock<DutchBookState>>,
    ) -> Self {
        Self {
            config,
            analyzer: ArbitrageAnalyzer::new(),
            pending_subscriptions: Arc::new(Mutex::new(Vec::new())),
            known_market_ids: Arc::new(Mutex::new(HashSet::new())),
            active_executions: HashMap::new(),
            open_positions: HashMap::new(),
            order_to_market: HashMap::new(),
            scanner_handle: None,
            shared_state,
        }
    }

    /// Get the shared state for constructing a dashboard view.
    pub fn shared_state(&self) -> Arc<RwLock<DutchBookState>> {
        Arc::clone(&self.shared_state)
    }

    /// Drain the pending subscription queue and emit SubscribeMarket actions.
    async fn drain_pending_subscriptions(&mut self) -> Vec<Action> {
        let mut pending = self.pending_subscriptions.lock().await;
        if pending.is_empty() {
            return vec![];
        }

        let markets: Vec<MarketInfo> = pending.drain(..).collect();
        drop(pending);

        let mut actions = Vec::with_capacity(markets.len());
        for market in markets {
            info!(
                market_id = %market.id,
                question = %market.question,
                "Subscribing to market for Dutch Book monitoring"
            );
            self.analyzer.add_market(&market);
            actions.push(Action::SubscribeMarket(market));
        }
        actions
    }

    /// Handle an orderbook update: check for arbitrage and emit orders if found.
    async fn handle_orderbook_update(
        &mut self,
        snapshot: &OrderbookSnapshot,
        ctx: &StrategyContext,
    ) -> Vec<Action> {
        let token_id = &snapshot.token_id;

        // Look up the market for this token
        let market_entry = match self.analyzer.market_for_token(token_id) {
            Some(entry) => entry.clone(),
            None => return vec![],
        };

        // Skip if we already have an active execution or position for this market
        if self.active_executions.contains_key(&market_entry.market_id) {
            return vec![];
        }
        if self.open_positions.contains_key(&market_entry.market_id) {
            return vec![];
        }

        // Check position limit
        let total_active = self.open_positions.len() + self.active_executions.len();
        if total_active >= self.config.max_concurrent_positions {
            return vec![];
        }

        // Check for arbitrage opportunity using orderbooks from shared state
        let md = ctx.market_data.read().await;
        let opportunity = match self.analyzer.check_arbitrage(token_id, &md.orderbooks, &self.config)
        {
            Some(opp) => opp,
            None => return vec![],
        };
        drop(md);

        self.execute_opportunity(opportunity, &market_entry)
    }

    /// Create paired FOK orders for a detected arbitrage opportunity.
    fn execute_opportunity(
        &mut self,
        opp: ArbitrageOpportunity,
        market_entry: &super::types::MarketEntry,
    ) -> Vec<Action> {
        let now = Utc::now();

        // Build FOK BUY orders for both sides
        let yes_order = OrderRequest::new(
            market_entry.token_a.clone(),
            opp.yes_ask,
            opp.max_size,
            OrderSide::Buy,
            OrderType::Fok,
            market_entry.neg_risk,
        );

        let no_order = OrderRequest::new(
            market_entry.token_b.clone(),
            opp.no_ask,
            opp.max_size,
            OrderSide::Buy,
            OrderType::Fok,
            market_entry.neg_risk,
        );

        info!(
            market_id = %opp.market_id,
            combined_cost = %opp.combined_cost,
            profit_pct = %opp.profit_pct,
            size = %opp.max_size,
            yes_ask = %opp.yes_ask,
            no_ask = %opp.no_ask,
            "Executing Dutch Book arbitrage"
        );

        // Record opportunity in shared dashboard state (non-blocking try_write)
        if let Ok(mut state) = self.shared_state.try_write() {
            state.record_opportunity(opp.clone());
        }

        // We don't have order IDs yet — they come from the Placed event.
        // Create a placeholder PairedOrder with empty IDs; we'll fill them
        // when we receive OrderEvent::Placed.
        let paired = PairedOrder {
            market_id: opp.market_id.clone(),
            yes_order_id: String::new(),
            no_order_id: String::new(),
            size: opp.max_size,
            submitted_at: now,
            state: ExecutionState::new(),
            yes_fill_price: None,
            no_fill_price: None,
        };

        self.active_executions.insert(opp.market_id.clone(), paired);

        vec![Action::PlaceBatchOrder(vec![yes_order, no_order])]
    }

    /// Handle an OrderEvent::Placed — record order IDs for tracking.
    pub(crate) fn handle_order_placed(&mut self, result: &OrderResult) -> Vec<Action> {
        if !result.success {
            return vec![];
        }

        let order_id = match &result.order_id {
            Some(id) => id.clone(),
            None => return vec![],
        };

        // Find which active execution this placement belongs to by matching token_id
        let market_id = {
            let mut found = None;
            for (mid, _exec) in &self.active_executions {
                if let Some(entry) = self.analyzer.market_for_token(&result.token_id) {
                    if entry.market_id == *mid {
                        found = Some(mid.clone());
                        break;
                    }
                }
            }
            match found {
                Some(mid) => mid,
                None => return vec![],
            }
        };

        let entry = match self.analyzer.market_for_token(&result.token_id) {
            Some(e) => e.clone(),
            None => return vec![],
        };

        if let Some(exec) = self.active_executions.get_mut(&market_id) {
            // Check if this is an unwind sell order (execution in PartialFill state + SELL side)
            if exec.state.needs_unwind() && result.side == OrderSide::Sell {
                exec.state = exec.state.clone().start_unwind(order_id.clone());
                self.order_to_market
                    .insert(order_id, market_id.clone());
                debug!(
                    %market_id,
                    state = ?exec.state,
                    "Unwind sell order placed"
                );
            } else {
                // Normal batch order placement
                if result.token_id == entry.token_a {
                    exec.yes_order_id = order_id.clone();
                } else if result.token_id == entry.token_b {
                    exec.no_order_id = order_id.clone();
                }
                self.order_to_market
                    .insert(order_id, market_id);
            }
        }

        vec![]
    }

    /// Handle an order fill event — update execution state.
    pub(crate) fn handle_order_filled(
        &mut self,
        order_id: &str,
        token_id: &str,
        price: Decimal,
        size: Decimal,
    ) -> Vec<Action> {
        // Check if this is an unwind order fill
        if let Some(actions) = self.handle_unwind_order_event(order_id, true) {
            return actions;
        }

        let market_id = match self.order_to_market.get(order_id) {
            Some(mid) => mid.clone(),
            None => return vec![],
        };

        let entry = match self.analyzer.market_for_token(token_id) {
            Some(e) => e.clone(),
            None => return vec![],
        };

        let exec = match self.active_executions.get_mut(&market_id) {
            Some(e) => e,
            None => return vec![],
        };

        // Determine which side filled and record fill price
        let is_yes_side = token_id == entry.token_a;
        if is_yes_side {
            exec.yes_fill_price = Some(price);
        } else {
            exec.no_fill_price = Some(price);
        }
        let new_state = if is_yes_side {
            exec.state.clone().fill_yes()
        } else {
            exec.state.clone().fill_no()
        };
        exec.state = new_state;

        debug!(
            %market_id, %order_id, %token_id, %price, %size,
            is_yes = is_yes_side,
            state = ?exec.state,
            "Dutch Book order filled"
        );

        // Check if both sides are now filled
        if exec.state == ExecutionState::BothFilled {
            self.promote_to_position(&market_id)
        } else {
            vec![]
        }
    }

    /// Move a completed execution (both sides filled) to open_positions.
    fn promote_to_position(&mut self, market_id: &str) -> Vec<Action> {
        let exec = match self.active_executions.remove(market_id) {
            Some(e) => e,
            None => return vec![],
        };

        let yes_price = exec.yes_fill_price.unwrap_or(Decimal::ZERO);
        let no_price = exec.no_fill_price.unwrap_or(Decimal::ZERO);
        let combined_cost = (yes_price + no_price) * exec.size;
        let expected_profit = exec.size - combined_cost;

        info!(
            %market_id,
            size = %exec.size,
            %yes_price,
            %no_price,
            %combined_cost,
            %expected_profit,
            "Dutch Book paired position opened — both sides filled"
        );

        // Clean up order mappings
        self.order_to_market.remove(&exec.yes_order_id);
        self.order_to_market.remove(&exec.no_order_id);

        let position = PairedPosition {
            market_id: market_id.to_string(),
            yes_entry_price: yes_price,
            no_entry_price: no_price,
            size: exec.size,
            combined_cost,
            expected_profit,
            opened_at: Utc::now(),
        };
        self.open_positions
            .insert(market_id.to_string(), position);

        vec![Action::Log {
            level: LogLevel::Info,
            message: format!(
                "Dutch Book position opened for market {market_id} (size: {}, cost: {combined_cost}, profit: {expected_profit})",
                exec.size
            ),
        }]
    }

    /// Handle an order cancellation — update execution state, trigger unwind if needed.
    pub(crate) fn handle_order_cancelled(&mut self, order_id: &str) -> Vec<Action> {
        // Check if this is an unwind order completion (cancel/timeout of sell)
        if let Some(actions) = self.handle_unwind_order_event(order_id, false) {
            return actions;
        }

        let market_id = match self.order_to_market.get(order_id) {
            Some(mid) => mid.clone(),
            None => return vec![],
        };

        let exec = match self.active_executions.get_mut(&market_id) {
            Some(e) => e,
            None => return vec![],
        };

        // Determine which side was cancelled
        let is_yes_cancelled = order_id == exec.yes_order_id;
        let new_state = if is_yes_cancelled {
            exec.state.clone().cancel_yes(exec.no_order_id.clone())
        } else {
            exec.state.clone().cancel_no(exec.yes_order_id.clone())
        };
        exec.state = new_state;

        debug!(
            %market_id, %order_id,
            is_yes = is_yes_cancelled,
            state = ?exec.state,
            "Dutch Book order cancelled"
        );

        // If both cancelled, clean up
        if exec.state == ExecutionState::Complete {
            info!(%market_id, "Both Dutch Book orders cancelled — opportunity missed");
            let exec = self.active_executions.remove(&market_id).unwrap();
            self.order_to_market.remove(&exec.yes_order_id);
            self.order_to_market.remove(&exec.no_order_id);
            return vec![];
        }

        // If partial fill, trigger emergency unwind
        if exec.state.needs_unwind() {
            return self.start_emergency_unwind(&market_id.clone());
        }

        vec![]
    }

    /// Start emergency unwind for a partially-filled paired order.
    ///
    /// Sells the filled side at a discounted price (buy_price * (1 - unwind_discount))
    /// using a GTC order to avoid holding unhedged directional risk.
    fn start_emergency_unwind(&mut self, market_id: &str) -> Vec<Action> {
        let exec = match self.active_executions.get(market_id) {
            Some(e) => e,
            None => return vec![],
        };

        let (filled_side, filled_token_id, fill_price) = match &exec.state {
            ExecutionState::PartialFill {
                filled_side,
                filled_order_id: _,
            } => {
                let entry = match self.analyzer.market_for_token(&exec.yes_order_id) {
                    Some(e) => e,
                    None => {
                        // Try looking up by market_id from tracked_markets
                        match self.analyzer.market_for_market_id(market_id) {
                            Some(e) => e,
                            None => return vec![],
                        }
                    }
                };
                let (token_id, price) = match filled_side {
                    FilledSide::Yes => (
                        entry.token_a.clone(),
                        exec.yes_fill_price.unwrap_or(Decimal::ZERO),
                    ),
                    FilledSide::No => (
                        entry.token_b.clone(),
                        exec.no_fill_price.unwrap_or(Decimal::ZERO),
                    ),
                };
                (filled_side.clone(), token_id, price)
            }
            _ => return vec![],
        };

        // Calculate sell price: fill_price * (1 - unwind_discount)
        let sell_price = fill_price * (Decimal::ONE - self.config.unwind_discount);

        warn!(
            %market_id,
            ?filled_side,
            %fill_price,
            %sell_price,
            size = %exec.size,
            "Emergency unwind: selling filled side at discounted price"
        );

        let entry = match self.analyzer.market_for_market_id(market_id) {
            Some(e) => e.clone(),
            None => return vec![],
        };

        // Build GTC SELL order for the filled side
        let sell_order = OrderRequest::new(
            filled_token_id,
            sell_price,
            exec.size,
            OrderSide::Sell,
            OrderType::Gtc,
            entry.neg_risk,
        );

        vec![Action::PlaceOrder(sell_order)]
    }

    /// Handle a fill or cancel event for an unwind order.
    /// Returns Some(actions) if the order_id matched an unwinding execution, None otherwise.
    fn handle_unwind_order_event(&mut self, order_id: &str, is_fill: bool) -> Option<Vec<Action>> {
        // Find which execution is in Unwinding state with this sell_order_id
        let market_id = {
            let mut found = None;
            for (mid, exec) in &self.active_executions {
                if let ExecutionState::Unwinding { sell_order_id } = &exec.state {
                    if sell_order_id == order_id {
                        found = Some(mid.clone());
                        break;
                    }
                }
            }
            found?
        };

        if is_fill {
            // Unwind complete — clean up
            let exec = self.active_executions.remove(&market_id)?;
            self.order_to_market.remove(&exec.yes_order_id);
            self.order_to_market.remove(&exec.no_order_id);
            self.order_to_market.remove(order_id);

            let loss = match (&exec.yes_fill_price, &exec.no_fill_price) {
                (Some(p), None) => p * exec.size * self.config.unwind_discount,
                (None, Some(p)) => p * exec.size * self.config.unwind_discount,
                _ => Decimal::ZERO,
            };

            info!(
                %market_id,
                %loss,
                "Emergency unwind complete — realized loss"
            );

            // Record unwind loss in shared state
            if let Ok(mut state) = self.shared_state.try_write() {
                state.total_unwind_losses += loss;
            }

            Some(vec![Action::Log {
                level: LogLevel::Warn,
                message: format!(
                    "Dutch Book unwind for market {market_id}: ~{loss} USDC loss"
                ),
            }])
        } else {
            // Unwind order cancelled/rejected — log error, keep tracking
            warn!(
                %market_id, %order_id,
                "Emergency unwind order cancelled — manual intervention may be needed"
            );
            Some(vec![])
        }
    }

    /// Handle a market expiration event.
    fn handle_market_expired(&mut self, market_id: &str) -> Vec<Action> {
        self.analyzer.remove_market(market_id);

        // Check if we have an open position that should be redeemed
        if let Some(pos) = self.open_positions.remove(market_id) {
            info!(
                %market_id,
                expected_profit = %pos.expected_profit,
                "Market expired with open Dutch Book position — requesting redemption"
            );
            // Redemption is handled by the engine's CtfRedeemer / ClaimMonitor.
            // We emit a RedeemPosition action.
            // Note: We need the condition_id and token_ids — use the market_id
            // which is the condition_id in Polymarket.
            return vec![Action::RedeemPosition(RedeemRequest {
                market_id: market_id.to_string(),
                condition_id: market_id.to_string(),
                token_ids: vec![pos.yes_entry_price.to_string(), pos.no_entry_price.to_string()],
                neg_risk: false,
            })];
        }

        // Clean up any active execution for this market
        if let Some(exec) = self.active_executions.remove(market_id) {
            self.order_to_market.remove(&exec.yes_order_id);
            self.order_to_market.remove(&exec.no_order_id);
        }

        vec![]
    }

    /// Get the number of tracked markets.
    pub fn tracked_market_count(&self) -> usize {
        self.analyzer.tracked_count()
    }

    /// Get the number of open positions.
    pub fn open_position_count(&self) -> usize {
        self.open_positions.len()
    }

    /// Get the number of active executions.
    pub fn active_execution_count(&self) -> usize {
        self.active_executions.len()
    }

    /// Sync internal state to the shared dashboard state.
    async fn sync_dashboard_state(&self) {
        let mut state = self.shared_state.write().await;
        state.tracked_markets = self.analyzer.tracked_count();
        state.positions = self.open_positions.values().cloned().collect();
        state.executions = self.active_executions.values().cloned().collect();
    }
}

#[async_trait]
impl Strategy for DutchBookStrategy {
    fn name(&self) -> &str {
        "dutch-book"
    }

    fn description(&self) -> &str {
        "Dutch Book arbitrage: buys both sides when combined ask < $1.00"
    }

    async fn on_start(&mut self, _ctx: &StrategyContext) -> Result<()> {
        info!(
            max_combined_cost = %self.config.max_combined_cost,
            min_profit = %self.config.min_profit_threshold,
            max_positions = self.config.max_concurrent_positions,
            scan_interval = self.config.scan_interval_secs,
            "Dutch Book strategy started"
        );

        // Spawn background market scanner
        let handle = GammaScanner::start_scanner(
            self.config.clone(),
            Arc::clone(&self.pending_subscriptions),
            Arc::clone(&self.known_market_ids),
        );
        self.scanner_handle = Some(handle);

        Ok(())
    }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        // Always drain pending subscriptions on any event
        let mut actions = self.drain_pending_subscriptions().await;

        let event_actions = match event {
            Event::MarketData(MarketDataEvent::OrderbookUpdate(snapshot)) => {
                self.handle_orderbook_update(snapshot, ctx).await
            }

            Event::OrderUpdate(OrderEvent::Placed(result)) => self.handle_order_placed(result),

            Event::OrderUpdate(OrderEvent::Filled {
                order_id,
                token_id,
                price,
                size,
                ..
            }) => self.handle_order_filled(order_id, token_id, *price, *size),

            Event::OrderUpdate(OrderEvent::Cancelled(order_id)) => {
                self.handle_order_cancelled(order_id)
            }

            Event::OrderUpdate(OrderEvent::Rejected { order_id, reason, .. }) => {
                if let Some(oid) = order_id {
                    debug!(%oid, %reason, "Dutch Book order rejected");
                    self.handle_order_cancelled(oid)
                } else {
                    vec![]
                }
            }

            Event::MarketData(MarketDataEvent::MarketExpired(market_id)) => {
                self.handle_market_expired(market_id)
            }

            _ => vec![],
        };

        actions.extend(event_actions);

        // Sync state to dashboard after processing
        self.sync_dashboard_state().await;

        Ok(actions)
    }

    async fn on_stop(&mut self, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        // Abort the scanner
        if let Some(handle) = self.scanner_handle.take() {
            handle.abort();
            info!("Dutch Book scanner stopped");
        }

        info!(
            open_positions = self.open_positions.len(),
            active_executions = self.active_executions.len(),
            "Dutch Book strategy stopped"
        );

        Ok(vec![])
    }
}
