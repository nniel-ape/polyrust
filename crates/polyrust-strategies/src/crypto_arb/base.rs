//! Shared base state and utilities for crypto arbitrage strategies.
//!
//! Contains:
//! - Shared state (price history, active markets, positions, etc.)
//! - Fee model calculations
//! - Kelly criterion position sizing
//! - Spike detection
//! - Reference price discovery
//! - Stop-loss logic
//! - Performance tracking

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use polyrust_core::prelude::*;
use polyrust_market::ChainlinkHistoricalClient;

use crate::crypto_arb::config::{ArbitrageConfig, SizingConfig};
use crate::crypto_arb::types::{
    ArbitragePosition, BoundarySnapshot, ExitOrderMeta, MarketWithReference, ModeStats,
    OpenLimitOrder, OrderTelemetry, PendingOrder, PositionLifecycle, PositionLifecycleState,
    ReferenceQuality, SpikeEvent,
};

/// Result of a composite fair price calculation from multiple data sources.
#[derive(Debug, Clone)]
pub struct CompositePriceResult {
    /// Weighted average price across sources.
    pub price: Decimal,
    /// Number of sources that contributed.
    pub sources_used: usize,
    /// Maximum lag in milliseconds across contributing sources.
    pub max_lag_ms: i64,
    /// Maximum dispersion from composite in basis points.
    pub dispersion_bps: Decimal,
}

/// Classification of stop-loss sell rejection reasons.
///
/// Determines which cooldown schedule to use and whether to fall back to GTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopLossRejectionKind {
    /// "couldn't be fully filled" or "no match" — transient liquidity gap.
    /// Uses fast cooldowns and marks token for GTC fallback.
    Liquidity,
    /// "not enough balance" or "allowance" — token settlement issue.
    /// Uses longer cooldowns.
    BalanceAllowance,
    /// "invalid amounts" / "must be higher than 0" — dust position too small to sell.
    /// Position should be removed immediately; no cooldown retry.
    InvalidSize,
    /// Everything else — treated like balance/allowance (longer cooldowns).
    Transient,
}

impl StopLossRejectionKind {
    /// Classify a rejection reason string.
    pub fn classify(reason: &str) -> Self {
        let lower = reason.to_lowercase();
        if lower.contains("fully filled") || lower.contains("no match") {
            Self::Liquidity
        } else if lower.contains("not enough balance") || lower.contains("allowance") {
            Self::BalanceAllowance
        } else if lower.contains("invalid amounts") || lower.contains("must be higher than 0") {
            Self::InvalidSize
        } else {
            Self::Transient
        }
    }
}

/// Number of price history entries to keep per coin.
/// At ~5s RTDS intervals, 200 entries covers ~16 minutes — enough for a full
/// 15-minute window plus discovery delay.
pub const PRICE_HISTORY_SIZE: usize = 200;

/// Maximum time (seconds) from a window boundary to consider a snapshot "exact".
pub const BOUNDARY_TOLERANCE_SECS: i64 = 2;

/// 15 minutes in seconds (window duration).
pub const WINDOW_SECS: i64 = 900;

// ---------------------------------------------------------------------------
// Fee helpers (module-level functions)
// ---------------------------------------------------------------------------

/// Compute the Polymarket taker fee per share at a given probability price.
///
/// Formula: `2 * p * (1 - p) * rate`
/// At p=0.50, fee = 0.50 * rate. At p=0.95, fee ≈ 0.095 * rate.
pub fn taker_fee(price: Decimal, rate: Decimal) -> Decimal {
    Decimal::new(2, 0) * price * (Decimal::ONE - price) * rate
}

/// Compute the net profit margin for an entry at `entry_price`, assuming the
/// winning outcome resolves to $1.
///
/// - Gross margin = `1 - entry_price`
/// - Entry fee: taker fee for taker orders, $0 for maker (GTC) orders
/// - Exit fee: ~$0 (resolution at $1 has negligible fee)
///
/// Returns `gross_margin - entry_fee`.
pub fn net_profit_margin(entry_price: Decimal, fee_rate: Decimal, is_maker: bool) -> Decimal {
    let gross = Decimal::ONE - entry_price;
    if is_maker {
        gross // Maker fee = $0
    } else {
        gross - taker_fee(entry_price, fee_rate)
    }
}

/// Compute the Kelly criterion position size in USDC.
///
/// - `payout = (1/price) - 1` — net payout per $1 risked if the bet wins
/// - `kelly = (confidence * payout - (1 - confidence)) / payout`
/// - `size = base_size * kelly * kelly_multiplier`, clamped to `[min_size, max_size]`
///
/// Returns `Decimal::ZERO` for negative edge (should skip the trade).
pub fn kelly_position_size(confidence: Decimal, price: Decimal, config: &SizingConfig) -> Decimal {
    if price.is_zero() || price >= Decimal::ONE {
        return Decimal::ZERO;
    }
    let payout = Decimal::ONE / price - Decimal::ONE;
    // Guard against very small payouts (prices very close to 1.0) that could cause
    // numerical instability or extreme position sizes. Min threshold: 0.001 (0.1% payout)
    if payout < Decimal::new(1, 3) {
        return Decimal::ZERO;
    }
    let kelly = (confidence * payout - (Decimal::ONE - confidence)) / payout;
    if kelly <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let size = config.base_size * kelly * config.kelly_multiplier;
    size.max(config.min_size).min(config.max_size)
}

/// Parse a unix timestamp from a slug suffix (e.g. `btc-updown-15m-1706000000` → timestamp).
/// Returns `None` if the slug doesn't end with a valid unix timestamp.
#[allow(dead_code)] // Used by tests
pub fn parse_slug_timestamp(slug: &str) -> Option<i64> {
    let last_segment = slug.rsplit('-').next()?;
    let ts: i64 = last_segment.parse().ok()?;
    // Sanity: must be a reasonable unix timestamp (after 2020)
    if ts > 1_577_836_800 { Some(ts) } else { None }
}

/// Escape a string for safe inclusion in HTML content.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// Format a USD price with 2 decimal places and thousands separators (e.g. `$88,959.37`).
pub fn fmt_usd(price: Decimal) -> String {
    let rounded = price.round_dp(2);
    let s = format!("{:.2}", rounded);
    // Split on decimal point and add thousands separators to integer part
    let parts: Vec<&str> = s.split('.').collect();
    let int_part = parts[0];
    let dec_part = parts.get(1).unwrap_or(&"00");

    let negative = int_part.starts_with('-');
    let digits: &str = if negative { &int_part[1..] } else { int_part };

    let with_commas: String = digits
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| std::str::from_utf8(chunk).unwrap())
        .collect::<Vec<&str>>()
        .join(",");

    if negative {
        format!("$-{}.{}", with_commas, dec_part)
    } else {
        format!("${}.{}", with_commas, dec_part)
    }
}

/// Format a market probability price with 2 decimal places (e.g. `0.50`).
pub fn fmt_market_price(price: Decimal) -> String {
    format!("{:.2}", price.round_dp(2))
}

// ---------------------------------------------------------------------------
// Shared base struct
// ---------------------------------------------------------------------------

/// Shared state and utilities for the crypto arbitrage strategy.
#[allow(clippy::type_complexity)]
pub struct CryptoArbBase {
    /// Strategy configuration.
    pub config: ArbitrageConfig,
    /// On-chain Chainlink RPC client for exact settlement price lookups.
    /// `None` when `config.use_chainlink` is false.
    pub chainlink_client: Option<Arc<ChainlinkHistoricalClient>>,
    /// Active markets indexed by market ID.
    pub active_markets: RwLock<HashMap<MarketId, MarketWithReference>>,
    /// Price history per coin: (receive_time, price, source, source_timestamp).
    /// Kept at PRICE_HISTORY_SIZE entries for retroactive reference lookup.
    /// `receive_time` is wall-clock when the bot processed the event;
    /// `source_timestamp` is when the upstream feed generated the price.
    pub price_history:
        RwLock<HashMap<String, VecDeque<(DateTime<Utc>, Decimal, String, DateTime<Utc>)>>>,
    /// Proactive price snapshots at 15-min window boundaries, keyed by "{COIN}-{unix_ts}".
    pub boundary_prices: RwLock<HashMap<String, BoundarySnapshot>>,
    /// Open positions indexed by market ID.
    pub positions: RwLock<HashMap<MarketId, Vec<ArbitragePosition>>>,
    /// Orders submitted but not yet confirmed — keyed by token_id.
    /// Prevents re-entry while orders are in flight.
    pub pending_orders: RwLock<HashMap<TokenId, PendingOrder>>,
    /// Open GTC limit orders awaiting fill, keyed by order_id.
    pub open_limit_orders: RwLock<HashMap<OrderId, OpenLimitOrder>>,
    /// Markets discovered before prices were available, keyed by coin.
    /// Promoted to active_markets once a price arrives for the coin.
    /// Vec allows multiple markets per coin (e.g. multiple BTC windows at backtest start).
    pub pending_discovery: RwLock<HashMap<String, Vec<MarketInfo>>>,
    /// Recent spike events for display and analysis.
    pub spike_events: RwLock<VecDeque<SpikeEvent>>,
    /// Performance statistics (wins, losses, P&L).
    pub stats: RwLock<ModeStats>,
    /// Cached best-ask prices per token_id, updated on orderbook events.
    /// Used by render_view() to display UP/DOWN market prices.
    pub cached_asks: RwLock<HashMap<TokenId, Decimal>>,
    /// Throttle for dashboard-update signal emission (~5 seconds).
    /// Uses real wall-clock time (not simulated) to rate-limit output.
    pub last_dashboard_emit: RwLock<Option<tokio::time::Instant>>,
    /// Throttle for periodic pipeline status summary (~60 seconds).
    /// Uses real wall-clock time (not simulated) to rate-limit output.
    pub last_status_log: RwLock<Option<tokio::time::Instant>>,
    /// Order rejection cooldowns per market — prevents retry storms.
    /// Uses `DateTime<Utc>` so backtests with simulated time work correctly.
    pub rejection_cooldowns: RwLock<HashMap<MarketId, DateTime<Utc>>>,
    /// Stale market cooldowns — prevents re-entry after a position was removed as stale.
    pub stale_market_cooldowns: RwLock<HashMap<MarketId, DateTime<Utc>>>,
    /// TailEnd skip-reason counters for diagnostics.
    /// Logged every 60s in the pipeline status summary.
    /// Uses std::sync::Mutex (not tokio RwLock) to avoid async overhead on a hot path.
    pub tailend_skip_stats: std::sync::Mutex<HashMap<&'static str, u64>>,
    /// Per-coin nearest market expiry time. Used as a fast pre-filter in TailEnd
    /// to skip ExternalPrice events for coins where no market is near expiration.
    /// Updated on market discovered/expired.
    pub coin_nearest_expiry: RwLock<HashMap<String, DateTime<Utc>>>,
    /// Atomic market reservations to prevent race conditions.
    /// Holds a market_id → slot_count mapping for markets currently being evaluated.
    /// Protects the gap between exposure check and pending_orders.insert().
    pub market_reservations: RwLock<HashMap<MarketId, usize>>,
    /// Order lifecycle telemetry (fill times, rejects, cancels).
    pub order_telemetry: std::sync::Mutex<OrderTelemetry>,
    /// Last time each feed source was seen (source name -> timestamp).
    /// Updated on every price event via `record_price`. Used for stale-feed gating.
    pub feed_last_seen: RwLock<HashMap<String, DateTime<Utc>>>,
    /// Signal veto counters for diagnostics.
    /// Tracks why entries were vetoed (stale feeds, dispersion, etc.).
    pub signal_veto_stats: std::sync::Mutex<HashMap<&'static str, u64>>,
    /// Cached composite prices for stop-loss decisions, keyed by coin.
    /// Updated on every ExternalPrice event in `record_price`.
    /// Tuple: (composite result, timestamp when computed).
    pub sl_composite_cache: RwLock<HashMap<String, (CompositePriceResult, DateTime<Utc>)>>,
    /// Per-position lifecycle state machines, keyed by token_id.
    /// Tracks each position through Healthy → ExitExecuting → etc.
    pub position_lifecycle: RwLock<HashMap<TokenId, PositionLifecycle>>,
    /// Exit/recovery orders in flight, keyed by order_id.
    /// Used to route fill/reject events back to the correct position lifecycle.
    pub exit_orders_by_id: RwLock<HashMap<OrderId, ExitOrderMeta>>,
    /// Re-entry cooldowns per market_id after recovery exit.
    /// Prevents re-entering the same market too quickly after a stop-loss cycle.
    /// Keyed by market_id, value is (expires_at, confirm_ticks_remaining).
    pub recovery_exit_cooldowns: RwLock<HashMap<MarketId, DateTime<Utc>>>,
    /// Coins configured for this strategy.
    coins: HashSet<String>,
    /// Last event timestamp from the strategy context (simulated or real).
    /// Updated at the start of each on_event call so internal methods
    /// (on_order_placed, on_order_filled) can use it without access to ctx.
    pub last_event_time: RwLock<DateTime<Utc>>,
}

impl CryptoArbBase {
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
            price_history: RwLock::new(HashMap::new()),
            boundary_prices: RwLock::new(HashMap::new()),
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
            feed_last_seen: RwLock::new(HashMap::new()),
            signal_veto_stats: std::sync::Mutex::new(HashMap::new()),
            sl_composite_cache: RwLock::new(HashMap::new()),
            position_lifecycle: RwLock::new(HashMap::new()),
            exit_orders_by_id: RwLock::new(HashMap::new()),
            recovery_exit_cooldowns: RwLock::new(HashMap::new()),
            coins,
            last_event_time: RwLock::new(Utc::now()),
        }
    }

    /// Pre-seed price_history with Chainlink prices at recent 15-min boundaries.
    /// Runs before feeds/discovery start so that `find_best_reference()` can use
    /// Historical-quality lookups for markets discovered shortly after startup.
    pub async fn warm_up(&self) {
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
                let mut history = self.price_history.write().await;
                let entry = history.entry(coin).or_default();
                entry.push_back((ts, cp.price, "chainlink".to_string(), ts));
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

    // -------------------------------------------------------------------------
    // Price handling
    // -------------------------------------------------------------------------

    /// Record a crypto price update and promote any pending markets for this coin.
    ///
    /// Returns `(spike_result, subscribe_actions)`.
    pub async fn record_price(
        &self,
        symbol: &str,
        price: Decimal,
        source: &str,
        now: DateTime<Utc>,
        source_timestamp: DateTime<Utc>,
    ) -> (Option<Decimal>, Vec<Action>) {
        // Update feed health tracking
        {
            let mut seen = self.feed_last_seen.write().await;
            seen.insert(source.to_string(), now);
        }

        // Record price history with source (keep last PRICE_HISTORY_SIZE entries).
        // Deduplicate: when multiple strategy handlers share this base, the same
        // ExternalPrice event triggers record_price once per handler. Skip the
        // insert if the last entry already has the same price to avoid shrinking
        // the effective history window.
        {
            let mut history = self.price_history.write().await;
            let entry = history.entry(symbol.to_string()).or_default();
            let is_duplicate = entry
                .back()
                .map(|(_, last_price, _, _)| *last_price == price)
                .unwrap_or(false);
            if !is_duplicate {
                entry.push_back((now, price, source.to_string(), source_timestamp));
                if entry.len() > PRICE_HISTORY_SIZE {
                    entry.pop_front();
                }
            }
        }

        // Capture boundary snapshot if we just crossed a 15-min boundary.
        let ts = now.timestamp();
        let boundary_ts = ts - (ts % WINDOW_SECS);
        let secs_from_boundary = (ts - boundary_ts).abs();
        if secs_from_boundary <= BOUNDARY_TOLERANCE_SECS {
            let key = format!("{symbol}-{boundary_ts}");
            let mut boundaries = self.boundary_prices.write().await;
            // Only record if we haven't already (prefer Chainlink source)
            let should_insert = match boundaries.get(&key) {
                None => true,
                Some(existing) => {
                    source.eq_ignore_ascii_case("chainlink")
                        && !existing.source.eq_ignore_ascii_case("chainlink")
                }
            };
            if should_insert {
                boundaries.insert(
                    key.clone(),
                    BoundarySnapshot {
                        timestamp: now,
                        price,
                        source: source.to_string(),
                    },
                );
                info!(
                    coin = %symbol,
                    boundary_ts = boundary_ts,
                    price = %price,
                    source = %source,
                    "Captured boundary price snapshot"
                );
            }
            // Prune old boundary snapshots
            drop(boundaries);
            self.prune_boundary_snapshots(symbol, now).await;

            // Boundary just captured — try upgrading Current→Exact for this coin's markets
            self.try_upgrade_quality(symbol).await;
        } else {
            // Startup warm-up: try Historical upgrade during first ~10 price entries per coin.
            // After warm-up, this path goes dormant to avoid per-tick overhead.
            let history_len = {
                let history = self.price_history.read().await;
                history.get(symbol).map(|e| e.len()).unwrap_or(0)
            };
            if history_len <= 10 {
                self.try_upgrade_quality(symbol).await;
            }
        }

        // Promote any pending markets for this coin
        let promote_actions = self.promote_pending_markets(symbol, price, now).await;

        // Spike detection
        let spike = self.detect_spike(symbol, price, now).await;

        (spike, promote_actions)
    }

    /// Get the latest price for a coin from price history.
    pub async fn get_latest_price(&self, coin: &str) -> Option<Decimal> {
        let history = self.price_history.read().await;
        history
            .get(coin)
            .and_then(|h| h.back().map(|(_, p, ..)| *p))
    }

    /// Get the settlement price for a coin at market end time.
    ///
    /// Uses the same oracle Polymarket resolves against, so the bot's win/loss
    /// determination matches on-chain redemption results.
    ///
    /// Priority: 1) Chainlink on-chain at end_ts, 2) price_history closest to end_ts, 3) latest
    pub async fn get_settlement_price(
        &self,
        coin: &str,
        end_date: DateTime<Utc>,
    ) -> Option<Decimal> {
        let end_ts = end_date.timestamp() as u64;

        // 1. Chainlink on-chain — same oracle Polymarket uses for resolution
        if let Some(client) = &self.chainlink_client {
            match client.get_price_at_timestamp(coin, end_ts, 100).await {
                Ok(cp) => {
                    let staleness = cp.timestamp.abs_diff(end_ts);
                    if staleness <= 30 {
                        info!(
                            coin,
                            price = %cp.price,
                            staleness_s = staleness,
                            "Settlement price from Chainlink on-chain"
                        );
                        return Some(cp.price);
                    }
                    warn!(
                        coin,
                        staleness_s = staleness,
                        "Chainlink settlement too stale"
                    );
                }
                Err(e) => warn!(coin, error = %e, "Chainlink settlement lookup failed"),
            }
        }

        // 2. price_history: closest entry at or before end_date
        let history = self.price_history.read().await;
        let entries = history.get(coin)?;
        let mut best = None;
        for (ts, price, ..) in entries.iter() {
            if *ts <= end_date {
                best = Some(*price);
            } else {
                break;
            }
        }
        best.or_else(|| entries.back().map(|(_, p, ..)| *p))
    }

    /// Check if price has favored the given direction for at least `min_sustained_secs`.
    ///
    /// Returns true if for the last `min_sustained_secs`, all prices consistently
    /// indicate the same outcome (above reference for Up, below for Down),
    /// AND there are at least `min_ticks` entries in the window.
    pub async fn check_sustained_direction(
        &self,
        coin: &str,
        reference_price: Decimal,
        predicted: OutcomeSide,
        min_sustained_secs: u64,
        min_ticks: usize,
        now: DateTime<Utc>,
    ) -> bool {
        let history = self.price_history.read().await;
        let entries = match history.get(coin) {
            Some(e) => e,
            None => return false,
        };
        let cutoff = now - chrono::Duration::seconds(min_sustained_secs as i64);

        // Get all entries within the sustained window
        let window_entries: Vec<_> = entries.iter().filter(|(ts, ..)| *ts >= cutoff).collect();

        // Need at least min_ticks entries to confirm direction
        if window_entries.len() < min_ticks {
            debug!(
                coin = %coin,
                entries = window_entries.len(),
                min_ticks = min_ticks,
                min_sustained_secs = min_sustained_secs,
                "Sustained direction check failed: insufficient ticks in window"
            );
            return false;
        }

        // Check if ALL entries in the window favor the predicted direction
        match predicted {
            OutcomeSide::Up | OutcomeSide::Yes => window_entries
                .iter()
                .all(|(_, price, ..)| *price > reference_price),
            OutcomeSide::Down | OutcomeSide::No => window_entries
                .iter()
                .all(|(_, price, ..)| *price < reference_price),
        }
    }

    /// Calculate maximum volatility (price wick) in the last `window_secs`.
    ///
    /// Returns the max percentage deviation from the reference price
    /// as an absolute value (always positive).
    pub async fn max_recent_volatility(
        &self,
        coin: &str,
        reference_price: Decimal,
        window_secs: u64,
        now: DateTime<Utc>,
    ) -> Option<Decimal> {
        if reference_price.is_zero() {
            return None;
        }

        let history = self.price_history.read().await;
        let entries = history.get(coin)?;
        let cutoff = now - chrono::Duration::seconds(window_secs as i64);

        let window_entries: Vec<_> = entries.iter().filter(|(ts, ..)| *ts >= cutoff).collect();

        if window_entries.is_empty() {
            return None;
        }

        let max_price = window_entries
            .iter()
            .map(|(_, p, ..)| *p)
            .max()
            .unwrap_or(reference_price);
        let min_price = window_entries
            .iter()
            .map(|(_, p, ..)| *p)
            .min()
            .unwrap_or(reference_price);

        // Calculate max deviation from reference (wick in either direction)
        let up_wick = (max_price - reference_price).abs() / reference_price;
        let down_wick = (min_price - reference_price).abs() / reference_price;

        Some(up_wick.max(down_wick))
    }

    // -------------------------------------------------------------------------
    // Feed health monitoring
    // -------------------------------------------------------------------------

    /// Check if all required feeds have been seen within the staleness threshold.
    pub async fn are_feeds_fresh(
        &self,
        required: &[&str],
        max_stale_secs: i64,
        now: DateTime<Utc>,
    ) -> bool {
        let seen = self.feed_last_seen.read().await;
        for &source in required {
            match seen.get(source) {
                Some(ts) => {
                    if (now - *ts).num_seconds() > max_stale_secs {
                        return false;
                    }
                }
                None => return false,
            }
        }
        true
    }

    /// Increment a signal veto counter.
    pub fn record_signal_veto(&self, reason: &'static str) {
        let mut stats = self.signal_veto_stats.lock().unwrap();
        *stats.entry(reason).or_insert(0) += 1;
    }

    // -------------------------------------------------------------------------
    // Composite fair price
    // -------------------------------------------------------------------------

    /// Compute a weighted composite fair price from multiple data sources.
    ///
    /// Weights: binance-futures 0.5, binance-spot 0.3, coinbase 0.2
    /// Rejects sources staler than `max_stale_secs`.
    /// Returns None if fewer than min_sources are fresh, or dispersion exceeds max.
    pub async fn composite_fair_price(
        &self,
        coin: &str,
        ctx: &StrategyContext,
        max_stale_secs: i64,
        min_sources: usize,
        max_dispersion_bps: Decimal,
    ) -> Option<CompositePriceResult> {
        let now = ctx.now().await;
        let md = ctx.market_data.read().await;
        let sources = md.sourced_prices.get(coin)?;

        static WEIGHTS: &[(&str, Decimal)] = &[
            // Use const-compatible Decimal construction
            ("binance-futures", Decimal::from_parts(5, 0, 0, false, 1)), // 0.5
            ("binance-spot", Decimal::from_parts(3, 0, 0, false, 1)),    // 0.3
            ("coinbase", Decimal::from_parts(2, 0, 0, false, 1)),        // 0.2
        ];

        let mut weighted_sum = Decimal::ZERO;
        let mut total_weight = Decimal::ZERO;
        let mut prices = Vec::new();
        let mut max_lag_ms: i64 = 0;
        let mut sources_used = 0usize;

        for &(source_name, weight) in WEIGHTS {
            if let Some(sp) = sources.get(source_name) {
                let age_secs = (now - sp.timestamp).num_seconds();
                if age_secs > max_stale_secs {
                    continue;
                }
                weighted_sum += sp.price * weight;
                total_weight += weight;
                prices.push(sp.price);
                sources_used += 1;
                let lag_ms = (now - sp.timestamp).num_milliseconds();
                if lag_ms > max_lag_ms {
                    max_lag_ms = lag_ms;
                }
            }
        }

        if sources_used < min_sources || total_weight.is_zero() {
            // Source-priority fallback: when composite quorum fails, use the
            // highest-priority single fresh source instead of returning None.
            // Priority: binance-futures > binance-spot > coinbase > chainlink
            static PRIORITY: &[&str] =
                &["binance-futures", "binance-spot", "coinbase", "chainlink"];
            for &source_name in PRIORITY {
                if let Some(sp) = sources.get(source_name) {
                    let age_secs = (now - sp.timestamp).num_seconds();
                    if age_secs <= max_stale_secs {
                        let lag_ms = (now - sp.timestamp).num_milliseconds();
                        return Some(CompositePriceResult {
                            price: sp.price,
                            sources_used: 1,
                            max_lag_ms: lag_ms,
                            dispersion_bps: Decimal::ZERO,
                        });
                    }
                }
            }
            return None;
        }

        let composite_price = weighted_sum / total_weight;

        // Compute dispersion in basis points: max deviation from composite
        let dispersion_bps = prices
            .iter()
            .map(|p| {
                if composite_price.is_zero() {
                    Decimal::ZERO
                } else {
                    ((*p - composite_price).abs() / composite_price) * Decimal::new(10000, 0)
                }
            })
            .max()
            .unwrap_or(Decimal::ZERO);

        if dispersion_bps > max_dispersion_bps {
            warn!(
                coin = %coin,
                dispersion_bps = %dispersion_bps,
                max_dispersion_bps = %max_dispersion_bps,
                sources_used = sources_used,
                "Composite price rejected: source dispersion exceeds threshold"
            );
            return None;
        }

        Some(CompositePriceResult {
            price: composite_price,
            sources_used,
            max_lag_ms,
            dispersion_bps,
        })
    }

    // -------------------------------------------------------------------------
    // Stop-loss composite cache
    // -------------------------------------------------------------------------

    /// Recompute the composite fair price for a coin and update the SL cache.
    ///
    /// Called from `handle_external_price` after `record_price`. Uses the
    /// stop-loss freshness config (`sl_max_external_age_ms`, `sl_min_sources`,
    /// `sl_max_dispersion_bps`). Also propagates the result to any open
    /// `PositionLifecycle` entries for positions on this coin.
    pub async fn update_sl_composite_cache(&self, coin: &str, ctx: &StrategyContext) {
        let now = ctx.now().await;
        let sl = &self.config.stop_loss;

        // Use SL-specific freshness parameters (seconds, converted from ms)
        let max_stale_secs = sl.sl_max_external_age_ms / 1000 + 1; // +1 for rounding
        let composite = self
            .composite_fair_price(
                coin,
                ctx,
                max_stale_secs,
                sl.sl_min_sources,
                sl.sl_max_dispersion_bps,
            )
            .await;

        if let Some(ref result) = composite {
            // Update the per-coin cache
            {
                let mut cache = self.sl_composite_cache.write().await;
                cache.insert(coin.to_string(), (result.clone(), now));
            }

            // Propagate to per-position lifecycle entries
            let snapshot = crate::crypto_arb::types::CompositePriceSnapshot::from_result(result);
            let positions = self.positions.read().await;
            let mut lifecycles = self.position_lifecycle.write().await;
            for positions_vec in positions.values() {
                for pos in positions_vec {
                    if pos.coin == coin
                        && let Some(lc) = lifecycles.get_mut(&pos.token_id)
                    {
                        lc.last_composite = Some(snapshot.clone());
                        lc.last_composite_at = Some(now);
                    }
                }
            }
        }
    }

    /// Get a cached composite price for stop-loss decisions if fresh enough.
    ///
    /// Returns `None` if no cached entry exists or if the cached entry is
    /// older than `max_age_ms` milliseconds.
    pub async fn get_sl_composite(
        &self,
        coin: &str,
        max_age_ms: i64,
        now: DateTime<Utc>,
    ) -> Option<CompositePriceResult> {
        let cache = self.sl_composite_cache.read().await;
        let (result, cached_at) = cache.get(coin)?;
        let age_ms = (now - *cached_at).num_milliseconds();
        if age_ms <= max_age_ms {
            Some(result.clone())
        } else {
            None
        }
    }

    /// Get the freshest single external price source for a coin within the age limit.
    ///
    /// Fallback when the composite is unavailable (too few sources or stale).
    /// Returns the price from the source with the most recent timestamp,
    /// provided it is within `max_age_ms`.
    pub async fn get_sl_single_fresh(
        &self,
        coin: &str,
        max_age_ms: i64,
        now: DateTime<Utc>,
    ) -> Option<Decimal> {
        let history = self.price_history.read().await;
        let entries = history.get(coin)?;

        // price_history is a VecDeque of (receive_time, price, source, source_timestamp).
        // Use source_timestamp for freshness — it reflects when the feed generated
        // the price, not when the bot received it. This prevents stale source data
        // from appearing fresh due to processing delay.
        if let Some((_receive_time, price, _source, source_ts)) = entries.back() {
            let age_ms = (now - *source_ts).num_milliseconds();
            if age_ms <= max_age_ms {
                return Some(*price);
            }
        }
        None
    }

    // -------------------------------------------------------------------------
    // Spike detection
    // -------------------------------------------------------------------------

    /// Detect a price spike for a coin by comparing current price to the
    /// price `spike.window_secs` seconds ago in `price_history`.
    ///
    /// Returns `Some(change_pct)` if the absolute percentage change exceeds
    /// `spike.threshold_pct`, otherwise `None`.
    pub async fn detect_spike(
        &self,
        coin: &str,
        current_price: Decimal,
        now: DateTime<Utc>,
    ) -> Option<Decimal> {
        let history = self.price_history.read().await;
        let entries = history.get(coin)?;
        let window = chrono::Duration::seconds(self.config.spike.window_secs as i64);
        let cutoff = now - window;

        // Find the oldest price entry that is at or before the cutoff
        let baseline = entries
            .iter()
            .rev()
            .find(|(ts, _, _, _)| *ts <= cutoff)
            .map(|(_, p, _, _)| *p)?;

        if baseline.is_zero() {
            return None;
        }

        let change_pct = (current_price - baseline) / baseline;
        if change_pct.abs() >= self.config.spike.threshold_pct {
            Some(change_pct)
        } else {
            None
        }
    }

    /// Record a spike event.
    pub async fn record_spike(&self, event: SpikeEvent) {
        let mut spikes = self.spike_events.write().await;
        spikes.push_back(event);
        while spikes.len() > self.config.spike.history_size {
            spikes.pop_front();
        }
    }

    // -------------------------------------------------------------------------
    // Reference price discovery
    // -------------------------------------------------------------------------

    /// Find the most accurate reference price for a coin at a given window start.
    ///
    /// Priority:
    /// 0. Exact boundary snapshot (captured within 2s of window start via RTDS)
    /// 1. On-chain Chainlink RPC lookup (if no boundary, use if staleness ≤ 30s)
    /// 2. Closest historical price entry (within 30s of window start)
    /// 3. Current price (fallback)
    pub async fn find_best_reference(
        &self,
        coin: &str,
        window_ts: i64,
        current_price: Decimal,
    ) -> (Decimal, ReferenceQuality) {
        // 0. Exact boundary snapshot — best real-time accuracy via RTDS (<2s from target)
        let key = format!("{coin}-{window_ts}");
        let boundary_snap = {
            let boundaries = self.boundary_prices.read().await;
            boundaries.get(&key).cloned()
        };

        if let Some(snap) = &boundary_snap {
            let snap_staleness = snap.timestamp.timestamp().abs_diff(window_ts);
            if snap_staleness <= BOUNDARY_TOLERANCE_SECS as u64 {
                // Optionally fetch on-chain for comparison logging (don't block on it)
                if let Some(client) = &self.chainlink_client
                    && let Ok(cp) = client
                        .get_price_at_timestamp(coin, window_ts as u64, 100)
                        .await
                {
                    let onchain_staleness = cp.timestamp.abs_diff(window_ts as u64);
                    info!(
                        coin = %coin,
                        boundary_price = %snap.price,
                        boundary_staleness_s = snap_staleness,
                        onchain_price = %cp.price,
                        onchain_staleness_s = onchain_staleness,
                        "Reference comparison: preferring boundary snapshot over on-chain"
                    );
                }
                return (snap.price, ReferenceQuality::Exact);
            }
        }

        // 1. On-chain Chainlink RPC — use if no fresh boundary and staleness ≤ 30s
        if let Some(client) = &self.chainlink_client {
            match client
                .get_price_at_timestamp(coin, window_ts as u64, 100)
                .await
            {
                Ok(cp) => {
                    let staleness = cp.timestamp.abs_diff(window_ts as u64);
                    if staleness <= 30 {
                        info!(
                            coin = %coin,
                            price = %cp.price,
                            staleness_s = staleness,
                            round_id = cp.round_id,
                            "On-chain Chainlink reference price retrieved (no boundary available)"
                        );
                        return (cp.price, ReferenceQuality::OnChain(staleness));
                    }
                    warn!(
                        coin = %coin,
                        staleness_s = staleness,
                        "On-chain round too stale (>30s), trying historical"
                    );
                }
                Err(e) => {
                    warn!(
                        coin = %coin,
                        error = %e,
                        "On-chain Chainlink lookup failed, falling back to local data"
                    );
                }
            }
        }

        // 2. Historical lookup — closest entry to window start, preferring Chainlink source
        let target = DateTime::from_timestamp(window_ts, 0);
        let history = self.price_history.read().await;
        if let (Some(target_dt), Some(entries)) = (target, history.get(coin)) {
            // Find all entries within 30s of window start
            let mut best: Option<(u64, Decimal, bool)> = None; // (staleness, price, is_preferred)
            for (ts, price, source, _) in entries {
                let staleness = (*ts - target_dt).num_seconds().unsigned_abs();
                if staleness >= 30 {
                    continue;
                }
                // Prefer Chainlink and Binance futures (Polymarket resolves on Binance futures mark price)
                let is_preferred = source.eq_ignore_ascii_case("chainlink")
                    || source.eq_ignore_ascii_case("binance-futures");
                let is_better = match best {
                    None => true,
                    Some((prev_stale, _, prev_pref)) => {
                        // Prefer authoritative sources if staleness is similar (within 5s)
                        if is_preferred && !prev_pref && staleness < prev_stale + 5 {
                            true
                        } else if !is_preferred && prev_pref && prev_stale < staleness + 5 {
                            false
                        } else {
                            staleness < prev_stale
                        }
                    }
                };
                if is_better {
                    best = Some((staleness, *price, is_preferred));
                }
            }
            if let Some((staleness, price, _)) = best {
                return (price, ReferenceQuality::Historical(staleness));
            }
        }

        // 3. Current price (existing behavior)
        info!(
            coin = %coin,
            price = %current_price,
            "No boundary/historical reference found, using current price"
        );
        (current_price, ReferenceQuality::Current)
    }

    /// Remove boundary snapshots older than 4 windows (1 hour) for a given coin.
    async fn prune_boundary_snapshots(&self, coin: &str, now: DateTime<Utc>) {
        let now_ts = now.timestamp();
        let cutoff = now_ts - (WINDOW_SECS * 4);
        let prefix = format!("{coin}-");
        let mut boundaries = self.boundary_prices.write().await;
        boundaries.retain(|key, _| {
            if !key.starts_with(&prefix) {
                return true;
            }
            key.strip_prefix(&prefix)
                .and_then(|ts_str| ts_str.parse::<i64>().ok())
                .is_none_or(|ts| ts >= cutoff)
        });
    }

    /// Retroactively upgrade reference quality for active markets of a coin.
    ///
    /// Called after `record_price()` captures a boundary snapshot or during
    /// startup warm-up (first ~10 price entries). Upgrades markets that were
    /// activated with `Current` quality to `Exact` (via boundary snapshot) or
    /// `Historical` (via price history lookup).
    ///
    /// Lock safety: reads boundary_prices and price_history first, drops those
    /// locks, then acquires active_markets write lock.
    pub async fn try_upgrade_quality(&self, coin: &str) {
        // 1. Snapshot boundary prices for this coin (read lock, then drop)
        let boundary_snapshot: Vec<(String, BoundarySnapshot)> = {
            let boundaries = self.boundary_prices.read().await;
            let prefix = format!("{coin}-");
            boundaries
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };

        // 2. Clone price history for this coin (read lock, then drop)
        let history_entries = {
            let history = self.price_history.read().await;
            history.get(coin).cloned()
        };

        // 3. Write-lock active_markets and upgrade qualifying entries
        let mut markets = self.active_markets.write().await;
        for mwr in markets.values_mut() {
            if mwr.coin != coin {
                continue;
            }

            // Already at best quality — nothing to upgrade
            if mwr.reference_quality == ReferenceQuality::Exact {
                continue;
            }

            let key = format!("{coin}-{}", mwr.window_ts);

            // Try boundary snapshot → Exact upgrade
            if let Some((_, snap)) = boundary_snapshot.iter().find(|(k, _)| k == &key) {
                let snap_staleness = snap.timestamp.timestamp().abs_diff(mwr.window_ts);
                if snap_staleness <= BOUNDARY_TOLERANCE_SECS as u64 {
                    let old_quality = mwr.reference_quality;
                    let old_price = mwr.reference_price;
                    mwr.reference_quality = ReferenceQuality::Exact;
                    mwr.reference_price = snap.price;
                    info!(
                        coin = %coin,
                        market = %mwr.market.id,
                        old_quality = ?old_quality,
                        new_quality = ?ReferenceQuality::Exact,
                        old_price = %old_price,
                        new_price = %snap.price,
                        "Retroactively upgraded reference quality (boundary snapshot)"
                    );
                    continue;
                }
            }

            // Only try Historical upgrade if currently at Current
            if mwr.reference_quality != ReferenceQuality::Current {
                continue;
            }

            // Try historical price lookup → Historical upgrade
            if let Some(entries) = &history_entries {
                let target = DateTime::from_timestamp(mwr.window_ts, 0);
                if let Some(target_dt) = target {
                    let mut best: Option<(u64, Decimal, bool)> = None;
                    for (ts, price, source, _) in entries {
                        let staleness = (*ts - target_dt).num_seconds().unsigned_abs();
                        if staleness >= 30 {
                            continue;
                        }
                        let is_preferred = source.eq_ignore_ascii_case("chainlink")
                            || source.eq_ignore_ascii_case("binance-futures");
                        let is_better = match best {
                            None => true,
                            Some((prev_stale, _, prev_pref)) => {
                                if is_preferred && !prev_pref && staleness < prev_stale + 5 {
                                    true
                                } else if !is_preferred && prev_pref && prev_stale < staleness + 5 {
                                    false
                                } else {
                                    staleness < prev_stale
                                }
                            }
                        };
                        if is_better {
                            best = Some((staleness, *price, is_preferred));
                        }
                    }
                    if let Some((staleness, price, _)) = best {
                        let old_price = mwr.reference_price;
                        mwr.reference_quality = ReferenceQuality::Historical(staleness);
                        mwr.reference_price = price;
                        info!(
                            coin = %coin,
                            market = %mwr.market.id,
                            old_quality = ?ReferenceQuality::Current,
                            new_quality = ?ReferenceQuality::Historical(staleness),
                            old_price = %old_price,
                            new_price = %price,
                            "Retroactively upgraded reference quality (historical lookup)"
                        );
                    }
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // Market lifecycle (discovery, promotion, expiry)
    // -------------------------------------------------------------------------

    /// Handle a newly discovered market. Extracts the coin, resolves the reference
    /// price, and either activates it immediately or buffers it until a price arrives.
    ///
    /// Returns subscribe action if the market was activated. Idempotent: calling
    /// this multiple times for the same market is safe.
    pub async fn on_market_discovered(
        &self,
        market: &MarketInfo,
        ctx: &StrategyContext,
    ) -> Vec<Action> {
        let coin = match self.extract_coin(&market.question) {
            Some(c) => c,
            None => {
                debug!(
                    market = %market.id,
                    question = %market.question,
                    "Skipping market: could not extract coin from question"
                );
                return vec![];
            }
        };

        if !self.coins.contains(&coin) {
            debug!(
                coin = %coin,
                market = %market.id,
                "Skipping market: coin not in configured set"
            );
            return vec![];
        }

        // Check if already active
        {
            let active = self.active_markets.read().await;
            if active.contains_key(&market.id) {
                debug!(
                    market = %market.id,
                    coin = %coin,
                    "Skipping market: already active"
                );
                return vec![];
            }
        }

        // Get the current crypto price — needed for reference lookup
        let md = ctx.market_data.read().await;
        let current_price = match md.external_prices.get(&coin) {
            Some(&p) => p,
            None => {
                info!(
                    coin = %coin,
                    market = %market.id,
                    "No price yet for coin, buffering market for later activation"
                );
                drop(md);
                let mut pending = self.pending_discovery.write().await;
                pending.entry(coin).or_default().push(market.clone());
                return vec![];
            }
        };
        drop(md);

        let now = ctx.now().await;
        self.activate_market(market, &coin, current_price, now)
            .await
    }

    /// Handle a market expiration. Removes from active markets, resolves open positions.
    ///
    /// Idempotent: calling this multiple times for the same market is safe.
    pub async fn on_market_expired(&self, market_id: &str) -> Vec<Action> {
        // Atomically remove market if present — only the first caller returns
        // the unsubscribe action, avoiding redundant actions when multiple
        // strategy handlers share this base.
        let removed_market = {
            let mut active = self.active_markets.write().await;
            active.remove(market_id)
        };

        let Some(market) = removed_market else {
            // Another handler already processed this expiry
            return vec![];
        };

        info!(
            market = %market_id,
            coin = %market.coin,
            "Market expired, removing from active markets"
        );

        // Clean up cached asks for expired market's token IDs
        {
            let mut cached = self.cached_asks.write().await;
            cached.remove(&market.market.token_ids.outcome_a);
            cached.remove(&market.market.token_ids.outcome_b);
        }

        // Clean up any stale reservation for this market
        {
            let mut reservations = self.market_reservations.write().await;
            reservations.remove(market_id);
        }

        // Cancel outstanding lifecycle exit orders for this market
        let cancel_actions: Vec<Action> = {
            let market_token_ids: Vec<String> = vec![
                market.market.token_ids.outcome_a.clone(),
                market.market.token_ids.outcome_b.clone(),
            ];
            let mut exit_orders = self.exit_orders_by_id.write().await;
            let to_cancel: Vec<(OrderId, String)> = exit_orders
                .iter()
                .filter(|(_, meta)| market_token_ids.contains(&meta.token_id))
                .map(|(oid, meta)| (oid.clone(), meta.token_id.clone()))
                .collect();
            let mut actions = Vec::new();
            for (oid, token_id) in to_cancel {
                exit_orders.remove(&oid);
                info!(
                    order_id = %oid,
                    token_id = %token_id,
                    market = %market_id,
                    "Cancelling exit order on market expiry"
                );
                actions.push(Action::CancelOrder(oid));
            }
            actions
        };

        // Cancel outstanding GTC entry orders for this expired market.
        // These orders are dead — the market no longer exists. Do NOT create
        // synthetic positions; just cancel and remove from tracking.
        let entry_cancel_actions: Vec<Action> = {
            let mut limits = self.open_limit_orders.write().await;
            let to_cancel: Vec<OrderId> = limits
                .iter()
                .filter(|(_, lo)| lo.market_id == market_id)
                .map(|(oid, _)| oid.clone())
                .collect();
            let mut actions = Vec::new();
            for oid in to_cancel {
                if let Some(lo) = limits.remove(&oid) {
                    info!(
                        order_id = %oid,
                        token_id = %lo.token_id,
                        market = %market_id,
                        "Cancelling GTC entry order on market expiry"
                    );
                    actions.push(Action::CancelOrder(oid));
                }
            }
            actions
        };

        // Resolve any remaining positions
        let removed = {
            let mut positions = self.positions.write().await;
            positions.remove(market_id)
        };

        if let Some(positions) = removed {
            let settlement_price = self
                .get_settlement_price(&market.coin, market.market.end_date)
                .await;
            for pos in &positions {
                // Check if position is Hedged (complete set: both YES+NO tokens held).
                // Hedged positions always redeem for $1.00/share regardless of outcome.
                let is_hedged = {
                    let lifecycles = self.position_lifecycle.read().await;
                    lifecycles
                        .get(&pos.token_id)
                        .is_some_and(|lc| matches!(lc.state, PositionLifecycleState::Hedged { .. }))
                };

                let pnl = if is_hedged {
                    // Hedged: set redeems for $1.00/share. P&L = redemption - entry - fees + recovery_cost.
                    // recovery_cost is negative (hedge buy cost already recorded).
                    (Decimal::ONE - pos.entry_price) * pos.size
                        - (pos.entry_fee_per_share * pos.size)
                        + pos.recovery_cost
                } else {
                    let won = match (&pos.side, settlement_price) {
                        (OutcomeSide::Up | OutcomeSide::Yes, Some(cp)) => {
                            cp > pos.reference_price
                        }
                        (OutcomeSide::Down | OutcomeSide::No, Some(cp)) => {
                            cp <= pos.reference_price
                        }
                        _ => false,
                    };
                    // Use entry_fee_per_share (0 for GTC entry, actual taker fee for FOK entry)
                    // Include recovery_cost (negative) so win/loss classification reflects
                    // the true net outcome including any opposite-side recovery buys.
                    if won {
                        (Decimal::ONE - pos.entry_price) * pos.size
                            - (pos.entry_fee_per_share * pos.size)
                            + pos.recovery_cost
                    } else {
                        -(pos.entry_price * pos.size) - (pos.entry_fee_per_share * pos.size)
                            + pos.recovery_cost
                    }
                };
                self.record_trade_pnl(pnl).await;
                // Clean up lifecycle state for expired positions
                self.remove_lifecycle(&pos.token_id).await;
                info!(
                    market = %market_id,
                    hedged = is_hedged,
                    pnl = %pnl,
                    settlement_price = ?settlement_price,
                    reference_price = %pos.reference_price,
                    side = ?pos.side,
                    "Position resolved at market expiry"
                );
            }
        }

        self.rebuild_nearest_expiry().await;

        let mut result = cancel_actions;
        result.extend(entry_cancel_actions);
        result.push(Action::UnsubscribeMarket(market_id.to_string()));
        result
    }

    /// Promote pending markets when a price becomes available.
    ///
    /// Called by `record_price` after recording a new price. Returns subscribe
    /// actions for any markets that were promoted.
    pub async fn promote_pending_markets(
        &self,
        symbol: &str,
        current_price: Decimal,
        now: DateTime<Utc>,
    ) -> Vec<Action> {
        let markets = {
            let mut pending = self.pending_discovery.write().await;
            pending.remove(symbol)
        };

        match markets {
            Some(market_list) => {
                let mut actions = Vec::new();
                for m in market_list {
                    actions.extend(self.activate_market(&m, symbol, current_price, now).await);
                }
                actions
            }
            None => vec![],
        }
    }

    /// Internal: activate a market by resolving its reference price and adding it
    /// to active_markets.
    async fn activate_market(
        &self,
        market: &MarketInfo,
        coin: &str,
        current_price: Decimal,
        now: DateTime<Utc>,
    ) -> Vec<Action> {
        let now_ts = now.timestamp();
        let boundary_ts = now_ts - (now_ts % WINDOW_SECS);

        let window_ts = market
            .start_date
            .map(|d| d.timestamp())
            .or_else(|| parse_slug_timestamp(&market.slug))
            .unwrap_or(boundary_ts);

        let (reference_price, reference_quality) = self
            .find_best_reference(coin, window_ts, current_price)
            .await;

        let mwr = MarketWithReference {
            market: market.clone(),
            reference_price,
            reference_quality,
            discovery_time: now,
            coin: coin.to_string(),
            window_ts,
        };

        info!(
            coin = %coin,
            market = %market.id,
            reference = %reference_price,
            quality = ?reference_quality,
            "Activated crypto market"
        );

        let mut active = self.active_markets.write().await;
        active.insert(market.id.clone(), mwr);
        drop(active);

        self.rebuild_nearest_expiry().await;

        vec![Action::SubscribeMarket(market.clone())]
    }

    // -------------------------------------------------------------------------
    // Market management
    // -------------------------------------------------------------------------

    /// Rebuild the coin_nearest_expiry cache from active_markets.
    /// Must be called after any change to active_markets.
    pub async fn rebuild_nearest_expiry(&self) {
        let markets = self.active_markets.read().await;
        let mut nearest: HashMap<String, DateTime<Utc>> = HashMap::new();
        for mwr in markets.values() {
            let entry = nearest
                .entry(mwr.coin.clone())
                .or_insert(mwr.market.end_date);
            if mwr.market.end_date < *entry {
                *entry = mwr.market.end_date;
            }
        }
        let mut cache = self.coin_nearest_expiry.write().await;
        *cache = nearest;
    }

    /// Check if this coin is tracked by the strategy.
    pub fn is_tracked_coin(&self, coin: &str) -> bool {
        self.coins.contains(coin)
    }

    /// Extract coin symbol from market question string.
    pub fn extract_coin(&self, question: &str) -> Option<String> {
        const COIN_NAMES: &[(&str, &str)] = &[
            ("BITCOIN", "BTC"),
            ("ETHEREUM", "ETH"),
            ("SOLANA", "SOL"),
            ("RIPPLE", "XRP"),
        ];

        let upper = question.to_uppercase();

        // First, check for full coin names
        for &(name, ticker) in COIN_NAMES {
            if upper.contains(name) {
                return Some(ticker.to_string());
            }
        }

        // Then, check for ticker symbols as whole words
        for coin in &self.coins {
            let mut found = false;
            for (idx, _) in upper.match_indices(coin.as_str()) {
                let before_ok = idx == 0
                    || upper[..idx]
                        .chars()
                        .next_back()
                        .is_none_or(|c| !c.is_ascii_alphanumeric());
                let after_idx = idx + coin.len();
                let after_ok = after_idx >= upper.len()
                    || upper[after_idx..]
                        .chars()
                        .next()
                        .is_none_or(|c| !c.is_ascii_alphanumeric());
                if before_ok && after_ok {
                    found = true;
                    break;
                }
            }
            if found {
                return Some(coin.clone());
            }
        }
        None
    }

    /// Check if we can open a new position (respects max_positions limit).
    pub async fn can_open_position(&self) -> bool {
        let positions = self.positions.read().await;
        let pending = self.pending_orders.read().await;
        let limits = self.open_limit_orders.read().await;
        let reservations = self.market_reservations.read().await;

        let total_positions: usize = positions.values().map(|v| v.len()).sum();
        let reserved_slots: usize = reservations.values().sum();
        let total = total_positions + pending.len() + limits.len() + reserved_slots;

        total < self.config.max_positions
    }

    /// Validate that the calculated share size meets the market's minimum order size.
    ///
    /// Returns `true` if the size is valid (>= min_order_size), `false` otherwise.
    /// Logs a warning if the size is below minimum to help diagnose config issues.
    pub async fn validate_min_order_size(&self, market_id: &MarketId, size: Decimal) -> bool {
        let markets = self.active_markets.read().await;
        let market = match markets.get(market_id) {
            Some(m) => &m.market,
            None => return false, // Can't validate without market info
        };

        if size < market.min_order_size {
            debug!(
                market = %market_id,
                size = %size,
                min_order_size = %market.min_order_size,
                "Order size below market minimum - skipping"
            );
            false
        } else {
            true
        }
    }

    /// Check if market already has a position, pending order, open limit order,
    /// or active reservation.
    pub async fn has_market_exposure(&self, market_id: &MarketId) -> bool {
        let positions = self.positions.read().await;
        if positions.contains_key(market_id) {
            return true;
        }

        let pending = self.pending_orders.read().await;
        if pending.values().any(|p| &p.market_id == market_id) {
            return true;
        }

        let limits = self.open_limit_orders.read().await;
        if limits.values().any(|lo| &lo.market_id == market_id) {
            return true;
        }

        let reservations = self.market_reservations.read().await;
        if reservations.contains_key(market_id) {
            return true;
        }

        false
    }

    /// Atomically check exposure + position limits and reserve a market for trading.
    ///
    /// Returns `true` if the reservation succeeded (no existing exposure,
    /// position limit not exceeded). The reservation prevents concurrent
    /// entry into the same market.
    pub async fn try_reserve_market(&self, market_id: &MarketId, slot_count: usize) -> bool {
        // Acquire all locks in a consistent order to prevent deadlocks
        let positions = self.positions.read().await;
        let pending = self.pending_orders.read().await;
        let limits = self.open_limit_orders.read().await;
        let mut reservations = self.market_reservations.write().await;

        // Check no existing exposure (same logic as has_market_exposure, inline)
        if positions.contains_key(market_id)
            || pending.values().any(|p| &p.market_id == market_id)
            || limits.values().any(|lo| &lo.market_id == market_id)
            || reservations.contains_key(market_id)
        {
            return false;
        }

        // Check position limit (reservations track slot counts)
        let total_positions: usize = positions.values().map(|v| v.len()).sum();
        let reserved_slots: usize = reservations.values().sum();
        let total = total_positions + pending.len() + limits.len() + reserved_slots;
        if total + slot_count > self.config.max_positions {
            return false;
        }

        reservations.insert(market_id.clone(), slot_count);
        true
    }

    /// Release a market reservation (called on early-exit paths before order placement).
    pub async fn release_reservation(&self, market_id: &MarketId) {
        let mut reservations = self.market_reservations.write().await;
        reservations.remove(market_id);
    }

    /// Consume a market reservation (called just before inserting into pending_orders).
    /// This transfers the "slot" from reservations to pending_orders atomically.
    pub async fn consume_reservation(&self, market_id: &MarketId) {
        let mut reservations = self.market_reservations.write().await;
        reservations.remove(market_id);
    }

    // -------------------------------------------------------------------------
    // Position management
    // -------------------------------------------------------------------------

    /// Record a new position and create its lifecycle state machine in Healthy state.
    pub async fn record_position(&self, pos: ArbitragePosition) {
        let token_id = pos.token_id.clone();
        let mut positions = self.positions.write().await;
        positions
            .entry(pos.market_id.clone())
            .or_default()
            .push(pos);
        drop(positions);
        self.ensure_lifecycle(&token_id).await;
    }

    /// Get or create a lifecycle entry for the given token_id.
    /// Returns a clone of the current lifecycle state.
    /// Creates a new Healthy lifecycle if none exists (handles migration of
    /// positions that existed before the lifecycle system was added).
    pub async fn ensure_lifecycle(&self, token_id: &str) -> PositionLifecycle {
        let mut lifecycles = self.position_lifecycle.write().await;
        lifecycles
            .entry(token_id.to_string())
            .or_insert_with(PositionLifecycle::new)
            .clone()
    }

    /// Remove the lifecycle entry for the given token_id.
    /// Called when a position is fully closed or expired.
    pub async fn remove_lifecycle(&self, token_id: &str) {
        let mut lifecycles = self.position_lifecycle.write().await;
        lifecycles.remove(token_id);
        // Also clean up any exit orders referencing this token
        let mut exit_orders = self.exit_orders_by_id.write().await;
        exit_orders.retain(|_, meta| meta.token_id != token_id);
    }

    /// Look up the opposite token_id for a given token in its market.
    ///
    /// In Polymarket, each market has two outcome tokens (outcome_a / outcome_b).
    /// Given one token, this returns the other. Returns `None` if the market
    /// isn't found or the token doesn't match either outcome.
    pub async fn get_opposite_token(&self, market_id: &str, token_id: &str) -> Option<TokenId> {
        let markets = self.active_markets.read().await;
        let mwr = markets.get(market_id)?;
        let ids = &mwr.market.token_ids;
        if token_id == ids.outcome_a {
            Some(ids.outcome_b.clone())
        } else if token_id == ids.outcome_b {
            Some(ids.outcome_a.clone())
        } else {
            None
        }
    }

    /// Remove a position by token_id across all markets, returning it.
    /// Also clears the stop-loss retry count for this token.
    pub async fn remove_position_by_token(&self, token_id: &str) -> Option<ArbitragePosition> {
        let removed = {
            let mut positions = self.positions.write().await;
            let mut removed = None;
            let mut empty_markets = Vec::new();

            for (market_id, pos_list) in positions.iter_mut() {
                if let Some(idx) = pos_list.iter().position(|p| p.token_id == token_id) {
                    removed = Some(pos_list.remove(idx));
                }
                if pos_list.is_empty() {
                    empty_markets.push(market_id.clone());
                }
            }

            for market_id in empty_markets {
                positions.remove(&market_id);
            }

            removed
        };

        // Clean up lifecycle when position is removed
        if removed.is_some() {
            self.remove_lifecycle(token_id).await;
        }

        removed
    }

    /// Reduce a position's size by `fill_size`, or remove it entirely if fully closed.
    ///
    /// Returns `(position_snapshot, was_fully_closed)`:
    /// - If `fill_size >= pos.size`: removes position entirely, clears stop-loss state
    /// - If `fill_size < pos.size`: reduces `pos.size` in-place, returns clone before reduction
    ///
    /// The returned snapshot always has the **original** size (before reduction) for P&L calculation.
    pub async fn reduce_or_remove_position_by_token(
        &self,
        token_id: &str,
        fill_size: Decimal,
    ) -> Option<(ArbitragePosition, bool)> {
        let result = {
            let mut positions = self.positions.write().await;
            let mut result = None;
            let mut empty_markets = Vec::new();

            for (market_id, pos_list) in positions.iter_mut() {
                if let Some(idx) = pos_list.iter().position(|p| p.token_id == token_id) {
                    let pos = &pos_list[idx];
                    if fill_size >= pos.size {
                        // Full close: remove entirely
                        let removed = pos_list.remove(idx);
                        result = Some((removed, true));
                    } else {
                        // Partial close: snapshot before reducing
                        let snapshot = pos.clone();
                        pos_list[idx].size -= fill_size;
                        result = Some((snapshot, false));
                    }
                }
                if pos_list.is_empty() {
                    empty_markets.push(market_id.clone());
                }
            }

            for market_id in empty_markets {
                positions.remove(&market_id);
            }

            result
        };

        // Clean up lifecycle only on full close
        if let Some((_, true)) = &result {
            self.remove_lifecycle(token_id).await;
        }

        result
    }

    /// Update peak_bid for trailing stop-loss tracking.
    pub async fn update_peak_bid(&self, token_id: &TokenId, current_bid: Decimal) {
        let mut positions = self.positions.write().await;
        for pos_list in positions.values_mut() {
            for pos in pos_list.iter_mut() {
                if &pos.token_id == token_id && current_bid > pos.peak_bid {
                    pos.peak_bid = current_bid;
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // Performance tracking
    // -------------------------------------------------------------------------

    /// Check if the strategy is auto-disabled due to poor performance.
    pub async fn is_auto_disabled(&self) -> bool {
        if !self.config.performance.auto_disable {
            return false;
        }
        let s = self.stats.read().await;
        s.total_trades() >= self.config.performance.min_trades
            && s.win_rate() < self.config.performance.min_win_rate
    }

    /// Record a trade P&L outcome.
    pub async fn record_trade_pnl(&self, pnl: Decimal) {
        let mut s = self.stats.write().await;
        s.record(pnl);
    }

    /// Adjust total P&L without counting as a separate trade in stats.
    /// Used for costs that are part of an existing trade lifecycle (e.g.,
    /// recovery buy cost) to avoid inflating trade count and skewing win rate.
    pub async fn adjust_trade_pnl(&self, amount: Decimal) {
        let mut s = self.stats.write().await;
        s.adjust_pnl(amount);
    }

    /// Accumulate recovery cost on a position so settlement P&L includes it.
    pub async fn add_recovery_cost(&self, token_id: &str, cost: Decimal) {
        let mut positions = self.positions.write().await;
        for pos_list in positions.values_mut() {
            if let Some(pos) = pos_list.iter_mut().find(|p| p.token_id == token_id) {
                pos.recovery_cost += cost;
                return;
            }
        }
    }

    // -------------------------------------------------------------------------
    // Order management
    // -------------------------------------------------------------------------

    /// Record a rejection cooldown for a market.
    pub async fn record_rejection_cooldown(&self, market_id: &MarketId, cooldown_secs: u64) {
        let now = self.event_time().await;
        let expires_at = now + chrono::Duration::seconds(cooldown_secs as i64);
        let mut cooldowns = self.rejection_cooldowns.write().await;
        cooldowns.insert(market_id.clone(), expires_at);
    }

    /// Check if a market is still in rejection cooldown.
    pub async fn is_rejection_cooled_down(&self, market_id: &MarketId) -> bool {
        let now = self.event_time().await;
        let cooldowns = self.rejection_cooldowns.read().await;
        if let Some(expires_at) = cooldowns.get(market_id) {
            now < *expires_at
        } else {
            false
        }
    }

    /// Record a stale market cooldown to prevent re-entry after position removal.
    pub async fn record_stale_market_cooldown(&self, market_id: &MarketId, cooldown_secs: u64) {
        let now = self.event_time().await;
        let expires_at = now + chrono::Duration::seconds(cooldown_secs as i64);
        let mut cooldowns = self.stale_market_cooldowns.write().await;
        cooldowns.insert(market_id.clone(), expires_at);
    }

    /// Check if a market is still in stale-removal cooldown.
    pub async fn is_stale_market_cooled_down(&self, market_id: &MarketId) -> bool {
        let now = self.event_time().await;
        let cooldowns = self.stale_market_cooldowns.read().await;
        if let Some(expires_at) = cooldowns.get(market_id) {
            now < *expires_at
        } else {
            false
        }
    }

    /// Record a recovery exit cooldown to prevent same-side re-entry too quickly.
    /// Uses `stale_market_cooldown_secs` as the cooldown duration (market-level cooldown).
    pub async fn record_recovery_exit_cooldown(&self, market_id: &MarketId) {
        let now = self.event_time().await;
        let expires_at = now
            + chrono::Duration::seconds(self.config.stop_loss.stale_market_cooldown_secs as i64);
        let mut cooldowns = self.recovery_exit_cooldowns.write().await;
        cooldowns.insert(market_id.clone(), expires_at);
    }

    /// Check if a market is still in recovery exit cooldown (preventing re-entry).
    pub async fn is_recovery_exit_cooled_down(&self, market_id: &MarketId) -> bool {
        let now = self.event_time().await;
        let cooldowns = self.recovery_exit_cooldowns.read().await;
        if let Some(expires_at) = cooldowns.get(market_id) {
            now < *expires_at
        } else {
            false
        }
    }

    /// Handle a CancelFailed event for a limit order.
    ///
    /// If the reason indicates the order is permanently gone (matched/canceled/not found),
    /// remove it from `open_limit_orders` to prevent retry loops. Otherwise, reset
    /// `cancel_pending` so the stale-order check can retry later.
    ///
    /// Returns `(found, actions)` — `found` is true if the order was in our tracking,
    /// and `actions` contains a matched-fill signal if the order was matched by a
    /// counterparty (so the claim monitor can track the position).
    pub async fn handle_cancel_failed(&self, order_id: &str, reason: &str) -> (bool, Vec<Action>) {
        let mut limits = self.open_limit_orders.write().await;
        if let Some(lo) = limits.get_mut(order_id) {
            let permanently_gone = reason.contains("matched")
                || reason.contains("canceled")
                || reason.contains("not found");
            if permanently_gone {
                let lo = limits.remove(order_id).unwrap();
                warn!(
                    order_id = %order_id,
                    market = %lo.market_id,
                    reason = %reason,
                    "Order permanently gone — removed from tracking"
                );

                let mut actions = Vec::new();
                if reason.contains("matched") {
                    info!(
                        order_id = %order_id,
                        market = %lo.market_id,
                        "Detected matched fill from cancel failure — creating position"
                    );
                    let now = self.event_time().await;
                    let position = ArbitragePosition::from_limit_order(
                        &lo,
                        lo.price,
                        lo.size,
                        Some(order_id.to_string()),
                        now,
                    );
                    self.record_position(position).await;
                    // Emit RecordFill so the persistence handler records this trade.
                    // Matched fills are always entry buys (GTC maker = 0 fee).
                    actions.push(Action::RecordFill {
                        order_id: order_id.to_string(),
                        market_id: lo.market_id.clone(),
                        token_id: lo.token_id.clone(),
                        side: OrderSide::Buy,
                        price: lo.price,
                        size: lo.size,
                        realized_pnl: None,
                        fee: Some(Decimal::ZERO),
                        order_type: Some("Gtc".to_string()),
                        orderbook_snapshot: None,
                    });
                    // Also emit signal for dashboard/logging consumers
                    actions.push(Action::EmitSignal {
                        signal_type: "matched-fill".to_string(),
                        payload: serde_json::json!({
                            "order_id": order_id,
                            "market_id": lo.market_id,
                            "token_id": lo.token_id,
                        }),
                    });
                }
                return (true, actions);
            } else {
                lo.cancel_pending = false;
                warn!(
                    order_id = %order_id,
                    market = %lo.market_id,
                    reason = %reason,
                    "Cancel failed (transient), will retry"
                );
            }
            return (true, vec![]);
        }
        (false, vec![])
    }

    /// Reconcile tracked limit orders against the CLOB's actual open order set.
    ///
    /// Orders in `open_limit_orders` that are NOT in `clob_open_ids` (and not
    /// already cancel_pending) are treated as potentially filled. However, a
    /// single miss could be a transient API snapshot gap, so we require **2
    /// consecutive misses** before creating a synthetic fill position.
    ///
    /// - First miss: increment `reconcile_miss_count`, log warning, skip
    /// - Second+ miss: proceed with synthetic fill (position + RecordFill)
    /// - Order reappears in snapshot: reset `reconcile_miss_count` to 0
    ///
    /// Returns actions (signals) for each confirmed fill.
    pub async fn reconcile_limit_orders(&self, clob_open_ids: &HashSet<String>) -> Vec<Action> {
        let mut limits = self.open_limit_orders.write().await;
        let mut confirmed_fills = Vec::new();
        let now = self.event_time().await;

        // Phase 1: Update miss counters and reset orders that reappeared
        let all_oids: Vec<String> = limits.keys().cloned().collect();
        for oid in &all_oids {
            let lo = limits.get_mut(oid).unwrap();
            if lo.cancel_pending {
                continue;
            }
            if clob_open_ids.contains(oid) {
                // Order is still on the book — reset miss counter
                if lo.reconcile_miss_count > 0 {
                    debug!(
                        order_id = %oid,
                        prev_misses = lo.reconcile_miss_count,
                        "Order reappeared in CLOB snapshot, resetting miss counter"
                    );
                    lo.reconcile_miss_count = 0;
                }
            } else {
                // Order missing from snapshot
                lo.reconcile_miss_count += 1;
                if lo.reconcile_miss_count < 2 {
                    warn!(
                        order_id = %oid,
                        market = %lo.market_id,
                        token = %lo.token_id,
                        miss_count = lo.reconcile_miss_count,
                        "Order missing from CLOB snapshot (miss {}/2), deferring reconciliation",
                        lo.reconcile_miss_count
                    );
                }
            }
        }

        // Phase 2: Collect confirmed misses (miss_count >= 2) for synthetic fill
        let confirmed_oids: Vec<String> = limits
            .iter()
            .filter(|(_, lo)| !lo.cancel_pending && lo.reconcile_miss_count >= 2)
            .map(|(oid, _)| oid.clone())
            .collect();

        for order_id in confirmed_oids {
            let lo = limits.remove(&order_id).unwrap();
            info!(
                order_id = %order_id,
                market = %lo.market_id,
                token = %lo.token_id,
                price = %lo.price,
                size = %lo.size,
                miss_count = lo.reconcile_miss_count,
                "Reconciled fill: order confirmed missing from CLOB after {} snapshots",
                lo.reconcile_miss_count
            );

            let position = ArbitragePosition::from_limit_order(
                &lo,
                lo.price,
                lo.size,
                Some(order_id.clone()),
                now,
            );
            confirmed_fills.push((position, order_id, lo));
        }
        drop(limits);

        let mut result_actions = Vec::new();
        for (position, order_id, lo) in confirmed_fills {
            self.record_position(position).await;
            // Emit RecordFill so the persistence handler records this trade.
            // Reconciled fills are always entry buys (GTC maker = 0 fee).
            result_actions.push(Action::RecordFill {
                order_id: order_id.clone(),
                market_id: lo.market_id.clone(),
                token_id: lo.token_id.clone(),
                side: OrderSide::Buy,
                price: lo.price,
                size: lo.size,
                realized_pnl: None,
                fee: Some(Decimal::ZERO),
                order_type: Some("Gtc".to_string()),
                orderbook_snapshot: None,
            });
            // Also emit signal for dashboard/logging consumers
            result_actions.push(Action::EmitSignal {
                signal_type: "reconciled-fill".to_string(),
                payload: serde_json::json!({
                    "order_id": order_id,
                    "market_id": lo.market_id,
                    "token_id": lo.token_id,
                    "price": lo.price.to_string(),
                    "size": lo.size.to_string(),
                    "side": format!("{:?}", lo.side),
                }),
            });
        }

        result_actions
    }

    /// Cancel GTC limit orders that have been open longer than `max_age_secs`.
    ///
    /// Orders are flagged with `cancel_pending = true` rather than removed from
    /// the map. This ensures that if the cancel fails (e.g., order was already
    /// matched), the subsequent `OrderEvent::Filled` can still find the order
    /// and record the position correctly.
    pub async fn check_stale_limit_orders(&self) -> Vec<Action> {
        let max_age_secs = self.config.order.max_age_secs as i64;
        let now = self.event_time().await;

        let mut orders = self.open_limit_orders.write().await;
        let mut actions = Vec::new();
        for (order_id, lo) in orders.iter_mut() {
            if lo.cancel_pending {
                continue; // Already has a cancel in flight
            }
            let age_secs = (now - lo.placed_at).num_seconds();
            if age_secs >= max_age_secs {
                info!(
                    order_id = %order_id,
                    market = %lo.market_id,
                    age_secs = age_secs,
                    "Cancelling stale GTC limit order"
                );
                lo.cancel_pending = true;
                actions.push(Action::CancelOrder(order_id.clone()));
                // Track cancel in telemetry
                let mut telem = self.order_telemetry.lock().unwrap();
                telem.total_cancels += 1;
                *telem.cancel_before_fill.entry(lo.coin.clone()).or_insert(0) += 1;
            }
        }
        actions
    }

    // -------------------------------------------------------------------------
    // TailEnd skip diagnostics
    // -------------------------------------------------------------------------

    /// Increment a TailEnd skip reason counter.
    /// Uses std::sync::Mutex — no async overhead.
    pub async fn record_tailend_skip(&self, reason: &'static str) {
        let mut stats = self.tailend_skip_stats.lock().unwrap();
        *stats.entry(reason).or_insert(0) += 1;
    }

    // -------------------------------------------------------------------------
    // Dashboard support
    // -------------------------------------------------------------------------

    /// Check if enough time has passed to emit a dashboard update signal.
    /// Atomically check whether a dashboard update should be emitted (5-second
    /// throttle) and mark the timestamp if so. Returns `true` when emission
    /// is allowed — combining the check-and-set in a single write lock avoids
    /// the TOCTOU race where multiple strategy tasks could pass the check
    /// concurrently.
    pub async fn try_claim_dashboard_emit(&self) -> bool {
        let now = tokio::time::Instant::now();
        let mut last = self.last_dashboard_emit.write().await;
        let should_emit = match *last {
            Some(t) => now.duration_since(t) >= std::time::Duration::from_secs(5),
            None => true,
        };
        if should_emit {
            *last = Some(now);
        }
        should_emit
    }

    // -------------------------------------------------------------------------
    // Pipeline observability
    // -------------------------------------------------------------------------

    /// Log a periodic status summary at most every 60 seconds.
    ///
    /// Emits at `info!` level so it's visible in Docker logs without requiring
    /// `polyrust=debug`. Helps diagnose "zero trades" by confirming market
    /// discovery and price ingestion are working.
    pub async fn maybe_log_status_summary(&self) {
        // Atomically check-and-set the throttle timestamp in a single write
        // lock to avoid the TOCTOU race where multiple strategy tasks pass
        // the check concurrently.
        let now = tokio::time::Instant::now();
        {
            let mut last = self.last_status_log.write().await;
            if let Some(t) = *last
                && now.duration_since(t) < std::time::Duration::from_secs(60)
            {
                return;
            }
            *last = Some(now);
        }

        let active_count = self.active_markets.read().await.len();
        let pending_count = self.pending_discovery.read().await.len();
        let coins_with_prices = self.price_history.read().await.len();
        let open_positions: usize = self.positions.read().await.values().map(|v| v.len()).sum();
        let pending_orders = self.pending_orders.read().await.len();

        // Drain TailEnd skip stats for this period
        let skip_stats: HashMap<&'static str, u64> = {
            let mut stats = self.tailend_skip_stats.lock().unwrap();
            std::mem::take(&mut *stats)
        };

        info!(
            active_markets = active_count,
            pending_markets = pending_count,
            coins_with_prices = coins_with_prices,
            open_positions = open_positions,
            pending_orders = pending_orders,
            "Pipeline status summary"
        );

        if !skip_stats.is_empty() {
            let summary: Vec<String> = skip_stats.iter().map(|(k, v)| format!("{k}={v}")).collect();
            info!(
                stats = %summary.join(", "),
                "TailEnd skip stats (last 60s)"
            );
        }

        // Drain signal veto stats
        let veto_stats: HashMap<&'static str, u64> = {
            let mut stats = self.signal_veto_stats.lock().unwrap();
            std::mem::take(&mut *stats)
        };
        if !veto_stats.is_empty() {
            let summary: Vec<String> = veto_stats.iter().map(|(k, v)| format!("{k}={v}")).collect();
            info!(
                stats = %summary.join(", "),
                "Signal veto stats (last 60s)"
            );
        }

        // Log order telemetry snapshot
        {
            let telem = self.order_telemetry.lock().unwrap();
            if telem.total_orders > 0 {
                info!(
                    total_orders = telem.total_orders,
                    total_fills = telem.total_fills,
                    total_cancels = telem.total_cancels,
                    post_only_rejects = telem.post_only_rejects,
                    fill_rate = format!("{:.1}%", telem.fill_rate() * 100.0),
                    "Order telemetry"
                );
            }
        }

        // Log per-source feed lag
        {
            let now = Utc::now();
            let seen = self.feed_last_seen.read().await;
            let lag_summary: Vec<String> = seen
                .iter()
                .map(|(source, ts)| {
                    let lag_ms = (now - *ts).num_milliseconds();
                    format!("{source}={lag_ms}ms")
                })
                .collect();
            if !lag_summary.is_empty() {
                info!(
                    feeds = %lag_summary.join(", "),
                    "Feed source lag"
                );
            }
        }
    }
}
