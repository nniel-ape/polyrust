use crate::config::AutoClaimConfig;
use crate::context::StrategyContext;
use crate::error::Result;
use crate::event_bus::EventBus;
use crate::events::{Event, MarketDataEvent, OrderEvent, SignalEvent};
use crate::execution::{ExecutionBackend, RedeemRequest};
use crate::types::MarketId;
use chrono::{DateTime, Duration, Utc};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Cached metadata from MarketDiscovered events, retained after MarketExpired
/// so the claim monitor can still look up condition_id, neg_risk, and token_ids.
#[derive(Debug, Clone)]
struct MarketMeta {
    neg_risk: bool,
    token_ids: Vec<String>,
}

/// A pending claim waiting for market resolution
#[derive(Debug, Clone)]
struct PendingClaim {
    market_id: MarketId,
    condition_id: String,
    neg_risk: bool,
    token_ids: Vec<String>,
    _expired_at: DateTime<Utc>,
    attempts: u32,
    next_check: DateTime<Utc>,
    /// Set when on-chain resolution is first detected; `None` while unresolved.
    resolved_at: Option<DateTime<Utc>>,
}

/// Background monitor that polls for resolved markets and triggers redemption
pub struct ClaimMonitor {
    pending: Arc<RwLock<HashMap<MarketId, PendingClaim>>>,
    known_markets: Arc<RwLock<HashMap<MarketId, MarketMeta>>>,
    /// Markets where we've had at least one fill — only these get queued on expiry.
    traded_markets: Arc<RwLock<HashSet<MarketId>>>,
    config: AutoClaimConfig,
    event_bus: EventBus,
    execution: Arc<dyn ExecutionBackend>,
    /// Shared strategy context — used as fallback when known_markets cache misses.
    context: StrategyContext,
    /// Gas circuit breaker: pauses all redemptions until this time when MATIC is insufficient.
    gas_paused_until: Arc<RwLock<Option<DateTime<Utc>>>>,
}

impl ClaimMonitor {
    /// Create a new ClaimMonitor
    pub fn new(
        config: AutoClaimConfig,
        event_bus: EventBus,
        execution: Arc<dyn ExecutionBackend>,
        context: StrategyContext,
    ) -> Self {
        Self {
            pending: Arc::new(RwLock::new(HashMap::new())),
            known_markets: Arc::new(RwLock::new(HashMap::new())),
            traded_markets: Arc::new(RwLock::new(HashSet::new())),
            config,
            event_bus,
            execution,
            context,
            gas_paused_until: Arc::new(RwLock::new(None)),
        }
    }

    /// Start the claim monitor background task
    pub async fn run(self: Arc<Self>) -> Result<()> {
        info!(
            "ClaimMonitor started (poll interval: {}s)",
            self.config.poll_interval_secs
        );

        // Subscribe to market events (both MarketDiscovered and MarketExpired)
        let mut discovery_events = self.event_bus.subscribe_topics(&["market_data"]);
        let mut expiry_events = self.event_bus.subscribe_topics(&["market_data"]);

        // Spawn the discovery listener — caches MarketInfo for later use
        let self_discovery = self.clone();
        tokio::spawn(async move {
            while let Some(event) = discovery_events.recv().await {
                if let Event::MarketData(MarketDataEvent::MarketDiscovered(info)) = event {
                    let meta = MarketMeta {
                        neg_risk: info.neg_risk,
                        token_ids: vec![
                            info.token_ids.outcome_a.clone(),
                            info.token_ids.outcome_b.clone(),
                        ],
                    };
                    self_discovery
                        .known_markets
                        .write()
                        .await
                        .insert(info.id.clone(), meta);
                    debug!(market_id = %info.id, neg_risk = info.neg_risk, "Cached market metadata");
                }
            }
        });

        // Spawn the fill listener — tracks which markets we've actually traded
        let mut fill_events = self.event_bus.subscribe_topics(&["order_update"]);
        let self_fills = self.clone();
        tokio::spawn(async move {
            while let Some(event) = fill_events.recv().await {
                if let Event::OrderUpdate(OrderEvent::Filled { market_id, .. }) = event {
                    let is_new = self_fills
                        .traded_markets
                        .write()
                        .await
                        .insert(market_id.clone());
                    if is_new {
                        info!(market_id = %market_id, "Recorded fill for claim tracking");
                    }
                }
            }
        });

        // Spawn the expiry listener — adds expired markets to pending queue
        let self_expiry = self.clone();
        tokio::spawn(async move {
            while let Some(event) = expiry_events.recv().await {
                if let Event::MarketData(MarketDataEvent::MarketExpired(market_id)) = event {
                    self_expiry.on_market_expired(market_id).await;
                }
            }
        });

        // Spawn matched-fill signal listener — detects GTC orders filled by counterparties
        let mut signal_events = self.event_bus.subscribe_topics(&["signal"]);
        let self_signals = self.clone();
        tokio::spawn(async move {
            while let Some(event) = signal_events.recv().await {
                if let Event::Signal(SignalEvent {
                    signal_type,
                    payload,
                    ..
                }) = event
                    && (signal_type == "matched-fill" || signal_type == "reconciled-fill")
                    && let Some(market_id) = payload.get("market_id").and_then(|v| v.as_str())
                {
                    let is_new = self_signals
                        .traded_markets
                        .write()
                        .await
                        .insert(market_id.to_string());
                    if is_new {
                        info!(
                            market_id = %market_id,
                            "Recorded {signal_type} for claim tracking"
                        );
                    }
                }
            }
        });

        // Main polling loop
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
            self.config.poll_interval_secs,
        ));

        loop {
            interval.tick().await;
            if let Err(e) = self.check_pending_claims().await {
                error!("Error checking pending claims: {}", e);
            }
        }
    }

    /// Handle a MarketExpired event — only queue if we've traded this market.
    async fn on_market_expired(&self, market_id: MarketId) {
        // Skip markets we never filled an order on
        if !self.traded_markets.read().await.contains(&market_id) {
            debug!(market_id = %market_id, "Skipping expired market (no fills recorded)");
            return;
        }

        // MarketInfo.id IS the condition_id (see types.rs doc comment)
        let condition_id = market_id.clone();

        // Look up neg_risk and token_ids from our cached MarketDiscovered data,
        // since the engine defers removal of MarketInfo from shared state on MarketExpired.
        // Fall back to StrategyContext.market_data if our local cache missed the event.
        let (neg_risk, token_ids) = match self.known_markets.read().await.get(&market_id) {
            Some(meta) => (meta.neg_risk, meta.token_ids.clone()),
            None => {
                // Fallback: check the shared context (engine defers removal by 30s)
                let md = self.context.market_data.read().await;
                if let Some(info) = md.markets.get(&market_id) {
                    warn!(
                        market_id = %market_id,
                        "Local cache miss — resolved from shared context"
                    );
                    (
                        info.neg_risk,
                        vec![
                            info.token_ids.outcome_a.clone(),
                            info.token_ids.outcome_b.clone(),
                        ],
                    )
                } else {
                    error!(
                        market_id = %market_id,
                        "No MarketInfo in cache or shared context for expired market — claim will lack token_ids"
                    );
                    (false, vec![])
                }
            }
        };

        let claim = PendingClaim {
            market_id: market_id.clone(),
            condition_id,
            neg_risk,
            token_ids,
            _expired_at: Utc::now(),
            attempts: 0,
            next_check: Utc::now(),
            resolved_at: None,
        };

        self.pending.write().await.insert(market_id.clone(), claim);
        self.traded_markets.write().await.remove(&market_id);
        info!(market_id = %market_id, neg_risk, "Added market to pending claims queue");
    }

    /// Check all pending claims and attempt redemption for resolved markets.
    ///
    /// Three-phase polling with time-window accumulation:
    /// 1. **Check resolution** — mark newly resolved claims with a timestamp
    /// 2. **Accumulate** — skip redemption if batch window hasn't elapsed and count threshold not met
    /// 3. **Flush** — batch all resolved claims into one multiSend tx
    async fn check_pending_claims(&self) -> Result<()> {
        let now = Utc::now();

        // Gas circuit breaker: skip all redemptions if paused due to insufficient MATIC
        {
            let paused = self.gas_paused_until.read().await;
            if let Some(until) = *paused
                && now < until
            {
                let remaining = (until - now).num_seconds();
                info!(
                    remaining_secs = remaining,
                    "Redemptions paused (insufficient MATIC), skipping cycle"
                );
                return Ok(());
            }
        }

        // ── Phase 1: Snapshot under short read lock ──
        let (to_check, empty_token_ids) = {
            let pending = self.pending.read().await;

            if !pending.is_empty() {
                info!(pending_count = pending.len(), "Checking pending claims");
            }

            let mut to_check: Vec<(MarketId, String)> = Vec::new();
            let mut empty_token_ids: Vec<MarketId> = Vec::new();

            for (market_id, claim) in pending.iter() {
                if claim.resolved_at.is_some() {
                    continue;
                }
                if claim.next_check > now {
                    continue;
                }
                if claim.token_ids.is_empty() {
                    empty_token_ids.push(market_id.clone());
                    continue;
                }
                to_check.push((market_id.clone(), claim.condition_id.clone()));
            }

            (to_check, empty_token_ids)
        }; // read lock dropped

        // ── Phase 2: RPC resolution checks with no lock held ──
        let mut newly_resolved: Vec<MarketId> = Vec::new();
        let mut recheck: Vec<MarketId> = Vec::new();
        let mut rpc_errors: Vec<MarketId> = Vec::new();
        let mut gas_error = false;

        for (market_id, condition_id) in &to_check {
            debug!("Checking resolution status for market {}", market_id);

            match self.execution.is_market_resolved(condition_id).await {
                Ok(true) => {
                    info!(market_id = %market_id, "Market resolved, queuing for batch redemption");
                    newly_resolved.push(market_id.clone());
                }
                Ok(false) => {
                    debug!("Market {} not yet resolved, will recheck", market_id);
                    recheck.push(market_id.clone());
                }
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("insufficient funds for gas") {
                        gas_error = true;
                        break;
                    }
                    error!("Error checking resolution for market {}: {}", market_id, e);
                    rpc_errors.push(market_id.clone());
                }
            }
        }

        // Handle gas error outside any pending lock
        if gas_error {
            let pause_secs = self.config.gas_pause_duration_secs;
            let pause_until = now + Duration::seconds(pause_secs as i64);
            *self.gas_paused_until.write().await = Some(pause_until);
            warn!(
                pause_secs = pause_secs,
                "Insufficient MATIC — pausing redemptions for {}s", pause_secs
            );
            return Ok(());
        }

        // ── Phase 3: Apply resolution results under short write lock ──
        {
            let mut pending = self.pending.write().await;

            for id in &newly_resolved {
                if let Some(claim) = pending.get_mut(id) {
                    claim.resolved_at = Some(now);
                }
            }

            for id in &recheck {
                if let Some(claim) = pending.get_mut(id) {
                    claim.next_check =
                        now + Duration::seconds(self.config.poll_interval_secs as i64);
                }
            }

            let mut to_remove = Vec::new();

            for id in &empty_token_ids {
                if let Some(claim) = pending.get_mut(id) {
                    error!(
                        market_id = %id,
                        attempts = claim.attempts,
                        "Cannot redeem: token_ids is empty (MarketDiscovered event was likely dropped)"
                    );
                    if self.handle_retry(claim) {
                        to_remove.push(id.clone());
                    }
                }
            }

            for id in &rpc_errors {
                if let Some(claim) = pending.get_mut(id)
                    && self.handle_retry(claim)
                {
                    to_remove.push(id.clone());
                }
            }

            for id in &to_remove {
                pending.remove(id);
            }
        } // write lock dropped

        // ── Phase 4: Check flush triggers and collect requests under short read lock ──
        let settlement_delay = Duration::seconds(self.config.settlement_delay_secs as i64);
        let flush_data = {
            let pending = self.pending.read().await;

            let resolved_claims: Vec<MarketId> = pending
                .iter()
                .filter(|(_, c)| c.resolved_at.is_some_and(|t| now - t >= settlement_delay))
                .map(|(mid, _)| mid.clone())
                .collect();

            let resolved_count = resolved_claims.len();
            if resolved_count == 0 {
                return Ok(());
            }

            let oldest_resolved_at = pending
                .values()
                .filter_map(|c| c.resolved_at)
                .min()
                .unwrap(); // safe: resolved_count > 0

            let window_elapsed = self.config.batch_window_secs == 0
                || (now - oldest_resolved_at).num_seconds() >= self.config.batch_window_secs as i64;
            let count_reached = resolved_count >= self.config.batch_min_count;

            if !window_elapsed && !count_reached {
                let remaining =
                    self.config.batch_window_secs as i64 - (now - oldest_resolved_at).num_seconds();
                info!(
                    resolved_count,
                    remaining_secs = remaining,
                    batch_min_count = self.config.batch_min_count,
                    "Accumulating batch ({} claims, {}s until window)",
                    resolved_count,
                    remaining
                );
                return Ok(());
            }

            info!(
                resolved_count,
                trigger = if count_reached { "count" } else { "window" },
                "Flushing batch redemption"
            );

            let requests: Vec<RedeemRequest> = resolved_claims
                .iter()
                .filter_map(|mid| {
                    pending.get(mid).map(|claim| RedeemRequest {
                        market_id: claim.market_id.clone(),
                        condition_id: claim.condition_id.clone(),
                        token_ids: claim.token_ids.clone(),
                        neg_risk: claim.neg_risk,
                    })
                })
                .collect();

            (resolved_claims, requests)
        }; // read lock dropped

        let (resolved_claims, requests) = flush_data;

        // ── Phase 5: Batch redemption RPC with no lock held ──
        let redemption_result = self.execution.redeem_positions_batch(&requests).await;

        // Process results and publish events (no lock needed for event publishing)
        let mut to_remove = Vec::new();
        let mut failed_ids: Vec<MarketId> = Vec::new();
        let mut batch_error = false;

        match redemption_result {
            Ok(results) => {
                for result in &results {
                    if result.success && result.tx_hash.is_empty() {
                        info!(
                            market_id = %result.market_id,
                            "No CTF balance for market, removing from queue"
                        );
                        to_remove.push(result.market_id.clone());
                    } else if result.success {
                        info!(
                            "Successfully redeemed market {}: tx {}",
                            result.market_id, result.tx_hash
                        );
                        let event = Event::OrderUpdate(OrderEvent::Redeemed {
                            market_id: result.market_id.clone(),
                            tx_hash: result.tx_hash.clone(),
                            strategy_name: "auto-claim".to_string(),
                        });
                        self.event_bus.publish(event);
                        to_remove.push(result.market_id.clone());
                    } else {
                        warn!(
                            "Redemption failed for market {}: {}",
                            result.market_id, result.message
                        );
                        failed_ids.push(result.market_id.clone());
                    }
                }
            }
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("insufficient funds for gas") {
                    let pause_secs = self.config.gas_pause_duration_secs;
                    let pause_until = now + Duration::seconds(pause_secs as i64);
                    *self.gas_paused_until.write().await = Some(pause_until);
                    warn!(
                        pause_secs = pause_secs,
                        "Insufficient MATIC — pausing redemptions for {}s", pause_secs
                    );
                    return Ok(());
                }
                error!("Batch redemption error: {}", e);
                batch_error = true;
            }
        }

        // ── Phase 6: Apply redemption results under short write lock ──
        {
            let mut pending = self.pending.write().await;

            // Handle individual redemption failures
            for id in &failed_ids {
                if let Some(claim) = pending.get_mut(id)
                    && self.handle_retry(claim)
                {
                    to_remove.push(id.clone());
                }
            }

            // Batch-level error (non-gas): retry each resolved claim
            if batch_error {
                for mid in &resolved_claims {
                    if let Some(claim) = pending.get_mut(mid)
                        && self.handle_retry(claim)
                    {
                        to_remove.push(mid.clone());
                    }
                }
            }

            for market_id in &to_remove {
                pending.remove(market_id);
            }
        } // write lock dropped

        Ok(())
    }

    /// Handle retry logic for failed/pending claims.
    /// Returns `true` if the claim should be removed (max retries exceeded).
    fn handle_retry(&self, claim: &mut PendingClaim) -> bool {
        claim.attempts += 1;

        if claim.attempts > self.config.max_retries {
            error!(
                market_id = %claim.market_id,
                attempts = claim.attempts,
                max_retries = self.config.max_retries,
                "Claim exceeded max retries — removing from queue, manual intervention needed"
            );
            return true;
        }

        // Apply exponential backoff
        let backoff_secs = self.config.retry_backoff_secs * 2_u64.pow((claim.attempts - 1).min(5));
        claim.next_check = Utc::now() + Duration::seconds(backoff_secs as i64);

        debug!(
            "Scheduling retry for market {} in {}s (attempt {}/{})",
            claim.market_id, backoff_secs, claim.attempts, self.config.max_retries
        );

        false
    }

    /// Get current pending claims count (for diagnostics/dashboard)
    pub async fn pending_count(&self) -> usize {
        self.pending.read().await.len()
    }

    /// Check if a market is tracked in traded_markets (for testing)
    #[cfg(test)]
    pub async fn has_traded_market(&self, market_id: &str) -> bool {
        self.traded_markets.read().await.contains(market_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::ExecutionBackend;
    use crate::types::*;
    use rust_decimal::Decimal;

    struct MockExecution;

    #[async_trait::async_trait]
    impl ExecutionBackend for MockExecution {
        async fn place_order(&self, _order: &OrderRequest) -> Result<OrderResult> {
            unimplemented!()
        }
        async fn cancel_order(&self, _order_id: &str) -> Result<()> {
            unimplemented!()
        }
        async fn cancel_all_orders(&self) -> Result<()> {
            unimplemented!()
        }
        async fn get_open_orders(&self) -> Result<Vec<Order>> {
            unimplemented!()
        }
        async fn get_positions(&self) -> Result<Vec<Position>> {
            unimplemented!()
        }
        async fn get_balance(&self) -> Result<Decimal> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn reconciled_fill_signal_tracked_in_traded_markets() {
        let event_bus = EventBus::new();
        let execution: Arc<dyn ExecutionBackend> = Arc::new(MockExecution);
        let ctx = StrategyContext::new();
        let config = AutoClaimConfig::default();

        let monitor = Arc::new(ClaimMonitor::new(config, event_bus.clone(), execution, ctx));

        // Spawn just the signal listener
        let monitor_clone = monitor.clone();
        let mut signal_events = event_bus.subscribe_topics(&["signal"]);
        tokio::spawn(async move {
            while let Some(event) = signal_events.recv().await {
                if let Event::Signal(SignalEvent {
                    signal_type,
                    payload,
                    ..
                }) = event
                    && (signal_type == "matched-fill" || signal_type == "reconciled-fill")
                    && let Some(market_id) = payload.get("market_id").and_then(|v| v.as_str())
                {
                    monitor_clone
                        .traded_markets
                        .write()
                        .await
                        .insert(market_id.to_string());
                }
            }
        });

        // Emit a "reconciled-fill" signal
        event_bus.publish(Event::Signal(SignalEvent {
            strategy_name: "crypto-arb-tailend".to_string(),
            signal_type: "reconciled-fill".to_string(),
            payload: serde_json::json!({ "market_id": "market_abc", "order_id": "ord1" }),
            timestamp: Utc::now(),
        }));

        // Give the async listener time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            monitor.has_traded_market("market_abc").await,
            "reconciled-fill signal should add market_id to traded_markets"
        );
    }

    #[tokio::test]
    async fn matched_fill_signal_still_tracked() {
        let event_bus = EventBus::new();
        let execution: Arc<dyn ExecutionBackend> = Arc::new(MockExecution);
        let ctx = StrategyContext::new();
        let config = AutoClaimConfig::default();

        let monitor = Arc::new(ClaimMonitor::new(config, event_bus.clone(), execution, ctx));

        let monitor_clone = monitor.clone();
        let mut signal_events = event_bus.subscribe_topics(&["signal"]);
        tokio::spawn(async move {
            while let Some(event) = signal_events.recv().await {
                if let Event::Signal(SignalEvent {
                    signal_type,
                    payload,
                    ..
                }) = event
                    && (signal_type == "matched-fill" || signal_type == "reconciled-fill")
                    && let Some(market_id) = payload.get("market_id").and_then(|v| v.as_str())
                {
                    monitor_clone
                        .traded_markets
                        .write()
                        .await
                        .insert(market_id.to_string());
                }
            }
        });

        event_bus.publish(Event::Signal(SignalEvent {
            strategy_name: "crypto-arb-tailend".to_string(),
            signal_type: "matched-fill".to_string(),
            payload: serde_json::json!({ "market_id": "market_xyz", "order_id": "ord2" }),
            timestamp: Utc::now(),
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(
            monitor.has_traded_market("market_xyz").await,
            "matched-fill signal should still add market_id to traded_markets"
        );
    }
}
