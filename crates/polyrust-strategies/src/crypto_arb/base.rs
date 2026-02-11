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
    ArbitrageMode, ArbitragePosition, BoundarySnapshot, MarketWithReference, ModeStats,
    OpenLimitOrder, PendingOrder, ReferenceQuality, SpikeEvent,
};

/// Metadata about why a stop-loss was triggered, for diagnostic logging.
#[derive(Debug, Clone)]
pub struct StopLossTrigger {
    /// Which trigger fired: "trailing_stop" or "dual_trigger".
    pub reason: &'static str,
    /// Peak bid observed during position lifetime.
    pub peak_bid: Decimal,
    /// Effective trailing distance (after time decay + floor).
    pub effective_distance: Decimal,
    /// Seconds remaining on the market.
    pub time_remaining: i64,
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
    if ts > 1_577_836_800 {
        Some(ts)
    } else {
        None
    }
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

/// Shared state and utilities for all crypto arbitrage strategies.
///
/// This struct holds all the mutable state that is shared between the two
/// strategy implementations (TailEnd, TwoSided).
/// Using a shared base avoids duplication and ensures consistent state.
#[allow(clippy::type_complexity)]
pub struct CryptoArbBase {
    /// Strategy configuration.
    pub config: ArbitrageConfig,
    /// On-chain Chainlink RPC client for exact settlement price lookups.
    /// `None` when `config.use_chainlink` is false.
    pub chainlink_client: Option<Arc<ChainlinkHistoricalClient>>,
    /// Active markets indexed by market ID.
    pub active_markets: RwLock<HashMap<MarketId, MarketWithReference>>,
    /// Price history per coin: (timestamp, price, source).
    /// Kept at PRICE_HISTORY_SIZE entries for retroactive reference lookup.
    pub price_history: RwLock<HashMap<String, VecDeque<(DateTime<Utc>, Decimal, String)>>>,
    /// Proactive price snapshots at 15-min window boundaries, keyed by "{COIN}-{unix_ts}".
    pub boundary_prices: RwLock<HashMap<String, BoundarySnapshot>>,
    /// Open positions indexed by market ID.
    pub positions: RwLock<HashMap<MarketId, Vec<ArbitragePosition>>>,
    /// Orders submitted but not yet confirmed — keyed by token_id.
    /// Prevents re-entry while orders are in flight.
    pub pending_orders: RwLock<HashMap<TokenId, PendingOrder>>,
    /// Open GTC limit orders awaiting fill, keyed by order_id.
    pub open_limit_orders: RwLock<HashMap<OrderId, OpenLimitOrder>>,
    /// Token IDs with active stop-loss sell orders awaiting confirmation.
    /// Value is the exit (sell) price for P&L calculation.
    pub pending_stop_loss: RwLock<HashMap<TokenId, Decimal>>,
    /// Markets discovered before prices were available, keyed by coin.
    /// Promoted to active_markets once a price arrives for the coin.
    /// Vec allows multiple markets per coin (e.g. multiple BTC windows at backtest start).
    pub pending_discovery: RwLock<HashMap<String, Vec<MarketInfo>>>,
    /// Recent spike events for display and analysis.
    pub spike_events: RwLock<VecDeque<SpikeEvent>>,
    /// Per-mode performance statistics (wins, losses, P&L).
    pub mode_stats: RwLock<HashMap<ArbitrageMode, ModeStats>>,
    /// Cached best-ask prices per token_id, updated on orderbook events.
    /// Used by render_view() to display UP/DOWN market prices.
    pub cached_asks: RwLock<HashMap<TokenId, Decimal>>,
    /// Throttle for dashboard-update signal emission (~5 seconds).
    /// Uses real wall-clock time (not simulated) to rate-limit output.
    pub last_dashboard_emit: RwLock<Option<tokio::time::Instant>>,
    /// Throttle for periodic pipeline status summary (~60 seconds).
    /// Uses real wall-clock time (not simulated) to rate-limit output.
    pub last_status_log: RwLock<Option<tokio::time::Instant>>,
    /// FOK rejection cooldowns per market — prevents retry storms.
    /// Uses `DateTime<Utc>` so backtests with simulated time work correctly.
    pub fok_cooldowns: RwLock<HashMap<MarketId, DateTime<Utc>>>,
    /// Stop-loss rejection cooldowns per token — prevents retry storms on sell failures.
    pub stop_loss_cooldowns: RwLock<HashMap<TokenId, DateTime<Utc>>>,
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

        Self {
            config,
            chainlink_client,
            active_markets: RwLock::new(HashMap::new()),
            price_history: RwLock::new(HashMap::new()),
            boundary_prices: RwLock::new(HashMap::new()),
            positions: RwLock::new(HashMap::new()),
            pending_orders: RwLock::new(HashMap::new()),
            open_limit_orders: RwLock::new(HashMap::new()),
            pending_stop_loss: RwLock::new(HashMap::new()),
            pending_discovery: RwLock::new(HashMap::new()),
            spike_events: RwLock::new(VecDeque::new()),
            mode_stats: RwLock::new(HashMap::new()),
            cached_asks: RwLock::new(HashMap::new()),
            last_dashboard_emit: RwLock::new(None),
            last_status_log: RwLock::new(None),
            fok_cooldowns: RwLock::new(HashMap::new()),
            stop_loss_cooldowns: RwLock::new(HashMap::new()),
            stale_market_cooldowns: RwLock::new(HashMap::new()),
            tailend_skip_stats: std::sync::Mutex::new(HashMap::new()),
            coin_nearest_expiry: RwLock::new(HashMap::new()),
            coins,
            last_event_time: RwLock::new(Utc::now()),
        }
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
    ) -> (Option<Decimal>, Vec<Action>) {

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
                .map(|(_, last_price, _)| *last_price == price)
                .unwrap_or(false);
            if !is_duplicate {
                entry.push_back((now, price, source.to_string()));
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
            .and_then(|h| h.back().map(|(_, p, _)| *p))
    }

    /// Check if price has favored the given direction for at least `min_sustained_secs`.
    ///
    /// Returns true if for the last `min_sustained_secs`, all prices consistently
    /// indicate the same outcome (above reference for Up, below for Down).
    pub async fn check_sustained_direction(
        &self,
        coin: &str,
        reference_price: Decimal,
        predicted: OutcomeSide,
        min_sustained_secs: u64,
        now: DateTime<Utc>,
    ) -> bool {
        let history = self.price_history.read().await;
        let entries = match history.get(coin) {
            Some(e) => e,
            None => return false,
        };
        let cutoff = now - chrono::Duration::seconds(min_sustained_secs as i64);

        // Get all entries within the sustained window
        let window_entries: Vec<_> = entries.iter().filter(|(ts, _, _)| *ts >= cutoff).collect();

        // Need at least 1 entry to confirm direction
        if window_entries.is_empty() {
            debug!(
                coin = %coin,
                min_sustained_secs = min_sustained_secs,
                "Sustained direction check failed: no entries in window"
            );
            return false;
        }

        // Check if ALL entries in the window favor the predicted direction
        match predicted {
            OutcomeSide::Up | OutcomeSide::Yes => {
                window_entries.iter().all(|(_, price, _)| *price > reference_price)
            }
            OutcomeSide::Down | OutcomeSide::No => {
                window_entries.iter().all(|(_, price, _)| *price < reference_price)
            }
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

        let window_entries: Vec<_> = entries.iter().filter(|(ts, _, _)| *ts >= cutoff).collect();

        if window_entries.is_empty() {
            return None;
        }

        let max_price = window_entries
            .iter()
            .map(|(_, p, _)| *p)
            .max()
            .unwrap_or(reference_price);
        let min_price = window_entries
            .iter()
            .map(|(_, p, _)| *p)
            .min()
            .unwrap_or(reference_price);

        // Calculate max deviation from reference (wick in either direction)
        let up_wick = (max_price - reference_price).abs() / reference_price;
        let down_wick = (min_price - reference_price).abs() / reference_price;

        Some(up_wick.max(down_wick))
    }

    // -------------------------------------------------------------------------
    // Spike detection
    // -------------------------------------------------------------------------

    /// Detect a price spike for a coin by comparing current price to the
    /// price `spike.window_secs` seconds ago in `price_history`.
    ///
    /// Returns `Some(change_pct)` if the absolute percentage change exceeds
    /// `spike.threshold_pct`, otherwise `None`.
    pub async fn detect_spike(&self, coin: &str, current_price: Decimal, now: DateTime<Utc>) -> Option<Decimal> {
        let history = self.price_history.read().await;
        let entries = history.get(coin)?;
        let window = chrono::Duration::seconds(self.config.spike.window_secs as i64);
        let cutoff = now - window;

        // Find the oldest price entry that is at or before the cutoff
        let baseline = entries
            .iter()
            .rev()
            .find(|(ts, _, _)| *ts <= cutoff)
            .map(|(_, p, _)| *p)?;

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
            for (ts, price, source) in entries {
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
        self.activate_market(market, &coin, current_price, now).await
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

        // Resolve any remaining positions
        let removed = {
            let mut positions = self.positions.write().await;
            positions.remove(market_id)
        };

        if let Some(positions) = removed {
            let current_crypto = self.get_latest_price(&market.coin).await;
            for pos in &positions {
                let won = match (&pos.side, current_crypto) {
                    (OutcomeSide::Up | OutcomeSide::Yes, Some(cp)) => {
                        cp > pos.reference_price
                    }
                    (OutcomeSide::Down | OutcomeSide::No, Some(cp)) => {
                        cp <= pos.reference_price
                    }
                    _ => false,
                };
                let pnl = if won {
                    (Decimal::ONE - pos.entry_price) * pos.size
                        - (pos.estimated_fee * pos.size)
                } else {
                    -(pos.entry_price * pos.size) - (pos.estimated_fee * pos.size)
                };
                self.record_trade_pnl(&pos.mode, pnl).await;
                info!(
                    market = %market_id,
                    mode = %pos.mode,
                    won = won,
                    pnl = %pnl,
                    "Position resolved at market expiry"
                );
            }
        }

        self.rebuild_nearest_expiry().await;

        vec![Action::UnsubscribeMarket(market_id.to_string())]
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

        let (reference_price, reference_quality) =
            self.find_best_reference(coin, window_ts, current_price)
                .await;

        let mwr = MarketWithReference {
            market: market.clone(),
            reference_price,
            reference_quality,
            discovery_time: now,
            coin: coin.to_string(),
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

        let total_positions: usize = positions.values().map(|v| v.len()).sum();
        let total = total_positions + pending.len() + limits.len();

        total < self.config.max_positions
    }

    /// Validate that the calculated share size meets the market's minimum order size.
    ///
    /// Returns `true` if the size is valid (>= min_order_size), `false` otherwise.
    /// Logs a warning if the size is below minimum to help diagnose config issues.
    pub async fn validate_min_order_size(
        &self,
        market_id: &MarketId,
        size: Decimal,
    ) -> bool {
        let markets = self.active_markets.read().await;
        let market = match markets.get(market_id) {
            Some(m) => &m.market,
            None => return false, // Can't validate without market info
        };

        if size < market.min_order_size {
            warn!(
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

    /// Check if market already has a position, pending order, or open limit order.
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

        false
    }

    // -------------------------------------------------------------------------
    // Position management
    // -------------------------------------------------------------------------

    /// Record a new position.
    pub async fn record_position(&self, pos: ArbitragePosition) {
        let mut positions = self.positions.write().await;
        positions
            .entry(pos.market_id.clone())
            .or_default()
            .push(pos);
    }

    /// Remove a position by token_id across all markets, returning it.
    pub async fn remove_position_by_token(&self, token_id: &str) -> Option<ArbitragePosition> {
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
    // Stop-loss
    // -------------------------------------------------------------------------

    /// Check if stop-loss should trigger for a position.
    ///
    /// Triggers when:
    /// 1. Crypto price reversed by >= stop_loss_reversal_pct (0.5%)
    /// 2. Market price dropped by >= stop_loss_min_drop (5¢) from entry
    /// 3. Time remaining > 60s (don't sell in final minute)
    ///
    /// Returns `Some((action, exit_price, trigger))` when stop-loss should trigger.
    pub async fn check_stop_loss(
        &self,
        pos: &ArbitragePosition,
        snapshot: &OrderbookSnapshot,
        now: DateTime<Utc>,
    ) -> Option<(Action, Decimal, StopLossTrigger)> {
        // Read time_remaining from active_markets, then drop the lock before
        // acquiring price_history (via get_latest_price) to avoid inconsistent
        // lock ordering with record_price which acquires them in reverse order.
        let time_remaining = {
            let markets = self.active_markets.read().await;
            let market = markets.get(&pos.market_id)?;
            market.market.seconds_remaining_at(now)
        };

        // Don't trigger stop-loss when time remaining is below configured threshold
        if time_remaining <= self.config.stop_loss.min_remaining_secs {
            return None;
        }

        // Check crypto price reversal
        let current_crypto = self.get_latest_price(&pos.coin).await;

        let crypto_reversed = if let Some(current) = current_crypto {
            let reversal = match pos.side {
                OutcomeSide::Up | OutcomeSide::Yes => {
                    // We bet Up, so reversal = price went down
                    (pos.reference_price - current) / pos.reference_price
                }
                OutcomeSide::Down | OutcomeSide::No => {
                    // We bet Down, so reversal = price went up
                    (current - pos.reference_price) / pos.reference_price
                }
            };
            reversal >= self.config.stop_loss.reversal_pct
        } else {
            false
        };

        // Check market price drop from entry
        let current_bid = snapshot.best_bid()?;
        let price_drop = pos.entry_price - current_bid;
        let market_dropped = price_drop >= self.config.stop_loss.min_drop;

        let min_distance = self.config.stop_loss.trailing_min_distance;

        // Trailing stop: triggers when position was profitable and bid dropped from peak.
        // Arming requires peak_bid >= entry_price + trailing_min_distance to avoid
        // triggering on sub-cent profit noise.
        let (trailing_triggered, effective_distance) = if self.config.stop_loss.trailing_enabled
            && pos.peak_bid >= pos.entry_price + min_distance
        {
            let base_distance = self.config.stop_loss.trailing_distance;
            let eff = if self.config.stop_loss.time_decay {
                // Tighten trailing distance as expiry approaches (900s = 15min market)
                let decay_factor = Decimal::from(time_remaining) / Decimal::from(900i64);
                // Clamp to [0, 1]
                let clamped = if decay_factor > Decimal::ONE {
                    Decimal::ONE
                } else if decay_factor < Decimal::ZERO {
                    Decimal::ZERO
                } else {
                    decay_factor
                };
                // Apply floor: never let effective distance go below trailing_min_distance
                (base_distance * clamped).max(min_distance)
            } else {
                base_distance
            };
            let drop_from_peak = pos.peak_bid - current_bid;
            (drop_from_peak >= eff, eff)
        } else {
            (false, min_distance)
        };

        if (crypto_reversed && market_dropped) || trailing_triggered {
            let reason = if trailing_triggered {
                "trailing_stop"
            } else {
                "dual_trigger"
            };
            let trigger = StopLossTrigger {
                reason,
                peak_bid: pos.peak_bid,
                effective_distance,
                time_remaining,
            };
            let order = OrderRequest::new(
                pos.token_id.clone(),
                current_bid,
                pos.size,
                OrderSide::Sell,
                OrderType::Fok,
                false, // neg_risk
            )
            .with_tick_size(pos.tick_size)
            .with_fee_rate_bps(pos.fee_rate_bps);
            Some((Action::PlaceOrder(order), current_bid, trigger))
        } else {
            None
        }
    }

    // -------------------------------------------------------------------------
    // Performance tracking
    // -------------------------------------------------------------------------

    /// Check if a mode is auto-disabled due to poor performance.
    pub async fn is_mode_disabled(&self, mode: &ArbitrageMode) -> bool {
        if !self.config.performance.auto_disable {
            return false;
        }
        let stats = self.mode_stats.read().await;
        if let Some(s) = stats.get(mode) {
            s.total_trades() >= self.config.performance.min_trades
                && s.win_rate() < self.config.performance.min_win_rate
        } else {
            false
        }
    }

    /// Record a trade P&L outcome for the given mode.
    pub async fn record_trade_pnl(&self, mode: &ArbitrageMode, pnl: Decimal) {
        let window_size = self.config.performance.window_size;
        let mut stats = self.mode_stats.write().await;
        stats
            .entry(mode.clone())
            .or_insert_with(|| ModeStats::new(window_size))
            .record(pnl);
    }

    // -------------------------------------------------------------------------
    // Order management
    // -------------------------------------------------------------------------

    /// Record a FOK rejection cooldown for a market.
    pub async fn record_fok_cooldown(&self, market_id: &MarketId, cooldown_secs: u64) {
        let now = self.event_time().await;
        let expires_at = now + chrono::Duration::seconds(cooldown_secs as i64);
        let mut cooldowns = self.fok_cooldowns.write().await;
        cooldowns.insert(market_id.clone(), expires_at);
    }

    /// Check if a market is still in FOK rejection cooldown.
    pub async fn is_fok_cooled_down(&self, market_id: &MarketId) -> bool {
        let now = self.event_time().await;
        let cooldowns = self.fok_cooldowns.read().await;
        if let Some(expires_at) = cooldowns.get(market_id) {
            now < *expires_at
        } else {
            false
        }
    }

    /// Record a stop-loss rejection cooldown for a token.
    pub async fn record_stop_loss_cooldown(&self, token_id: &TokenId, cooldown_secs: u64) {
        let now = self.event_time().await;
        let expires_at = now + chrono::Duration::seconds(cooldown_secs as i64);
        let mut cooldowns = self.stop_loss_cooldowns.write().await;
        cooldowns.insert(token_id.clone(), expires_at);
    }

    /// Check if a token is still in stop-loss rejection cooldown.
    pub async fn is_stop_loss_cooled_down(&self, token_id: &TokenId) -> bool {
        let now = self.event_time().await;
        let cooldowns = self.stop_loss_cooldowns.read().await;
        if let Some(expires_at) = cooldowns.get(token_id) {
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

    /// Handle a rejected stop-loss sell order.
    ///
    /// If the reason indicates a permanent balance/allowance issue, removes the stale
    /// position to prevent retry loops. Otherwise applies a cooldown for transient errors.
    pub async fn handle_stop_loss_rejection(&self, token_id: &TokenId, reason: &str, mode_name: &str) {
        let mut pending_sl = self.pending_stop_loss.write().await;
        pending_sl.remove(token_id);

        if reason.contains("not enough balance") || reason.contains("allowance") {
            warn!(
                token_id = %token_id,
                mode = mode_name,
                reason = %reason,
                "Removing stale position: balance/allowance insufficient"
            );
            drop(pending_sl);
            if let Some(pos) = self.remove_position_by_token(token_id).await {
                let cooldown = self.config.stop_loss.stale_market_cooldown_secs;
                self.record_stale_market_cooldown(&pos.market_id, cooldown)
                    .await;
                info!(
                    token_id = %token_id,
                    market = %pos.market_id,
                    mode = mode_name,
                    cooldown_secs = cooldown,
                    "Stale position removed after stop-loss rejection, market cooldown applied"
                );
            }
        } else {
            warn!(
                token_id = %token_id,
                mode = mode_name,
                reason = %reason,
                "Stop-loss sell rejected (transient), cooldown applied"
            );
            drop(pending_sl);
            self.record_stop_loss_cooldown(token_id, 30).await;
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
                    mode = %lo.mode,
                    reason = %reason,
                    "Order permanently gone — removed from tracking"
                );

                let mut actions = Vec::new();
                if reason.contains("matched") {
                    info!(
                        order_id = %order_id,
                        market = %lo.market_id,
                        mode = %lo.mode,
                        "Detected matched fill from cancel failure"
                    );
                    actions.push(Action::EmitSignal {
                        signal_type: "matched-fill".to_string(),
                        payload: serde_json::json!({ "market_id": lo.market_id }),
                    });
                }
                return (true, actions);
            } else {
                lo.cancel_pending = false;
                warn!(
                    order_id = %order_id,
                    market = %lo.market_id,
                    mode = %lo.mode,
                    reason = %reason,
                    "Cancel failed (transient), will retry"
                );
            }
            return (true, vec![]);
        }
        (false, vec![])
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
        let open_positions: usize = self
            .positions
            .read()
            .await
            .values()
            .map(|v| v.len())
            .sum();
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
            let summary: Vec<String> = skip_stats
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            info!(
                stats = %summary.join(", "),
                "TailEnd skip stats (last 60s)"
            );
        }
    }
}
