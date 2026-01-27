use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{Timelike, Utc};
use polymarket_client_sdk::gamma;
use polyrust_core::prelude::*;
use tracing::{debug, info, warn};

use crate::feed::MarketDataFeed;

/// Slug prefix for each supported coin's 15-minute Up/Down market.
const COIN_SLUGS: &[(&str, &str)] = &[
    ("BTC", "btc-updown-15m"),
    ("ETH", "eth-updown-15m"),
    ("SOL", "sol-updown-15m"),
    ("XRP", "xrp-updown-15m"),
];

/// 15 minutes in seconds.
const WINDOW_SECS: i64 = 900;

/// Configuration for the market discovery feed.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Polling interval in seconds.
    pub poll_interval_secs: u64,
    /// Coins to discover markets for (must match keys in COIN_SLUGS).
    pub coins: Vec<String>,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 30,
            coins: vec![
                "BTC".to_string(),
                "ETH".to_string(),
                "SOL".to_string(),
                "XRP".to_string(),
            ],
        }
    }
}

/// Market discovery feed using deterministic slug-based lookup.
///
/// For each configured coin, constructs slugs like `btc-updown-15m-{unix_timestamp}`
/// based on 15-minute windows and fetches directly via `GET /markets/slug/{slug}`.
/// Falls back through current → next → previous windows.
///
/// Publishes `MarketDiscovered` events for new markets and `MarketExpired` events
/// when a coin's active market changes (the old slug is replaced).
pub struct DiscoveryFeed {
    config: DiscoveryConfig,
    event_bus: Option<EventBus>,
}

impl DiscoveryFeed {
    pub fn new(config: DiscoveryConfig) -> Self {
        Self {
            config,
            event_bus: None,
        }
    }
}

/// Build the slug prefix map filtered to configured coins.
fn build_slug_map(coins: &[String]) -> Vec<(String, String)> {
    coins
        .iter()
        .filter_map(|coin| {
            let upper = coin.to_uppercase();
            COIN_SLUGS
                .iter()
                .find(|(k, _)| *k == upper)
                .map(|(_, prefix)| (upper, prefix.to_string()))
        })
        .collect()
}

/// Compute the unix timestamp for the start of the current 15-minute window.
fn current_window_timestamp() -> i64 {
    let now = Utc::now();
    let minute = (now.minute() / 15) * 15;
    let window_start = now
        .with_minute(minute)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now);
    window_start.timestamp()
}

/// Try to fetch a market by slug from the Gamma API.
/// Returns `None` for 404s / any errors (logged at debug level).
async fn fetch_market_by_slug(
    client: &gamma::Client,
    slug: &str,
) -> Option<gamma::types::response::Market> {
    let request = gamma::types::request::MarketBySlugRequest::builder()
        .slug(slug)
        .build();
    match client.market_by_slug(&request).await {
        Ok(market) => Some(market),
        Err(e) => {
            debug!(slug = %slug, error = %e, "slug lookup returned no market");
            None
        }
    }
}

/// Try current window, then next (+900s), then previous (-900s).
/// Returns the first market that is accepting orders.
async fn find_market_for_coin(
    client: &gamma::Client,
    prefix: &str,
) -> Option<gamma::types::response::Market> {
    let base_ts = current_window_timestamp();
    let offsets = [0, WINDOW_SECS, -WINDOW_SECS];

    for offset in offsets {
        let ts = base_ts + offset;
        let slug = format!("{prefix}-{ts}");
        if let Some(market) = fetch_market_by_slug(client, &slug).await
            && market.accepting_orders.unwrap_or(false)
        {
            return Some(market);
        }
    }
    None
}

/// Convert a Gamma API market to our domain MarketInfo.
/// Returns None if required fields are missing.
fn convert_market(market: &gamma::types::response::Market) -> Option<MarketInfo> {
    let condition_id = market.condition_id.as_ref()?;
    let question = market.question.as_ref()?;
    let slug = market.slug.as_ref()?;
    let end_date = market.end_date?;
    let clob_token_ids = market.clob_token_ids.as_ref()?;

    if clob_token_ids.len() < 2 {
        return None;
    }

    Some(MarketInfo {
        id: condition_id.to_string(),
        slug: slug.clone(),
        question: question.clone(),
        end_date,
        token_ids: TokenIds {
            outcome_a: clob_token_ids[0].to_string(),
            outcome_b: clob_token_ids[1].to_string(),
        },
        accepting_orders: market.accepting_orders.unwrap_or(false),
        neg_risk: market.neg_risk.unwrap_or(false),
    })
}

#[async_trait]
impl MarketDataFeed for DiscoveryFeed {
    async fn start(&mut self, event_bus: EventBus) -> Result<()> {
        info!("starting market discovery feed (slug-based lookup)");

        let config = self.config.clone();
        let bus = event_bus.clone();
        self.event_bus = Some(event_bus);

        tokio::spawn(async move {
            let client = gamma::Client::default();
            let slug_map = build_slug_map(&config.coins);
            // Track the last known slug per coin for expiry detection.
            let mut last_slugs: HashMap<String, (String, MarketId)> = HashMap::new();
            let mut consecutive_failures: u32 = 0;

            loop {
                match poll_by_slugs(&client, &slug_map, &mut last_slugs, &bus).await {
                    Ok(()) => {
                        consecutive_failures = 0;
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        if consecutive_failures > 3 {
                            let backoff = std::cmp::min(
                                config.poll_interval_secs * 2u64.pow(consecutive_failures - 3),
                                300,
                            );
                            warn!(
                                error = %e,
                                failures = consecutive_failures,
                                backoff_secs = backoff,
                                "discovery poll failure, backing off"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                            continue;
                        }
                        warn!(error = %e, failures = consecutive_failures, "discovery poll failed");
                    }
                }

                tokio::time::sleep(std::time::Duration::from_secs(config.poll_interval_secs))
                    .await;
            }
        });

        Ok(())
    }

    async fn subscribe_market(&mut self, _market: &MarketInfo) -> Result<()> {
        // Discovery is global — no per-market subscription needed
        Ok(())
    }

    async fn unsubscribe_market(&mut self, _market_id: &str) -> Result<()> {
        // Discovery is global — no per-market unsubscription needed
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        info!("stopping market discovery feed");
        self.event_bus = None;
        Ok(())
    }
}

/// Poll for markets using deterministic slug construction.
/// For each coin: build slug → fetch → compare with last known → publish events.
async fn poll_by_slugs(
    client: &gamma::Client,
    slug_map: &[(String, String)],
    last_slugs: &mut HashMap<String, (String, MarketId)>,
    bus: &EventBus,
) -> Result<()> {
    for (coin, prefix) in slug_map {
        let market = match find_market_for_coin(client, prefix).await {
            Some(m) => m,
            None => {
                debug!(coin = %coin, "no active market found");
                continue;
            }
        };

        let info = match convert_market(&market) {
            Some(info) => info,
            None => {
                debug!(coin = %coin, "market missing required fields");
                continue;
            }
        };

        // Check if this is a new slug (market rotation)
        if let Some((old_slug, old_market_id)) = last_slugs.get(coin) {
            if *old_slug == info.slug {
                // Same market, no change
                continue;
            }
            // Slug changed → old market expired
            info!(
                coin = %coin,
                old_slug = %old_slug,
                new_slug = %info.slug,
                "market rotated"
            );
            bus.publish(Event::MarketData(MarketDataEvent::MarketExpired(
                old_market_id.clone(),
            )));
        }

        info!(
            coin = %coin,
            market_id = %info.id,
            slug = %info.slug,
            question = %info.question,
            end_date = %info.end_date,
            "discovered crypto market"
        );

        let slug = info.slug.clone();
        let market_id = info.id.clone();
        bus.publish(Event::MarketData(MarketDataEvent::MarketDiscovered(info)));
        last_slugs.insert(coin.clone(), (slug, market_id));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_slug_map() {
        let coins = vec!["BTC".to_string(), "ETH".to_string()];
        let map = build_slug_map(&coins);
        assert_eq!(map.len(), 2);
        assert_eq!(map[0], ("BTC".to_string(), "btc-updown-15m".to_string()));
        assert_eq!(map[1], ("ETH".to_string(), "eth-updown-15m".to_string()));
    }

    #[test]
    fn test_build_slug_map_case_insensitive() {
        let coins = vec!["btc".to_string(), "sol".to_string()];
        let map = build_slug_map(&coins);
        assert_eq!(map.len(), 2);
        assert_eq!(map[0].0, "BTC");
        assert_eq!(map[1].0, "SOL");
    }

    #[test]
    fn test_build_slug_map_unknown_coin() {
        let coins = vec!["DOGE".to_string()];
        let map = build_slug_map(&coins);
        assert!(map.is_empty());
    }

    #[test]
    fn test_current_window_timestamp_aligned() {
        let ts = current_window_timestamp();
        // Must be divisible by 900 (15 minutes)
        assert_eq!(ts % WINDOW_SECS, 0, "timestamp {ts} not aligned to 15-min window");
    }

    #[test]
    fn test_discovery_config_default() {
        let config = DiscoveryConfig::default();
        assert_eq!(config.poll_interval_secs, 30);
        assert_eq!(config.coins.len(), 4);
        assert!(config.coins.contains(&"BTC".to_string()));
        assert!(config.coins.contains(&"ETH".to_string()));
        assert!(config.coins.contains(&"SOL".to_string()));
        assert!(config.coins.contains(&"XRP".to_string()));
    }

    #[test]
    fn test_slug_format() {
        let ts = current_window_timestamp();
        let slug = format!("btc-updown-15m-{ts}");
        assert!(slug.starts_with("btc-updown-15m-"));
        // Timestamp should be a reasonable unix time (after 2024)
        assert!(ts > 1_700_000_000);
    }
}
