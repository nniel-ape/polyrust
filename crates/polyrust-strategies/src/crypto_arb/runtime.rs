//! Runtime struct definition and initialization for the crypto arbitrage strategy.
//!
//! `CryptoArbRuntime` holds all shared mutable state used by the strategy:
//! price history, active markets, positions, orders, cooldowns, and diagnostics.
//! Method implementations live in `services/` submodules (added in later tasks).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tracing::info;

use polyrust_core::prelude::*;
use polyrust_market::ChainlinkHistoricalClient;

use crate::crypto_arb::config::ArbitrageConfig;
use crate::crypto_arb::domain::{
    ArbitragePosition, ExitOrderMeta, MarketWithReference, ModeStats, OpenLimitOrder,
    OrderTelemetry, PendingOrder, PositionLifecycle, SpikeEvent,
};

/// 15 minutes in seconds (window duration).
pub const WINDOW_SECS: i64 = 900;

/// Shared state and utilities for the crypto arbitrage strategy.
#[allow(clippy::type_complexity)]
pub struct CryptoArbRuntime {
    /// Strategy configuration.
    pub config: ArbitrageConfig,
    /// On-chain Chainlink RPC client for exact settlement price lookups.
    /// `None` when `config.use_chainlink` is false.
    pub(super) chainlink_client: Option<Arc<ChainlinkHistoricalClient>>,
    /// Active markets indexed by market ID.
    pub(super) active_markets: RwLock<HashMap<MarketId, MarketWithReference>>,
    /// Open positions indexed by market ID.
    pub(super) positions: RwLock<HashMap<MarketId, Vec<ArbitragePosition>>>,
    /// Orders submitted but not yet confirmed — keyed by token_id.
    /// Prevents re-entry while orders are in flight.
    pub(super) pending_orders: RwLock<HashMap<TokenId, PendingOrder>>,
    /// Open GTC limit orders awaiting fill, keyed by order_id.
    pub(super) open_limit_orders: RwLock<HashMap<OrderId, OpenLimitOrder>>,
    /// Markets discovered before prices were available, keyed by coin.
    /// Promoted to active_markets once a price arrives for the coin.
    /// Vec allows multiple markets per coin (e.g. multiple BTC windows at backtest start).
    pub(super) pending_discovery: RwLock<HashMap<String, Vec<MarketInfo>>>,
    /// Recent spike events for display and analysis.
    pub(super) spike_events: RwLock<VecDeque<SpikeEvent>>,
    /// Performance statistics (wins, losses, P&L).
    pub(super) stats: RwLock<ModeStats>,
    /// Cached best-ask prices per token_id, updated on orderbook events.
    /// Used by render_view() to display UP/DOWN market prices.
    pub(super) cached_asks: RwLock<HashMap<TokenId, Decimal>>,
    /// Throttle for dashboard-update signal emission (~5 seconds).
    /// Uses real wall-clock time (not simulated) to rate-limit output.
    pub(super) last_dashboard_emit: RwLock<Option<tokio::time::Instant>>,
    /// Throttle for periodic pipeline status summary (~60 seconds).
    /// Uses real wall-clock time (not simulated) to rate-limit output.
    pub(super) last_status_log: RwLock<Option<tokio::time::Instant>>,
    /// Order rejection cooldowns per market — prevents retry storms.
    /// Uses `DateTime<Utc>` so backtests with simulated time work correctly.
    pub(super) rejection_cooldowns: RwLock<HashMap<MarketId, DateTime<Utc>>>,
    /// Stale market cooldowns — prevents re-entry after a position was removed as stale.
    pub(super) stale_market_cooldowns: RwLock<HashMap<MarketId, DateTime<Utc>>>,
    /// TailEnd skip-reason counters for diagnostics.
    /// Logged every 60s in the pipeline status summary.
    /// Uses std::sync::Mutex (not tokio RwLock) to avoid async overhead on a hot path.
    pub(super) tailend_skip_stats: std::sync::Mutex<HashMap<&'static str, u64>>,
    /// Per-coin nearest market expiry time. Used as a fast pre-filter in TailEnd
    /// to skip ExternalPrice events for coins where no market is near expiration.
    /// Updated on market discovered/expired.
    pub(super) coin_nearest_expiry: RwLock<HashMap<String, DateTime<Utc>>>,
    /// Atomic market reservations to prevent race conditions.
    /// Holds a market_id → slot_count mapping for markets currently being evaluated.
    /// Protects the gap between exposure check and pending_orders.insert().
    pub(super) market_reservations: RwLock<HashMap<MarketId, usize>>,
    /// Order lifecycle telemetry (fill times, rejects, cancels).
    pub(super) order_telemetry: std::sync::Mutex<OrderTelemetry>,
    /// Signal veto counters for diagnostics.
    /// Tracks why entries were vetoed (stale feeds, dispersion, etc.).
    pub(super) signal_veto_stats: std::sync::Mutex<HashMap<&'static str, u64>>,
    /// Per-position lifecycle state machines, keyed by token_id.
    /// Tracks each position through Healthy → ExitExecuting → etc.
    pub(super) position_lifecycle: RwLock<HashMap<TokenId, PositionLifecycle>>,
    /// Exit/recovery orders in flight, keyed by order_id.
    /// Used to route fill/reject events back to the correct position lifecycle.
    pub(super) exit_orders_by_id: RwLock<HashMap<OrderId, ExitOrderMeta>>,
    /// Re-entry cooldowns per market_id after recovery exit.
    /// Prevents re-entering the same market too quickly after a stop-loss cycle.
    /// Keyed by market_id, value is (expires_at, confirm_ticks_remaining).
    pub(super) recovery_exit_cooldowns: RwLock<HashMap<MarketId, DateTime<Utc>>>,
    /// Coins configured for this strategy.
    pub(super) coins: HashSet<String>,
    /// Last event timestamp from the strategy context (simulated or real).
    /// Updated at the start of each on_event call so internal methods
    /// (on_order_placed, on_order_filled) can use it without access to ctx.
    pub(super) last_event_time: RwLock<DateTime<Utc>>,
}

impl CryptoArbRuntime {
    /// Create a new shared base with the given configuration.
    pub fn new(config: ArbitrageConfig, rpc_urls: Vec<String>) -> Self {
        let chainlink_client = if config.use_chainlink {
            Some(Arc::new(ChainlinkHistoricalClient::new(rpc_urls)))
        } else {
            None
        };

        let coins: HashSet<String> = config.coins.iter().cloned().collect();
        let window_size = config.performance.window_size;

        Self {
            config,
            chainlink_client,
            active_markets: RwLock::new(HashMap::new()),
            positions: RwLock::new(HashMap::new()),
            pending_orders: RwLock::new(HashMap::new()),
            open_limit_orders: RwLock::new(HashMap::new()),
            pending_discovery: RwLock::new(HashMap::new()),
            spike_events: RwLock::new(VecDeque::new()),
            stats: RwLock::new(ModeStats::new(window_size)),
            cached_asks: RwLock::new(HashMap::new()),
            last_dashboard_emit: RwLock::new(None),
            last_status_log: RwLock::new(None),
            rejection_cooldowns: RwLock::new(HashMap::new()),
            stale_market_cooldowns: RwLock::new(HashMap::new()),
            tailend_skip_stats: std::sync::Mutex::new(HashMap::new()),
            coin_nearest_expiry: RwLock::new(HashMap::new()),
            market_reservations: RwLock::new(HashMap::new()),
            order_telemetry: std::sync::Mutex::new(OrderTelemetry::default()),
            signal_veto_stats: std::sync::Mutex::new(HashMap::new()),
            position_lifecycle: RwLock::new(HashMap::new()),
            exit_orders_by_id: RwLock::new(HashMap::new()),
            recovery_exit_cooldowns: RwLock::new(HashMap::new()),
            coins,
            last_event_time: RwLock::new(Utc::now()),
        }
    }

    /// Pre-seed PriceService with Chainlink prices at recent 15-min boundaries.
    /// Runs before feeds/discovery start so that `find_best_reference()` can use
    /// Historical-quality lookups for markets discovered shortly after startup.
    pub async fn warm_up(&self, price_service: &PriceService) {
        let Some(client) = &self.chainlink_client else {
            info!("Chainlink disabled, skipping price warm-up");
            return;
        };

        let now_ts = Utc::now().timestamp();
        let current_boundary = now_ts - (now_ts % WINDOW_SECS);

        // 5 boundaries: 0, 15, 30, 45, 60 min ago — covers TailEnd markets up to ~75 min old
        let boundaries: Vec<i64> = (0..5).map(|i| current_boundary - i * WINDOW_SECS).collect();

        let mut join_set = tokio::task::JoinSet::new();
        for coin in self.coins.iter() {
            for &boundary_ts in &boundaries {
                let client = Arc::clone(client);
                let coin = coin.clone();
                join_set.spawn(async move {
                    let result = tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        client.get_price_at_timestamp(&coin, boundary_ts as u64, 100),
                    )
                    .await;
                    (coin, boundary_ts, result)
                });
            }
        }

        let mut success = 0u32;
        while let Some(Ok((coin, _bts, result))) = join_set.join_next().await {
            if let Ok(Ok(cp)) = result {
                let ts = DateTime::from_timestamp(cp.timestamp as i64, 0).unwrap_or_else(Utc::now);
                price_service
                    .record_price(&coin, cp.price, "chainlink", ts, ts)
                    .await;
                success += 1;
            }
        }

        info!(seeded = success, "Chainlink warm-up complete");
    }

    /// Update the cached event time from the strategy context.
    /// Should be called at the start of each on_event handler.
    pub async fn update_event_time(&self, ctx: &StrategyContext) {
        let now = ctx.now().await;
        *self.last_event_time.write().await = now;
    }

    /// Get the last cached event time.
    pub async fn event_time(&self) -> DateTime<Utc> {
        *self.last_event_time.read().await
    }
}
