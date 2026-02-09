use crate::config::AutoClaimConfig;
use crate::context::StrategyContext;
use crate::error::Result;
use crate::event_bus::EventBus;
use crate::events::{Event, MarketDataEvent, OrderEvent, SignalEvent};
use crate::execution::{ExecutionBackend, RedeemRequest, RedeemResult};
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
        info!("ClaimMonitor started (poll interval: {}s)", self.config.poll_interval_secs);

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
                    let is_new = self_fills.traded_markets.write().await.insert(market_id.clone());
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
                    && signal_type == "matched-fill"
                    && let Some(market_id) =
                        payload.get("market_id").and_then(|v| v.as_str())
                {
                    let is_new = self_signals
                        .traded_markets
                        .write()
                        .await
                        .insert(market_id.to_string());
                    if is_new {
                        info!(
                            market_id = %market_id,
                            "Recorded matched fill for claim tracking"
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
        };

        self.pending.write().await.insert(market_id.clone(), claim);
        self.traded_markets.write().await.remove(&market_id);
        info!(market_id = %market_id, neg_risk, "Added market to pending claims queue");
    }

    /// Check all pending claims and attempt redemption for resolved markets
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

        let mut pending = self.pending.write().await;

        if !pending.is_empty() {
            info!(pending_count = pending.len(), "Checking pending claims");
        }

        let mut to_remove = Vec::new();

        for (market_id, claim) in pending.iter_mut() {
            // Skip if not yet time to check
            if claim.next_check > now {
                continue;
            }

            // Guard: refuse to proceed with empty token_ids — retry instead of silently failing
            if claim.token_ids.is_empty() {
                error!(
                    market_id = %market_id,
                    attempts = claim.attempts,
                    "Cannot redeem: token_ids is empty (MarketDiscovered event was likely dropped)"
                );
                if self.handle_retry(claim) {
                    to_remove.push(market_id.clone());
                }
                continue;
            }

            debug!("Checking resolution status for market {}", market_id);

            // Check if market has resolved on-chain
            match self.execution.is_market_resolved(&claim.condition_id).await {
                Ok(true) => {
                    // Market resolved! Attempt redemption
                    info!("Market {} has resolved, attempting redemption", market_id);
                    match self.redeem_position(claim).await {
                        Ok(result) => {
                            if result.success && result.tx_hash.is_empty() {
                                // No CTF balance — nothing to redeem
                                info!(
                                    market_id = %market_id,
                                    "No CTF balance for market, removing from queue"
                                );
                                to_remove.push(market_id.clone());
                            } else if result.success {
                                info!(
                                    "Successfully redeemed market {}: tx {}",
                                    market_id, result.tx_hash
                                );
                                // Publish Redeemed event
                                let event = Event::OrderUpdate(OrderEvent::Redeemed {
                                    market_id: market_id.clone(),
                                    tx_hash: result.tx_hash,
                                    strategy_name: "auto-claim".to_string(),
                                });
                                self.event_bus.publish(event);
                                to_remove.push(market_id.clone());
                            } else {
                                warn!(
                                    "Redemption failed for market {}: {}",
                                    market_id, result.message
                                );
                                if self.handle_retry(claim) {
                                    to_remove.push(market_id.clone());
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
                                    "Insufficient MATIC — pausing redemptions for {}s",
                                    pause_secs
                                );
                                break;
                            }
                            error!("Error redeeming market {}: {}", market_id, e);
                            if self.handle_retry(claim) {
                                to_remove.push(market_id.clone());
                            }
                        }
                    }
                }
                Ok(false) => {
                    // Not yet resolved — reschedule without burning a retry
                    debug!("Market {} not yet resolved, will recheck", market_id);
                    claim.next_check =
                        now + Duration::seconds(self.config.poll_interval_secs as i64);
                }
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("insufficient funds for gas") {
                        let pause_secs = self.config.gas_pause_duration_secs;
                        let pause_until = now + Duration::seconds(pause_secs as i64);
                        *self.gas_paused_until.write().await = Some(pause_until);
                        warn!(
                            pause_secs = pause_secs,
                            "Insufficient MATIC — pausing redemptions for {}s",
                            pause_secs
                        );
                        break;
                    }
                    error!("Error checking resolution for market {}: {}", market_id, e);
                    if self.handle_retry(claim) {
                        to_remove.push(market_id.clone());
                    }
                }
            }
        }

        // Remove completed/expired claims
        for market_id in to_remove {
            pending.remove(&market_id);
        }

        Ok(())
    }

    /// Attempt to redeem a position
    async fn redeem_position(&self, claim: &PendingClaim) -> Result<RedeemResult> {
        let request = RedeemRequest {
            market_id: claim.market_id.clone(),
            condition_id: claim.condition_id.clone(),
            token_ids: claim.token_ids.clone(),
            neg_risk: claim.neg_risk,
        };

        self.execution.redeem_positions(&request).await
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
}
