//! Skip diagnostics, dashboard throttle, and periodic status summary.

use std::collections::HashMap;

use chrono::Utc;
use tracing::info;

use crate::crypto_arb::runtime::CryptoArbRuntime;

impl CryptoArbRuntime {
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
