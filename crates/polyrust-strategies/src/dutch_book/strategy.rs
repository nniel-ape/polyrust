use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Timeout for active executions: if an execution stays unresolved for this long,
/// it is cleaned up to prevent permanently blocking the market slot.
/// FOK orders should resolve within seconds; 120s is very generous.
const EXECUTION_TIMEOUT_SECS: i64 = 120;

/// Minimum interval between stale execution cleanup runs (seconds).
const CLEANUP_INTERVAL_SECS: i64 = 30;

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
    /// Markets where an unwind PlaceOrder was dispatched but the execution was removed
    /// (e.g., by a stale fill) before the Placed event arrived. When handle_order_placed
    /// sees a SELL for one of these markets, it emits CancelOrder immediately.
    pub(crate) cancel_on_placed: HashSet<MarketId>,
    /// Token → market mapping for cancel_on_placed flow. When a market is removed from
    /// the analyzer (e.g., on expiry) while cancel_on_placed is pending, the normal
    /// token→market resolution via `analyzer.market_for_token()` fails. This map
    /// preserves the mapping so handle_order_placed can still resolve the market_id.
    pub(crate) cancel_token_to_market: HashMap<TokenId, MarketId>,
    /// Sell orders that were cancelled but may still fill (cancel could race with fill).
    /// Maps order_id → market_id so late fills can be detected and accounted for rather
    /// than silently dropped. Populated when cancel_on_placed cancels a SELL, or when
    /// a stale-fill cleanup cancels the active unwind order.
    pub(crate) orphaned_sells: HashMap<OrderId, MarketId>,
    /// Handle to the background scanner task.
    scanner_handle: Option<JoinHandle<()>>,
    /// Last time cleanup_stale_executions was run (rate-limited to avoid hot-path overhead).
    pub(crate) last_cleanup: DateTime<Utc>,
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
            cancel_on_placed: HashSet::new(),
            cancel_token_to_market: HashMap::new(),
            orphaned_sells: HashMap::new(),
            scanner_handle: None,
            last_cleanup: DateTime::<Utc>::MIN_UTC,
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

        // Check balance before placing orders (include estimated taker fees)
        let fee_rate = polyrust_core::fees::default_taker_fee_rate();
        let fee_yes = taker_fee_per_share(opportunity.yes_ask, fee_rate) * opportunity.max_size;
        let fee_no = taker_fee_per_share(opportunity.no_ask, fee_rate) * opportunity.max_size;
        let required_usdc = opportunity.combined_cost * opportunity.max_size + fee_yes + fee_no;
        let available = ctx.balance.read().await.available_usdc;
        if available < required_usdc {
            debug!(
                market_id = %opportunity.market_id,
                %required_usdc, %available,
                "Insufficient balance for Dutch Book trade (including fees)"
            );
            return vec![];
        }

        self.execute_opportunity(opportunity, &market_entry).await
    }

    /// Create paired FOK orders for a detected arbitrage opportunity.
    async fn execute_opportunity(
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
        )
        .with_tick_size(market_entry.tick_size)
        .with_fee_rate_bps(market_entry.fee_rate_bps);

        let no_order = OrderRequest::new(
            market_entry.token_b.clone(),
            opp.no_ask,
            opp.max_size,
            OrderSide::Buy,
            OrderType::Fok,
            market_entry.neg_risk,
        )
        .with_tick_size(market_entry.tick_size)
        .with_fee_rate_bps(market_entry.fee_rate_bps);

        info!(
            market_id = %opp.market_id,
            combined_cost = %opp.combined_cost,
            profit_pct = %opp.profit_pct,
            size = %opp.max_size,
            yes_ask = %opp.yes_ask,
            no_ask = %opp.no_ask,
            "Executing Dutch Book arbitrage"
        );

        // Record opportunity in shared dashboard state
        {
            let mut state = self.shared_state.write().await;
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
            unwind_retries: 0,
            stale_unwind_ids: Vec::new(),
            remaining_unwind_size: None,
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

        // Resolve market_id from the token_id. Primary path uses the analyzer;
        // fallback uses cancel_token_to_market for markets already removed from
        // the analyzer (e.g., after expiry) but with pending cancel_on_placed.
        let (entry, market_id) =
            if let Some(e) = self.analyzer.market_for_token(&result.token_id) {
                let mid = e.market_id.clone();
                (Some(e.clone()), mid)
            } else if let Some(mid) = self.cancel_token_to_market.get(&result.token_id) {
                (None, mid.clone())
            } else {
                return vec![];
            };

        // Check cancel_on_placed — an orphaned unwind SELL from a previous execution
        // must be cancelled even if a new execution already exists for this market.
        // Only consume the flag on SELL side; BUY events from a new execution must not clear it.
        //
        // Always cancel unconditionally when the flag is set: we cannot distinguish
        // orphaned SELLs from legitimate new-execution SELLs by order attributes alone.
        // If a new execution was awaiting its SELL, re-dispatch the unwind after consuming
        // the flag so the re-dispatched SELL arrives with cancel_on_placed already cleared.
        if result.side == OrderSide::Sell && self.cancel_on_placed.contains(&market_id) {
            self.cancel_on_placed.remove(&market_id);
            // Clean up the fallback token mappings for this market
            self.cancel_token_to_market
                .retain(|_, mid| mid != &market_id);

            // Track the cancelled order so late fills (if cancel races with fill) are
            // detected and accounted for rather than silently dropped.
            self.orphaned_sells
                .insert(order_id.clone(), market_id.clone());

            let mut actions = vec![Action::CancelOrder(order_id.clone())];

            let needs_redispatch = self
                .active_executions
                .get(&market_id)
                .is_some_and(|exec| {
                    matches!(
                        &exec.state,
                        ExecutionState::Unwinding { sell_order_id } if sell_order_id.is_empty()
                    )
                });

            if needs_redispatch {
                warn!(
                    %market_id, %order_id,
                    "Cancelling ambiguous SELL (cancel_on_placed) — re-dispatching unwind for active execution"
                );
                actions.extend(self.start_unwind_redispatch(&market_id));
            } else {
                warn!(
                    %market_id, %order_id,
                    "Cancelling orphaned unwind order (execution removed before Placed event)"
                );
            }

            return actions;
        }

        // Past this point we need the full MarketEntry from the analyzer.
        // If entry is None (resolved via cancel_token_to_market fallback only),
        // there's no active execution to track — the market was already removed.
        let entry = match entry {
            Some(e) => e,
            None => return vec![],
        };

        if !self.active_executions.contains_key(&market_id) {
            return vec![];
        }

        if let Some(exec) = self.active_executions.get_mut(&market_id) {
            // Check if this is an unwind sell order:
            // - Unwinding with empty sell_order_id (pending Placed event) + SELL side
            let is_pending_unwind = matches!(
                &exec.state,
                ExecutionState::Unwinding { sell_order_id } if sell_order_id.is_empty()
            ) && result.side == OrderSide::Sell;

            if is_pending_unwind {
                exec.state = ExecutionState::Unwinding {
                    sell_order_id: order_id.clone(),
                };
                self.order_to_market
                    .insert(order_id, market_id.clone());
                debug!(
                    %market_id,
                    state = ?exec.state,
                    "Unwind sell order placed"
                );
            } else if matches!(&exec.state, ExecutionState::Unwinding { sell_order_id } if !sell_order_id.is_empty())
                && result.side == OrderSide::Sell
            {
                // Stale SELL from a previous cancel_on_placed re-dispatch cycle.
                // The execution already has its sell tracked — cancel this one to
                // prevent an unmanaged live order on the exchange.
                // Track the order for late-fill handling so it isn't silently dropped.
                exec.stale_unwind_ids.push(order_id.clone());
                self.order_to_market
                    .insert(order_id.clone(), market_id.clone());
                warn!(
                    %market_id, %order_id,
                    "Cancelling late SELL Placed — execution already tracking a sell order"
                );
                return vec![Action::CancelOrder(order_id)];
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

    /// Re-dispatch the unwind for an execution that had its SELL cancelled by
    /// cancel_on_placed. Generates a new PlaceOrder SELL without changing the
    /// execution state (it remains Unwinding with empty sell_order_id).
    fn start_unwind_redispatch(&self, market_id: &str) -> Vec<Action> {
        let exec = match self.active_executions.get(market_id) {
            Some(e) => e,
            None => return vec![],
        };

        let entry = match self.analyzer.market_for_market_id(market_id) {
            Some(e) => e.clone(),
            None => return vec![],
        };

        let (filled_token_id, fill_price) = match (&exec.yes_fill_price, &exec.no_fill_price) {
            (Some(p), None) => (entry.token_a.clone(), *p),
            (None, Some(p)) => (entry.token_b.clone(), *p),
            _ => return vec![],
        };

        let sell_price = fill_price * (Decimal::ONE - self.config.unwind_discount);

        let sell_order = OrderRequest::new(
            filled_token_id,
            sell_price,
            exec.size,
            OrderSide::Sell,
            OrderType::Gtc,
            entry.neg_risk,
        )
        .with_tick_size(entry.tick_size)
        .with_fee_rate_bps(entry.fee_rate_bps);

        vec![Action::PlaceOrder(sell_order)]
    }

    /// Handle an order fill event — update execution state.
    pub(crate) async fn handle_order_filled(
        &mut self,
        order_id: &str,
        token_id: &str,
        price: Decimal,
        size: Decimal,
    ) -> Vec<Action> {
        // Check if this is an unwind order fill
        if let Some(actions) = self.handle_unwind_order_event(order_id, true).await {
            return actions;
        }

        // Check if this is a late fill from an orphaned sell (cancel raced with fill).
        // Log the double-sell for accounting — we can't undo the fill, but we can track it.
        if let Some(market_id) = self.orphaned_sells.remove(order_id) {
            warn!(
                %market_id, %order_id, %price, %size,
                "Late fill from orphaned sell order (cancel failed) — double-sell detected"
            );
            return vec![Action::Log {
                level: LogLevel::Error,
                message: format!(
                    "Dutch Book DOUBLE-SELL for market {market_id}: orphaned sell {order_id} filled at {price} (size {size}) after cancel failed"
                ),
            }];
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
            exec.state.clone().fill_yes(order_id.to_string())
        } else {
            exec.state.clone().fill_no(order_id.to_string())
        };
        exec.state = new_state;

        debug!(
            %market_id, %order_id, %token_id, %price, %size,
            is_yes = is_yes_side,
            state = ?exec.state,
            "Dutch Book order filled"
        );

        // Check resulting state
        if exec.state == ExecutionState::BothFilled {
            self.promote_to_position(&market_id)
        } else if exec.state.needs_unwind() {
            // Fill arrived after the other side was cancelled → partial fill
            self.start_emergency_unwind(&market_id)
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

        // Look up market entry for token IDs and neg_risk
        let entry = match self.analyzer.market_for_market_id(market_id) {
            Some(e) => e.clone(),
            None => return vec![],
        };

        let (yes_price, no_price) = match (exec.yes_fill_price, exec.no_fill_price) {
            (Some(y), Some(n)) => (y, n),
            _ => {
                warn!(
                    %market_id,
                    yes_fill = ?exec.yes_fill_price,
                    no_fill = ?exec.no_fill_price,
                    "Promoting to position without both fill prices — skipping"
                );
                return vec![];
            }
        };
        let fee_rate = polyrust_core::fees::default_taker_fee_rate();
        let total_fees = (taker_fee_per_share(yes_price, fee_rate)
            + taker_fee_per_share(no_price, fee_rate))
            * exec.size;
        let combined_cost = (yes_price + no_price) * exec.size + total_fees;
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
            yes_token_id: entry.token_a,
            no_token_id: entry.token_b,
            neg_risk: entry.neg_risk,
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
    pub(crate) async fn handle_order_cancelled(&mut self, order_id: &str) -> Vec<Action> {
        // Check if this is an unwind order completion (cancel/timeout of sell)
        if let Some(actions) = self.handle_unwind_order_event(order_id, false).await {
            return actions;
        }

        // Clean up orphaned sell tracking on successful cancel (no late fill risk)
        self.orphaned_sells.remove(order_id);

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

        match &exec.state {
            // Both cancelled → clean up
            ExecutionState::Complete => {
                info!(%market_id, "Both Dutch Book orders cancelled — opportunity missed");
                let exec = self.active_executions.remove(&market_id).unwrap();
                self.order_to_market.remove(&exec.yes_order_id);
                self.order_to_market.remove(&exec.no_order_id);
                vec![]
            }
            // One side cancelled, other side not yet reported → wait for second event
            ExecutionState::OneCancelled { .. } => {
                debug!(%market_id, state = ?exec.state, "One side cancelled, awaiting other side's event");
                vec![]
            }
            // Partial fill (one cancelled + other filled) → unwind
            _ if exec.state.needs_unwind() => {
                self.start_emergency_unwind(&market_id.clone())
            }
            _ => vec![],
        }
    }

    /// Handle a CancelFailed event -- the cancel we requested did not succeed,
    /// meaning the order is still live on the exchange. Re-insert tracking if we
    /// had removed it (e.g., orphaned_sells removed on Cancelled, but cancel failed
    /// means the order is still live).
    async fn handle_cancel_failed(&mut self, order_id: &str, reason: &str) -> Vec<Action> {
        // Check if this was an orphaned sell we thought was cancelled.
        // Since the cancel failed, the order is still live -- re-insert into orphaned_sells
        // so late fills are still tracked.
        if let Some(market_id) = self.order_to_market.get(order_id) {
            let market_id = market_id.clone();
            warn!(
                %market_id, %order_id, %reason,
                "CancelFailed for Dutch Book order — order is still live"
            );

            // If this was a cancel_on_placed cancellation that failed, the sell order
            // is still live. Track it in orphaned_sells for late-fill detection.
            if !self.orphaned_sells.contains_key(order_id)
                && !self.active_executions.values().any(|exec| {
                    matches!(&exec.state, ExecutionState::Unwinding { sell_order_id } if sell_order_id == order_id)
                })
            {
                self.orphaned_sells
                    .insert(order_id.to_string(), market_id);
            }
        } else {
            // Order not tracked by us -- likely already cleaned up
            debug!(%order_id, %reason, "CancelFailed for unknown order — ignoring");
        }

        vec![]
    }

    /// Handle a batch rejection where order_id is None.
    /// Uses token_id to locate the active execution and clean it up.
    async fn handle_batch_rejection(&mut self, token_id: &str) -> Vec<Action> {
        let market_id = match self.analyzer.market_for_token(token_id) {
            Some(entry) => entry.market_id.clone(),
            None => return vec![],
        };

        let exec = match self.active_executions.get(&market_id) {
            Some(e) => e,
            None => return vec![],
        };

        // If one side already filled, this is a partial fill needing unwind
        if exec.yes_fill_price.is_some() != exec.no_fill_price.is_some() {
            let filled_side = if exec.yes_fill_price.is_some() {
                FilledSide::Yes
            } else {
                FilledSide::No
            };
            let filled_order_id = if exec.yes_fill_price.is_some() {
                exec.yes_order_id.clone()
            } else {
                exec.no_order_id.clone()
            };

            let exec = self.active_executions.get_mut(&market_id).unwrap();
            exec.state = ExecutionState::PartialFill {
                filled_side,
                filled_order_id,
            };

            warn!(
                %market_id, %token_id,
                "Batch rejection with partial fill — triggering emergency unwind"
            );
            return self.start_emergency_unwind(&market_id);
        }

        // No fills on either side — clean up the execution entirely
        warn!(
            %market_id, %token_id,
            "Batch rejection with no fills — removing stale execution"
        );
        let exec = self.active_executions.remove(&market_id).unwrap();
        self.order_to_market.remove(&exec.yes_order_id);
        self.order_to_market.remove(&exec.no_order_id);
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

        let entry = match self.analyzer.market_for_market_id(market_id) {
            Some(e) => e.clone(),
            None => return vec![],
        };

        let (filled_side, filled_token_id, fill_price) = match &exec.state {
            ExecutionState::PartialFill {
                filled_side,
                filled_order_id: _,
            } => {
                let (token_id, price) = match filled_side {
                    FilledSide::Yes => (entry.token_a.clone(), exec.yes_fill_price),
                    FilledSide::No => (entry.token_b.clone(), exec.no_fill_price),
                };
                let Some(price) = price else {
                    warn!(
                        %market_id,
                        ?filled_side,
                        "PartialFill without fill price — cannot unwind safely"
                    );
                    return vec![];
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

        // Transition to Unwinding immediately to prevent duplicate unwind orders
        // from cleanup_stale_executions firing before the Placed event arrives.
        // The sell_order_id will be updated when handle_order_placed receives the Placed event.
        let exec = self.active_executions.get_mut(market_id).unwrap();
        exec.state = ExecutionState::Unwinding {
            sell_order_id: String::new(),
        };

        // Initialize remaining_unwind_size on the first unwind; subsequent retries
        // use the already-decremented value (reduced by partial fills).
        let unwind_size = exec.remaining_unwind_size.unwrap_or(exec.size);
        if exec.remaining_unwind_size.is_none() {
            exec.remaining_unwind_size = Some(exec.size);
        }

        // Build GTC SELL order for the filled side
        let sell_order = OrderRequest::new(
            filled_token_id,
            sell_price,
            unwind_size,
            OrderSide::Sell,
            OrderType::Gtc,
            entry.neg_risk,
        )
        .with_tick_size(entry.tick_size)
        .with_fee_rate_bps(entry.fee_rate_bps);

        vec![Action::PlaceOrder(sell_order)]
    }

    /// Handle a fill or cancel event for an unwind order.
    /// Returns Some(actions) if the order_id matched an unwinding execution, None otherwise.
    ///
    /// Checks both the current unwind sell_order_id and stale_unwind_ids (from previous
    /// retry attempts) to handle late fills that arrive after an unwind was cancelled.
    async fn handle_unwind_order_event(&mut self, order_id: &str, is_fill: bool) -> Option<Vec<Action>> {
        // Find which execution this unwind order belongs to.
        // Check both the active sell_order_id and stale IDs from previous retries.
        let (market_id, is_stale) = {
            let mut found = None;
            for (mid, exec) in &self.active_executions {
                if let ExecutionState::Unwinding { sell_order_id } = &exec.state
                    && sell_order_id == order_id
                {
                    found = Some((mid.clone(), false));
                    break;
                }
                if exec.stale_unwind_ids.iter().any(|id| id == order_id) {
                    found = Some((mid.clone(), true));
                    break;
                }
            }
            found?
        };

        if is_fill && is_stale {
            // Late fill from a previously-cancelled unwind order. The tokens were sold
            // by the old order, so the current retry unwind is now redundant.
            // Cancel the active unwind order and complete the execution.
            let exec = self.active_executions.remove(&market_id)?;
            self.order_to_market.remove(&exec.yes_order_id);
            self.order_to_market.remove(&exec.no_order_id);
            self.order_to_market.remove(order_id);
            for stale_id in &exec.stale_unwind_ids {
                self.order_to_market.remove(stale_id.as_str());
            }

            // Cancel the current active unwind order (it's now redundant).
            // If sell_order_id is empty, a PlaceOrder was dispatched but the Placed event
            // hasn't arrived yet — mark for cancellation when it does.
            let mut actions = Vec::new();
            if let ExecutionState::Unwinding { sell_order_id } = &exec.state {
                if !sell_order_id.is_empty() {
                    self.order_to_market.remove(sell_order_id.as_str());
                    // Track in orphaned_sells: if the cancel races with a fill,
                    // the late fill will be detected rather than silently dropped.
                    self.orphaned_sells
                        .insert(sell_order_id.clone(), market_id.clone());
                    actions.push(Action::CancelOrder(sell_order_id.clone()));
                } else {
                    // PlaceOrder dispatched but Placed event pending — cancel on arrival
                    self.cancel_on_placed.insert(market_id.clone());
                }
            }

            let fee_rate = polyrust_core::fees::default_taker_fee_rate();
            let loss = match (&exec.yes_fill_price, &exec.no_fill_price) {
                (Some(p), None) => {
                    let buy_cost = (*p + taker_fee_per_share(*p, fee_rate)) * exec.size;
                    let sell_proceeds = *p * (Decimal::ONE - self.config.unwind_discount) * exec.size;
                    buy_cost - sell_proceeds
                }
                (None, Some(p)) => {
                    let buy_cost = (*p + taker_fee_per_share(*p, fee_rate)) * exec.size;
                    let sell_proceeds = *p * (Decimal::ONE - self.config.unwind_discount) * exec.size;
                    buy_cost - sell_proceeds
                }
                _ => Decimal::ZERO,
            };

            warn!(
                %market_id, %order_id,
                "Late fill from stale unwind order — completing unwind, cancelling active retry"
            );

            {
                let mut state = self.shared_state.write().await;
                state.total_unwind_losses += loss;
            }

            actions.push(Action::Log {
                level: LogLevel::Warn,
                message: format!(
                    "Dutch Book unwind for market {market_id} (late fill from stale order): ~{loss} USDC loss"
                ),
            });
            return Some(actions);
        }

        if is_stale && !is_fill {
            // Cancel event for an already-stale unwind order — just clean up the stale ID
            if let Some(exec) = self.active_executions.get_mut(&market_id) {
                exec.stale_unwind_ids.retain(|id| id != order_id);
            }
            self.order_to_market.remove(order_id);
            return Some(vec![]);
        }

        if is_fill {
            // Unwind complete — clean up
            let exec = self.active_executions.remove(&market_id)?;
            self.order_to_market.remove(&exec.yes_order_id);
            self.order_to_market.remove(&exec.no_order_id);
            self.order_to_market.remove(order_id);
            for stale_id in &exec.stale_unwind_ids {
                self.order_to_market.remove(stale_id.as_str());
            }

            // Loss = buy cost (price + taker fee) - sell proceeds (price * (1 - discount))
            // The sell is GTC (maker, 0% fee), so sell proceeds = price * (1 - discount) * size
            let fee_rate = polyrust_core::fees::default_taker_fee_rate();
            let loss = match (&exec.yes_fill_price, &exec.no_fill_price) {
                (Some(p), None) => {
                    let buy_cost = (*p + taker_fee_per_share(*p, fee_rate)) * exec.size;
                    let sell_proceeds = *p * (Decimal::ONE - self.config.unwind_discount) * exec.size;
                    buy_cost - sell_proceeds
                }
                (None, Some(p)) => {
                    let buy_cost = (*p + taker_fee_per_share(*p, fee_rate)) * exec.size;
                    let sell_proceeds = *p * (Decimal::ONE - self.config.unwind_discount) * exec.size;
                    buy_cost - sell_proceeds
                }
                _ => Decimal::ZERO,
            };

            info!(
                %market_id,
                %loss,
                "Emergency unwind complete — realized loss"
            );

            // Record unwind loss in shared state
            {
                let mut state = self.shared_state.write().await;
                state.total_unwind_losses += loss;
            }

            Some(vec![Action::Log {
                level: LogLevel::Warn,
                message: format!(
                    "Dutch Book unwind for market {market_id}: ~{loss} USDC loss"
                ),
            }])
        } else {
            // Unwind order cancelled/rejected — retry or give up
            let exec = self.active_executions.get_mut(&market_id)?;
            exec.unwind_retries += 1;

            if exec.unwind_retries >= super::types::MAX_UNWIND_RETRIES {
                // Max retries exceeded — give up, clean up, and record estimated loss
                warn!(
                    %market_id, %order_id,
                    retries = exec.unwind_retries,
                    "Emergency unwind failed after max retries — cleaning up execution"
                );

                let exec = self.active_executions.remove(&market_id)?;
                self.order_to_market.remove(&exec.yes_order_id);
                self.order_to_market.remove(&exec.no_order_id);
                self.order_to_market.remove(order_id);
                for stale_id in &exec.stale_unwind_ids {
                    self.order_to_market.remove(stale_id.as_str());
                }

                // Estimate worst-case loss as the full buy cost (position becomes worthless)
                let fee_rate = polyrust_core::fees::default_taker_fee_rate();
                let loss = match (&exec.yes_fill_price, &exec.no_fill_price) {
                    (Some(p), None) => (*p + taker_fee_per_share(*p, fee_rate)) * exec.size,
                    (None, Some(p)) => (*p + taker_fee_per_share(*p, fee_rate)) * exec.size,
                    _ => Decimal::ZERO,
                };

                {
                    let mut state = self.shared_state.write().await;
                    state.total_unwind_losses += loss;
                }

                Some(vec![Action::Log {
                    level: LogLevel::Error,
                    message: format!(
                        "Dutch Book unwind FAILED for market {market_id} after {} retries: ~{loss} USDC estimated loss (position stranded)",
                        exec.unwind_retries
                    ),
                }])
            } else {
                // Retry: track the old sell_order_id as stale (for late fill handling),
                // transition back to PartialFill and re-trigger unwind.
                exec.stale_unwind_ids.push(order_id.to_string());

                let (filled_side, filled_order_id) = match (&exec.yes_fill_price, &exec.no_fill_price) {
                    (Some(_), None) => (FilledSide::Yes, exec.yes_order_id.clone()),
                    (None, Some(_)) => (FilledSide::No, exec.no_order_id.clone()),
                    _ => {
                        // Should not happen — clean up defensively
                        let exec = self.active_executions.remove(&market_id)?;
                        self.order_to_market.remove(&exec.yes_order_id);
                        self.order_to_market.remove(&exec.no_order_id);
                        self.order_to_market.remove(order_id);
                        for stale_id in &exec.stale_unwind_ids {
                            self.order_to_market.remove(stale_id.as_str());
                        }
                        return Some(vec![]);
                    }
                };

                warn!(
                    %market_id, %order_id,
                    retry = exec.unwind_retries,
                    ?filled_side,
                    "Emergency unwind order cancelled — retrying"
                );

                exec.state = ExecutionState::PartialFill {
                    filled_side,
                    filled_order_id,
                };
                // Reset submitted_at so the retry gets a fresh timeout window
                // instead of being immediately detected as stale.
                exec.submitted_at = Utc::now();

                let actions = self.start_emergency_unwind(&market_id);
                Some(actions)
            }
        }
    }

    /// Handle a market expiration event.
    async fn handle_market_expired(&mut self, market_id: &str) -> Vec<Action> {
        // Check if we have an open position that should be redeemed
        let result = if let Some(pos) = self.open_positions.remove(market_id) {
            info!(
                %market_id,
                expected_profit = %pos.expected_profit,
                "Market expired with open Dutch Book position — requesting redemption"
            );

            // Record expected profit pending redemption.
            // Note: profit is not truly realized until RedeemPosition succeeds.
            // The event system does not currently provide a redemption result event,
            // so we record optimistically here. If redemption fails, this overstates P&L.
            {
                let mut state = self.shared_state.write().await;
                state.total_realized_pnl += pos.expected_profit;
            }

            vec![Action::RedeemPosition(RedeemRequest {
                market_id: market_id.to_string(),
                condition_id: market_id.to_string(),
                token_ids: vec![pos.yes_token_id, pos.no_token_id],
                neg_risk: pos.neg_risk,
            })]
        } else {
            // Clean up any active execution for this market (including unwind mappings)
            let mut cleanup_actions = Vec::new();
            if let Some(exec) = self.active_executions.remove(market_id) {
                self.order_to_market.remove(&exec.yes_order_id);
                self.order_to_market.remove(&exec.no_order_id);
                if let ExecutionState::Unwinding { ref sell_order_id } = exec.state {
                    if !sell_order_id.is_empty() {
                        self.order_to_market.remove(sell_order_id);
                        // Track in orphaned_sells for late-fill detection
                        self.orphaned_sells
                            .insert(sell_order_id.clone(), market_id.to_string());
                        cleanup_actions.push(Action::CancelOrder(sell_order_id.clone()));
                    } else {
                        // PlaceOrder SELL was dispatched but Placed event hasn't arrived.
                        // Track for cancellation when it does.
                        self.cancel_on_placed.insert(market_id.to_string());
                        // Preserve token→market mapping so handle_order_placed can
                        // resolve the market_id after the analyzer removes this market.
                        if let Some(entry) = self.analyzer.market_for_market_id(market_id) {
                            self.cancel_token_to_market
                                .insert(entry.token_a.clone(), market_id.to_string());
                            self.cancel_token_to_market
                                .insert(entry.token_b.clone(), market_id.to_string());
                        }
                    }
                }
                for stale_id in &exec.stale_unwind_ids {
                    self.order_to_market.remove(stale_id.as_str());
                }
            }
            cleanup_actions
        };

        // Remove market from analyzer after extracting any needed data
        self.analyzer.remove_market(market_id);

        result
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

    /// Clean up stale executions that have exceeded the timeout.
    /// For partial fills, triggers emergency unwind to sell the filled side.
    /// For executions with no fills, removes them cleanly.
    /// Rate-limited to run at most once every CLEANUP_INTERVAL_SECS.
    async fn cleanup_stale_executions(&mut self) -> Vec<Action> {
        let now = Utc::now();
        if now - self.last_cleanup < Duration::seconds(CLEANUP_INTERVAL_SECS) {
            return vec![];
        }
        self.last_cleanup = now;
        let timeout = Duration::seconds(EXECUTION_TIMEOUT_SECS);
        let stale_ids: Vec<MarketId> = self
            .active_executions
            .iter()
            .filter(|(_, exec)| now - exec.submitted_at > timeout)
            .map(|(mid, _)| mid.clone())
            .collect();

        let mut actions = vec![];
        for market_id in stale_ids {
            let exec = self.active_executions.get(&market_id).unwrap();

            // For Unwinding executions, allow extra time but eventually clean up
            // to prevent permanent resource leaks (e.g., sell order event was lost).
            if let ExecutionState::Unwinding { ref sell_order_id } = exec.state {
                let extended_timeout = Duration::seconds(EXECUTION_TIMEOUT_SECS * 3);
                if now - exec.submitted_at > extended_timeout {
                    let sell_id = sell_order_id.clone();
                    warn!(
                        %market_id,
                        age_secs = (now - exec.submitted_at).num_seconds(),
                        "Unwinding execution stuck beyond extended timeout — forcing cleanup"
                    );
                    let exec = self.active_executions.remove(&market_id).unwrap();
                    self.order_to_market.remove(&exec.yes_order_id);
                    self.order_to_market.remove(&exec.no_order_id);
                    // Cancel the live sell order so it doesn't become unmanaged
                    if !sell_id.is_empty() {
                        self.order_to_market.remove(&sell_id);
                        // Track in orphaned_sells for late-fill detection
                        self.orphaned_sells
                            .insert(sell_id.clone(), market_id.clone());
                        actions.push(Action::CancelOrder(sell_id));
                    } else {
                        // PlaceOrder was dispatched but Placed event never arrived (or is
                        // still in flight). Track for cancellation if it arrives later.
                        self.cancel_on_placed.insert(market_id.clone());
                        // Preserve token→market mapping so handle_order_placed can
                        // resolve the market_id after the analyzer removes this market.
                        if let Some(entry) = self.analyzer.market_for_market_id(&market_id) {
                            self.cancel_token_to_market
                                .insert(entry.token_a.clone(), market_id.clone());
                            self.cancel_token_to_market
                                .insert(entry.token_b.clone(), market_id.clone());
                        }
                    }
                    for stale_id in &exec.stale_unwind_ids {
                        self.order_to_market.remove(stale_id.as_str());
                    }

                    // Record estimated worst-case loss (full buy cost, position may be stranded)
                    let fee_rate = polyrust_core::fees::default_taker_fee_rate();
                    let loss = match (&exec.yes_fill_price, &exec.no_fill_price) {
                        (Some(p), None) => (*p + taker_fee_per_share(*p, fee_rate)) * exec.size,
                        (None, Some(p)) => (*p + taker_fee_per_share(*p, fee_rate)) * exec.size,
                        _ => Decimal::ZERO,
                    };
                    if loss > Decimal::ZERO {
                        let mut state = self.shared_state.write().await;
                        state.total_unwind_losses += loss;
                    }
                }
                continue;
            }

            let has_yes_fill = exec.yes_fill_price.is_some();
            let has_no_fill = exec.no_fill_price.is_some();

            warn!(
                %market_id,
                state = ?exec.state,
                age_secs = (now - exec.submitted_at).num_seconds(),
                has_yes_fill, has_no_fill,
                "Stale Dutch Book execution timed out"
            );

            if has_yes_fill != has_no_fill {
                // One side filled, one side not — force into PartialFill and unwind
                let filled_side = if has_yes_fill {
                    FilledSide::Yes
                } else {
                    FilledSide::No
                };
                let filled_order_id = if has_yes_fill {
                    exec.yes_order_id.clone()
                } else {
                    exec.no_order_id.clone()
                };

                // Update the execution state to PartialFill so start_emergency_unwind works
                let exec = self.active_executions.get_mut(&market_id).unwrap();
                exec.state = ExecutionState::PartialFill {
                    filled_side,
                    filled_order_id,
                };

                warn!(
                    %market_id,
                    "Triggering emergency unwind for timed-out partial fill"
                );
                actions.extend(self.start_emergency_unwind(&market_id));
            } else {
                // No fills or both fills (BothFilled shouldn't timeout, but handle gracefully)
                let exec = self.active_executions.remove(&market_id).unwrap();
                self.order_to_market.remove(&exec.yes_order_id);
                self.order_to_market.remove(&exec.no_order_id);
            }
        }
        actions
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

    async fn on_start(&mut self, ctx: &StrategyContext) -> Result<()> {
        info!(
            max_combined_cost = %self.config.max_combined_cost,
            min_profit = %self.config.min_profit_threshold,
            max_positions = self.config.max_concurrent_positions,
            scan_interval = self.config.scan_interval_secs,
            "Dutch Book strategy started"
        );

        // Skip live Gamma API scanner in backtest mode (simulated_clock is set).
        // In backtest, markets are pre-loaded by the engine — no live discovery needed.
        let is_backtest = ctx.simulated_clock.read().await.is_some();
        if !is_backtest {
            let handle = GammaScanner::start_scanner(
                self.config.clone(),
                Arc::clone(&self.pending_subscriptions),
                Arc::clone(&self.known_market_ids),
            );
            self.scanner_handle = Some(handle);
        } else {
            info!("Backtest mode — skipping live Gamma scanner");
        }

        Ok(())
    }

    async fn on_event(&mut self, event: &Event, ctx: &StrategyContext) -> Result<Vec<Action>> {
        // Always drain pending subscriptions on any event
        let mut actions = self.drain_pending_subscriptions().await;

        // Periodically clean up stale executions
        actions.extend(self.cleanup_stale_executions().await);

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
            }) => self.handle_order_filled(order_id, token_id, *price, *size).await,

            Event::OrderUpdate(OrderEvent::Cancelled(order_id)) => {
                self.handle_order_cancelled(order_id).await
            }

            Event::OrderUpdate(OrderEvent::PartiallyFilled {
                order_id,
                filled_size,
                remaining_size,
            }) => {
                // GTC unwind orders can receive partial fills. Track remaining size
                // so retries use the correct (reduced) amount instead of the original.
                if let Some(market_id) = self.order_to_market.get(order_id.as_str()).cloned() {
                    warn!(
                        %order_id, %filled_size, %remaining_size, %market_id,
                        "Dutch Book unwind order partially filled"
                    );
                    if let Some(exec) = self.active_executions.get_mut(&market_id) {
                        exec.remaining_unwind_size = Some(*remaining_size);
                    }
                }
                vec![]
            }

            Event::OrderUpdate(OrderEvent::CancelFailed { order_id, reason }) => {
                self.handle_cancel_failed(order_id, reason).await
            }

            Event::OrderUpdate(OrderEvent::Rejected { order_id, reason, token_id }) => {
                if let Some(oid) = order_id {
                    debug!(%oid, %reason, "Dutch Book order rejected");
                    self.handle_order_cancelled(oid).await
                } else if let Some(tid) = token_id {
                    // Batch failure: order_id is None but token_id is available.
                    // Use token_id to find the active execution and clean it up.
                    debug!(%tid, %reason, "Dutch Book batch order rejected (no order_id)");
                    self.handle_batch_rejection(tid).await
                } else {
                    vec![]
                }
            }

            Event::MarketData(MarketDataEvent::MarketExpired(market_id)) => {
                self.handle_market_expired(market_id).await
            }

            _ => vec![],
        };

        actions.extend(event_actions);

        // Sync dashboard state when there's active state to display
        if !actions.is_empty() || !self.open_positions.is_empty() || !self.active_executions.is_empty() {
            self.sync_dashboard_state().await;
        }

        Ok(actions)
    }

    async fn on_stop(&mut self, _ctx: &StrategyContext) -> Result<Vec<Action>> {
        // Abort the scanner
        if let Some(handle) = self.scanner_handle.take() {
            handle.abort();
            info!("Dutch Book scanner stopped");
        }

        // Cancel any live unwind sell orders to prevent orphaned orders on the exchange
        let mut actions = Vec::new();
        for (market_id, exec) in &self.active_executions {
            if let ExecutionState::Unwinding { sell_order_id } = &exec.state
                && !sell_order_id.is_empty()
            {
                warn!(
                    %market_id, %sell_order_id,
                    "Cancelling live unwind order on strategy stop"
                );
                actions.push(Action::CancelOrder(sell_order_id.clone()));
            }
        }

        info!(
            open_positions = self.open_positions.len(),
            active_executions = self.active_executions.len(),
            cancelled_unwinds = actions.len(),
            "Dutch Book strategy stopped"
        );

        Ok(actions)
    }
}
