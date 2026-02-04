use crate::config::AutoClaimConfig;
use crate::error::Result;
use crate::event_bus::EventBus;
use crate::events::{Event, MarketDataEvent, OrderEvent};
use crate::execution::{ExecutionBackend, RedeemRequest, RedeemResult};
use crate::types::MarketId;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// A pending claim waiting for market resolution
#[derive(Debug, Clone)]
struct PendingClaim {
    market_id: MarketId,
    condition_id: String,
    neg_risk: bool,
    _expired_at: DateTime<Utc>,
    attempts: u32,
    next_check: DateTime<Utc>,
}

/// Background monitor that polls for resolved markets and triggers redemption
pub struct ClaimMonitor {
    pending: Arc<RwLock<HashMap<MarketId, PendingClaim>>>,
    config: AutoClaimConfig,
    event_bus: EventBus,
    execution: Arc<dyn ExecutionBackend>,
}

impl ClaimMonitor {
    /// Create a new ClaimMonitor
    pub fn new(
        config: AutoClaimConfig,
        event_bus: EventBus,
        execution: Arc<dyn ExecutionBackend>,
    ) -> Self {
        Self {
            pending: Arc::new(RwLock::new(HashMap::new())),
            config,
            event_bus,
            execution,
        }
    }

    /// Start the claim monitor background task
    pub async fn run(self: Arc<Self>) -> Result<()> {
        info!("ClaimMonitor started (poll interval: {}s)", self.config.poll_interval_secs);

        // Subscribe to MarketExpired events
        let mut market_events = self.event_bus.subscribe_topics(&["market_data"]);

        // Spawn the expiry listener task
        let self_clone = self.clone();
        tokio::spawn(async move {
            while let Some(event) = market_events.recv().await {
                if let Event::MarketData(MarketDataEvent::MarketExpired(market_id)) = event {
                    self_clone.on_market_expired(market_id).await;
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

    /// Handle a MarketExpired event
    async fn on_market_expired(&self, market_id: MarketId) {
        // TODO: In a full implementation, we'd need to extract condition_id and neg_risk
        // from MarketInfo. For now, we'll use market_id as condition_id and assume neg_risk=false.
        // This will be wired properly when integrating with the engine.
        let condition_id = market_id.clone();
        let neg_risk = false;

        let claim = PendingClaim {
            market_id: market_id.clone(),
            condition_id,
            neg_risk,
            _expired_at: Utc::now(),
            attempts: 0,
            next_check: Utc::now(),
        };

        self.pending.write().await.insert(market_id.clone(), claim);
        info!("Added market {} to pending claims queue", market_id);
    }

    /// Check all pending claims and attempt redemption for resolved markets
    async fn check_pending_claims(&self) -> Result<()> {
        let now = Utc::now();
        let mut pending = self.pending.write().await;
        let mut to_remove = Vec::new();

        for (market_id, claim) in pending.iter_mut() {
            // Skip if not yet time to check
            if claim.next_check > now {
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
                            if result.success {
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
                                self.handle_retry(claim)?;
                            }
                        }
                        Err(e) => {
                            error!("Error redeeming market {}: {}", market_id, e);
                            self.handle_retry(claim)?;
                        }
                    }
                }
                Ok(false) => {
                    // Not yet resolved, schedule next check
                    debug!("Market {} not yet resolved", market_id);
                    self.handle_retry(claim)?;
                }
                Err(e) => {
                    error!("Error checking resolution for market {}: {}", market_id, e);
                    self.handle_retry(claim)?;
                }
            }
        }

        // Remove successfully redeemed claims
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
            token_ids: vec![], // Will be populated by backend based on market info
            neg_risk: claim.neg_risk,
        };

        self.execution.redeem_positions(&request).await
    }

    /// Handle retry logic for failed/pending claims
    fn handle_retry(&self, claim: &mut PendingClaim) -> Result<()> {
        claim.attempts += 1;

        if claim.attempts > self.config.max_retries {
            warn!(
                "Market {} exceeded max retries ({}), manual intervention needed",
                claim.market_id, self.config.max_retries
            );
            // TODO: Could publish a SystemEvent::Error here for dashboard visibility
            return Ok(());
        }

        // Apply exponential backoff
        let backoff_secs = self.config.retry_backoff_secs * 2_u64.pow((claim.attempts - 1).min(5));
        claim.next_check = Utc::now() + Duration::seconds(backoff_secs as i64);

        debug!(
            "Scheduling retry for market {} in {}s (attempt {}/{})",
            claim.market_id, backoff_secs, claim.attempts, self.config.max_retries
        );

        Ok(())
    }

    /// Get current pending claims count (for diagnostics/dashboard)
    pub async fn pending_count(&self) -> usize {
        self.pending.read().await.len()
    }
}
