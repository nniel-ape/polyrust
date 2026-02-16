//! Price handling, composite pricing, spike detection, and reference price discovery.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use tracing::{debug, info, warn};

use polyrust_core::prelude::*;

use crate::crypto_arb::domain::{
    BoundarySnapshot, CompositePriceResult, ReferenceQuality, SpikeEvent,
};
use crate::crypto_arb::runtime::{
    CryptoArbRuntime, BOUNDARY_TOLERANCE_SECS, PRICE_HISTORY_SIZE, WINDOW_SECS,
};

impl CryptoArbRuntime {
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
        // the effective history window. When the price is unchanged, update the
        // timestamps so freshness gating (get_sl_single_fresh) stays accurate.
        {
            let mut history = self.price_history.write().await;
            let entry = history.entry(symbol.to_string()).or_default();
            let is_duplicate = entry
                .back()
                .map(|(_, last_price, _, _)| *last_price == price)
                .unwrap_or(false);
            if is_duplicate {
                // Same price — update timestamps and source without adding a new entry.
                if let Some(last) = entry.back_mut() {
                    last.0 = now;
                    last.2 = source.to_string();
                    last.3 = source_timestamp;
                }
            } else {
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
            let snapshot = crate::crypto_arb::domain::CompositePriceSnapshot::from_result(result);
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
}
