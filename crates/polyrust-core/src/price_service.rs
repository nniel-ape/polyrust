//! Shared price service — single source of truth for external price data.
//!
//! Centralizes all external price observations (crypto prices from Binance, Coinbase, Chainlink)
//! with eager composite calculation, history tracking, boundary snapshots, and feed health monitoring.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use tokio::sync::RwLock;

/// A single price observation with full provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceObservation {
    /// When the bot processed the event.
    pub timestamp: DateTime<Utc>,
    /// The observed price.
    pub price: Decimal,
    /// Source identifier (e.g., "binance-futures", "coinbase").
    pub source: String,
    /// When the upstream feed generated this price.
    pub source_timestamp: DateTime<Utc>,
}

/// A price snapshot captured at a configurable time boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundarySnapshot {
    /// Boundary timestamp (aligned to interval).
    pub timestamp: DateTime<Utc>,
    /// Price at boundary.
    pub price: Decimal,
    /// Source of the price.
    pub source: String,
}

/// Result of a composite fair price calculation from multiple sources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositePriceResult {
    /// Weighted composite price.
    pub price: Decimal,
    /// Number of sources included in calculation.
    pub sources_used: usize,
    /// Maximum lag (ms) among sources used.
    pub max_lag_ms: i64,
    /// Price dispersion in basis points.
    pub dispersion_bps: Decimal,
}

/// Lightweight snapshot for embedding in other structs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositePriceSnapshot {
    /// Weighted composite price.
    pub price: Decimal,
    /// Number of sources included in calculation.
    pub sources_used: usize,
    /// Maximum lag (ms) among sources used.
    pub max_lag_ms: i64,
    /// Price dispersion in basis points.
    pub dispersion_bps: Decimal,
}

impl From<CompositePriceResult> for CompositePriceSnapshot {
    fn from(result: CompositePriceResult) -> Self {
        Self {
            price: result.price,
            sources_used: result.sources_used,
            max_lag_ms: result.max_lag_ms,
            dispersion_bps: result.dispersion_bps,
        }
    }
}

/// Configuration for PriceService.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceServiceConfig {
    /// Source weights for composite: (source_name, weight). Order = fallback priority.
    #[serde(default = "default_source_weights")]
    pub source_weights: Vec<(String, f64)>,
    /// Source priority for single-source fallback (highest first).
    #[serde(default = "default_source_priority")]
    pub source_priority: Vec<String>,
    /// Max staleness (seconds) for a source to be included in composite.
    #[serde(default = "default_max_stale_secs")]
    pub max_stale_secs: i64,
    /// Max price history entries per symbol.
    #[serde(default = "default_max_history_size")]
    pub max_history_size: usize,
    /// Boundary interval in seconds (e.g. 900 for 15-min). None = disabled.
    #[serde(default = "default_boundary_interval_secs")]
    pub boundary_interval_secs: Option<i64>,
    /// Max seconds from boundary to consider a snapshot valid.
    #[serde(default = "default_boundary_tolerance_secs")]
    pub boundary_tolerance_secs: i64,
}

fn default_source_weights() -> Vec<(String, f64)> {
    vec![
        ("binance-futures".to_string(), 0.5),
        ("binance-spot".to_string(), 0.3),
        ("coinbase".to_string(), 0.2),
    ]
}

fn default_source_priority() -> Vec<String> {
    vec![
        "binance-futures".to_string(),
        "binance-spot".to_string(),
        "coinbase".to_string(),
        "chainlink".to_string(),
    ]
}

fn default_max_stale_secs() -> i64 {
    60
}

fn default_max_history_size() -> usize {
    200
}

fn default_boundary_interval_secs() -> Option<i64> {
    Some(900) // 15 minutes
}

fn default_boundary_tolerance_secs() -> i64 {
    5
}

impl Default for PriceServiceConfig {
    fn default() -> Self {
        Self {
            source_weights: default_source_weights(),
            source_priority: default_source_priority(),
            max_stale_secs: default_max_stale_secs(),
            max_history_size: default_max_history_size(),
            boundary_interval_secs: default_boundary_interval_secs(),
            boundary_tolerance_secs: default_boundary_tolerance_secs(),
        }
    }
}

/// Sourced price with timestamp for staleness checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourcedPrice {
    pub price: Decimal,
    pub timestamp: DateTime<Utc>,
}

/// PriceService owns all external price data and eagerly computes composite prices.
pub struct PriceService {
    config: PriceServiceConfig,
    /// Latest price per symbol (any source) — fast path.
    latest_prices: RwLock<HashMap<String, Decimal>>,
    /// Per-source observations: symbol -> source -> SourcedPrice.
    sourced_prices: RwLock<HashMap<String, HashMap<String, SourcedPrice>>>,
    /// Eagerly computed composite: symbol -> (result, computed_at).
    composite_cache: RwLock<HashMap<String, (CompositePriceResult, DateTime<Utc>)>>,
    /// Price history: symbol -> VecDeque<PriceObservation>.
    price_history: RwLock<HashMap<String, VecDeque<PriceObservation>>>,
    /// Boundary snapshots: "{symbol}-{unix_ts}" -> BoundarySnapshot.
    boundary_snapshots: RwLock<HashMap<String, BoundarySnapshot>>,
    /// Feed health: source_name -> last seen timestamp.
    feed_last_seen: RwLock<HashMap<String, DateTime<Utc>>>,
}

impl PriceService {
    pub fn new(config: PriceServiceConfig) -> Self {
        Self {
            config,
            latest_prices: RwLock::new(HashMap::new()),
            sourced_prices: RwLock::new(HashMap::new()),
            composite_cache: RwLock::new(HashMap::new()),
            price_history: RwLock::new(HashMap::new()),
            boundary_snapshots: RwLock::new(HashMap::new()),
            feed_last_seen: RwLock::new(HashMap::new()),
        }
    }

    /// Record a price observation. Eagerly recomputes composite.
    pub async fn record_price(
        &self,
        symbol: &str,
        price: Decimal,
        source: &str,
        now: DateTime<Utc>,
        source_timestamp: DateTime<Utc>,
    ) {
        // 1. Update feed health
        self.feed_last_seen
            .write()
            .await
            .insert(source.to_string(), now);

        // 2. Update sourced prices and latest price
        {
            let mut sourced = self.sourced_prices.write().await;
            sourced.entry(symbol.to_string()).or_default().insert(
                source.to_string(),
                SourcedPrice {
                    price,
                    timestamp: source_timestamp,
                },
            );
        }
        self.latest_prices
            .write()
            .await
            .insert(symbol.to_string(), price);

        // 3. Record in price history (with deduplication)
        {
            let mut history = self.price_history.write().await;
            let entries = history.entry(symbol.to_string()).or_default();

            // Deduplicate: skip if last entry has same price and source within 1 second
            let is_duplicate = entries.back().map_or(false, |last| {
                last.price == price
                    && last.source == source
                    && (now - last.timestamp).num_seconds().abs() <= 1
            });

            if !is_duplicate {
                entries.push_back(PriceObservation {
                    timestamp: now,
                    price,
                    source: source.to_string(),
                    source_timestamp,
                });

                // Prune old entries
                while entries.len() > self.config.max_history_size {
                    entries.pop_front();
                }
            }
        }

        // 4. Capture boundary snapshot if within tolerance
        if let Some(interval_secs) = self.config.boundary_interval_secs {
            let unix_ts = now.timestamp();
            let boundary_ts = (unix_ts / interval_secs) * interval_secs;
            let offset = (unix_ts - boundary_ts).abs();

            if offset <= self.config.boundary_tolerance_secs {
                let boundary_dt = DateTime::from_timestamp(boundary_ts, 0).unwrap_or(now);
                let key = format!("{}-{}", symbol, boundary_ts);

                self.boundary_snapshots.write().await.insert(
                    key,
                    BoundarySnapshot {
                        timestamp: boundary_dt,
                        price,
                        source: source.to_string(),
                    },
                );
            }
        }

        // 5. Prune old boundary snapshots (keep last 24 hours)
        if self.config.boundary_interval_secs.is_some() {
            let cutoff = now.timestamp() - 86400; // 24 hours
            self.boundary_snapshots.write().await.retain(|key, _| {
                key.split('-')
                    .nth(1)
                    .and_then(|ts_str| ts_str.parse::<i64>().ok())
                    .map_or(false, |ts| ts >= cutoff)
            });
        }

        // 6. Eagerly recompute composite for this symbol
        self.recompute_composite(symbol, now).await;
    }

    /// Recompute composite price for a symbol and cache it.
    async fn recompute_composite(&self, symbol: &str, now: DateTime<Utc>) {
        let sourced = self.sourced_prices.read().await;
        let symbol_prices = match sourced.get(symbol) {
            Some(prices) => prices,
            None => return,
        };

        // Filter fresh sources
        let mut fresh_sources: Vec<_> = symbol_prices
            .iter()
            .filter(|(_, sp)| (now - sp.timestamp).num_seconds() <= self.config.max_stale_secs)
            .collect();

        if fresh_sources.is_empty() {
            return;
        }

        // Sort by source priority for deterministic ordering
        fresh_sources.sort_by_key(|(source_name, _)| {
            self.config
                .source_priority
                .iter()
                .position(|s| s == *source_name)
                .unwrap_or(usize::MAX)
        });

        // Calculate weighted composite
        let mut total_weight = Decimal::ZERO;
        let mut weighted_sum = Decimal::ZERO;
        let mut max_lag_ms = 0i64;
        let mut prices_for_dispersion = Vec::new();

        for (source_name, sp) in &fresh_sources {
            if let Some((_, weight)) = self
                .config
                .source_weights
                .iter()
                .find(|(s, _)| s == *source_name)
            {
                let weight_dec = Decimal::from_f64_retain(*weight).unwrap_or(Decimal::ZERO);
                weighted_sum += sp.price * weight_dec;
                total_weight += weight_dec;
                prices_for_dispersion.push(sp.price);

                let lag_ms = (now - sp.timestamp).num_milliseconds();
                if lag_ms > max_lag_ms {
                    max_lag_ms = lag_ms;
                }
            }
        }

        if total_weight.is_zero() {
            return;
        }

        let composite_price = weighted_sum / total_weight;

        // Calculate dispersion in basis points: max deviation from composite
        let dispersion_bps = if composite_price.is_zero() {
            Decimal::ZERO
        } else {
            prices_for_dispersion
                .iter()
                .map(|p| {
                    ((*p - composite_price).abs() / composite_price * Decimal::from(10000))
                        .round_dp(2)
                })
                .max()
                .unwrap_or(Decimal::ZERO)
        };

        let result = CompositePriceResult {
            price: composite_price,
            sources_used: fresh_sources.len(),
            max_lag_ms,
            dispersion_bps,
        };

        self.composite_cache
            .write()
            .await
            .insert(symbol.to_string(), (result, now));
    }

    /// Get eagerly-computed composite price + timestamp.
    pub async fn composite(&self, symbol: &str) -> Option<(CompositePriceResult, DateTime<Utc>)> {
        self.composite_cache.read().await.get(symbol).cloned()
    }

    /// Get latest price from any source.
    pub async fn latest_price(&self, symbol: &str) -> Option<Decimal> {
        self.latest_prices.read().await.get(symbol).copied()
    }

    /// Get price from a specific source.
    pub async fn sourced_price(&self, symbol: &str, source: &str) -> Option<SourcedPrice> {
        self.sourced_prices
            .read()
            .await
            .get(symbol)
            .and_then(|sources| sources.get(source).cloned())
    }

    /// Get all sourced prices for a symbol.
    pub async fn all_sources(&self, symbol: &str) -> Option<HashMap<String, SourcedPrice>> {
        self.sourced_prices.read().await.get(symbol).cloned()
    }

    /// Get price history for a symbol.
    pub async fn history(&self, symbol: &str) -> Option<VecDeque<PriceObservation>> {
        self.price_history.read().await.get(symbol).cloned()
    }

    /// Get boundary snapshot.
    pub async fn boundary_snapshot(
        &self,
        symbol: &str,
        boundary_ts: i64,
    ) -> Option<BoundarySnapshot> {
        let key = format!("{}-{}", symbol, boundary_ts);
        self.boundary_snapshots.read().await.get(&key).cloned()
    }

    /// Check if all required feeds have been seen within staleness threshold.
    pub async fn are_feeds_fresh(
        &self,
        required: &[&str],
        max_stale_secs: i64,
        now: DateTime<Utc>,
    ) -> bool {
        let last_seen = self.feed_last_seen.read().await;
        required.iter().all(|feed| {
            last_seen
                .get(*feed)
                .map_or(false, |ts| (now - *ts).num_seconds() <= max_stale_secs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn test_config() -> PriceServiceConfig {
        PriceServiceConfig {
            source_weights: vec![
                ("binance-futures".to_string(), 0.5),
                ("binance-spot".to_string(), 0.3),
                ("coinbase".to_string(), 0.2),
            ],
            source_priority: vec![
                "binance-futures".to_string(),
                "binance-spot".to_string(),
                "coinbase".to_string(),
            ],
            max_stale_secs: 60,
            max_history_size: 10,
            boundary_interval_secs: Some(900),
            boundary_tolerance_secs: 5,
        }
    }

    #[tokio::test]
    async fn test_record_and_retrieve_price() {
        let service = PriceService::new(test_config());
        let now = Utc::now();

        service
            .record_price("BTC", dec!(50000), "binance-futures", now, now)
            .await;

        let latest = service.latest_price("BTC").await;
        assert_eq!(latest, Some(dec!(50000)));

        let sourced = service.sourced_price("BTC", "binance-futures").await;
        assert!(sourced.is_some());
        assert_eq!(sourced.unwrap().price, dec!(50000));
    }

    #[tokio::test]
    async fn test_composite_calculation() {
        let service = PriceService::new(test_config());
        let now = Utc::now();

        // Record prices from multiple sources
        service
            .record_price("BTC", dec!(50000), "binance-futures", now, now)
            .await;
        service
            .record_price("BTC", dec!(50100), "binance-spot", now, now)
            .await;
        service
            .record_price("BTC", dec!(50200), "coinbase", now, now)
            .await;

        let composite = service.composite("BTC").await;
        assert!(composite.is_some());

        let (result, _) = composite.unwrap();
        assert_eq!(result.sources_used, 3);
        // Weighted: 50000*0.5 + 50100*0.3 + 50200*0.2 = 25000 + 15030 + 10040 = 50070
        assert_eq!(result.price, dec!(50070));
    }

    #[tokio::test]
    async fn test_stale_source_excluded() {
        let service = PriceService::new(test_config());
        let now = Utc::now();
        let stale = now - chrono::Duration::seconds(120);

        service
            .record_price("BTC", dec!(50000), "binance-futures", now, now)
            .await;
        service
            .record_price("BTC", dec!(50100), "binance-spot", stale, stale)
            .await;

        let composite = service.composite("BTC").await;
        assert!(composite.is_some());

        let (result, _) = composite.unwrap();
        // Only binance-futures should be included (binance-spot is stale)
        assert_eq!(result.sources_used, 1);
        assert_eq!(result.price, dec!(50000));
    }

    #[tokio::test]
    async fn test_price_history() {
        let service = PriceService::new(test_config());
        let now = Utc::now();

        service
            .record_price("BTC", dec!(50000), "binance-futures", now, now)
            .await;
        service
            .record_price(
                "BTC",
                dec!(50100),
                "binance-futures",
                now + chrono::Duration::seconds(2),
                now + chrono::Duration::seconds(2),
            )
            .await;

        let history = service.history("BTC").await;
        assert!(history.is_some());
        assert_eq!(history.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_deduplication() {
        let service = PriceService::new(test_config());
        let now = Utc::now();

        // Same price, same source, within 1 second — should deduplicate
        service
            .record_price("BTC", dec!(50000), "binance-futures", now, now)
            .await;
        service
            .record_price("BTC", dec!(50000), "binance-futures", now, now)
            .await;

        let history = service.history("BTC").await;
        assert!(history.is_some());
        assert_eq!(history.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_boundary_snapshot() {
        let service = PriceService::new(test_config());
        // Boundary at 900-second intervals
        let boundary_ts = 1700000000i64; // Example boundary
        let boundary_dt = DateTime::from_timestamp(boundary_ts, 0).unwrap();

        // Record within tolerance (5 seconds)
        service
            .record_price(
                "BTC",
                dec!(50000),
                "binance-futures",
                boundary_dt,
                boundary_dt,
            )
            .await;

        let snapshot = service.boundary_snapshot("BTC", boundary_ts).await;
        assert!(snapshot.is_some());
        assert_eq!(snapshot.unwrap().price, dec!(50000));
    }

    #[tokio::test]
    async fn test_feed_freshness() {
        let service = PriceService::new(test_config());
        let now = Utc::now();

        service
            .record_price("BTC", dec!(50000), "binance-futures", now, now)
            .await;
        service
            .record_price("BTC", dec!(50100), "binance-spot", now, now)
            .await;

        let fresh = service
            .are_feeds_fresh(&["binance-futures", "binance-spot"], 60, now)
            .await;
        assert!(fresh);

        let stale = service
            .are_feeds_fresh(
                &["binance-futures", "binance-spot"],
                60,
                now + chrono::Duration::seconds(120),
            )
            .await;
        assert!(!stale);
    }
}
