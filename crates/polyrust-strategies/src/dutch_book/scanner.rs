use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use polyrust_core::types::{MarketInfo, TokenIds};

use super::config::DutchBookConfig;

const GAMMA_BASE_URL: &str = "https://gamma-api.polymarket.com";
const PAGE_LIMIT: u64 = 100;
const REQUEST_TIMEOUT_SECS: u64 = 30;
const MAX_RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 1000;

/// Gamma API market response for the `/markets` endpoint.
///
/// The Gamma API returns `clobTokenIds` as a JSON-encoded string
/// (e.g. `"[\"id1\", \"id2\"]"`) and numeric fields as strings.
#[derive(Debug, Deserialize)]
pub(crate) struct GammaMarketResponse {
    #[serde(rename = "conditionId")]
    pub condition_id: Option<String>,
    pub slug: Option<String>,
    pub question: Option<String>,
    #[serde(rename = "startDate")]
    pub start_date: Option<String>,
    #[serde(rename = "endDate")]
    pub end_date: Option<String>,
    #[serde(rename = "clobTokenIds", default)]
    pub clob_token_ids: Option<String>,
    #[serde(rename = "negRisk", default)]
    pub neg_risk: Option<bool>,
    #[serde(rename = "acceptingOrders", default)]
    pub accepting_orders: Option<bool>,
    /// Liquidity in USD (string from API, e.g. "12345.67")
    pub liquidity: Option<String>,
    /// Minimum order size in shares
    #[serde(rename = "orderMinSize", default)]
    pub order_min_size: Option<f64>,
    /// Tick size for price rounding
    #[serde(rename = "orderPriceMinTickSize", default)]
    pub order_price_min_tick_size: Option<f64>,
    /// Maker base fee (basis points)
    #[serde(rename = "makerBaseFee", default)]
    pub maker_base_fee: Option<f64>,
}

/// Discovers active Polymarket markets via the Gamma API.
///
/// Queries the `/markets` endpoint with pagination and filters,
/// converting results to `MarketInfo` for strategy consumption.
pub struct GammaScanner {
    client: Client,
    config: DutchBookConfig,
}

impl GammaScanner {
    pub fn new(config: DutchBookConfig) -> std::result::Result<Self, String> {
        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

        Ok(Self { client, config })
    }

    /// Scan for active markets that pass configured filters.
    ///
    /// Paginates through the Gamma API `/markets` endpoint, filtering by:
    /// - `active=true`, `closed=false`
    /// - `accepting_orders` is true
    /// - Liquidity >= `min_liquidity_usd`
    /// - End date within `max_days_until_resolution`
    /// - Has exactly 2 CLOB token IDs
    ///
    /// Deduplicates against `known_market_ids` (already subscribed).
    pub async fn scan_markets(
        &self,
        known_market_ids: &HashSet<String>,
    ) -> std::result::Result<Vec<MarketInfo>, String> {
        let now = Utc::now();
        let max_end = now + chrono::Duration::days(self.config.max_days_until_resolution as i64);
        let mut all_markets = Vec::new();
        let mut offset = 0u64;

        loop {
            let page = self.fetch_page(offset).await?;
            let page_len = page.len();

            for raw in page {
                if let Some(info) = self.convert_and_filter(raw, now, max_end) && !known_market_ids.contains(&info.id) {
                    all_markets.push(info);
                }
            }

            // Stop when we get fewer results than the page limit (last page)
            if (page_len as u64) < PAGE_LIMIT {
                break;
            }
            offset += PAGE_LIMIT;
        }

        info!(
            new_markets = all_markets.len(),
            known = known_market_ids.len(),
            "Gamma scan complete"
        );

        Ok(all_markets)
    }

    /// Fetch a single page from the Gamma API.
    async fn fetch_page(
        &self,
        offset: u64,
    ) -> std::result::Result<Vec<GammaMarketResponse>, String> {
        let url = format!(
            "{}/markets?active=true&closed=false&limit={}&offset={}",
            GAMMA_BASE_URL, PAGE_LIMIT, offset
        );

        let response = self.fetch_with_retry(&url).await?;
        response
            .json::<Vec<GammaMarketResponse>>()
            .await
            .map_err(|e| format!("Failed to parse Gamma markets response: {e}"))
    }

    /// HTTP GET with exponential backoff retry.
    async fn fetch_with_retry(
        &self,
        url: &str,
    ) -> std::result::Result<reqwest::Response, String> {
        let mut attempts = 0;

        loop {
            attempts += 1;
            match self.client.get(url).send().await {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response) => {
                    let status = response.status();
                    if attempts >= MAX_RETRIES {
                        return Err(format!(
                            "HTTP error {} after {MAX_RETRIES} retries",
                            status
                        ));
                    }
                    warn!(
                        %url, %status, attempts, max_retries = MAX_RETRIES,
                        "Gamma API request failed, retrying"
                    );
                }
                Err(e) => {
                    if attempts >= MAX_RETRIES {
                        return Err(format!(
                            "Request failed after {MAX_RETRIES} retries: {e}"
                        ));
                    }
                    warn!(
                        %url, %e, attempts, max_retries = MAX_RETRIES,
                        "Gamma API request error, retrying"
                    );
                }
            }

            let delay_ms = RETRY_DELAY_MS * 2_u64.pow(attempts - 1);
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }

    /// Convert a raw Gamma API response to `MarketInfo`, applying all filters.
    /// Returns `None` if the market should be skipped.
    pub(crate) fn convert_and_filter(
        &self,
        raw: GammaMarketResponse,
        now: DateTime<Utc>,
        max_end: DateTime<Utc>,
    ) -> Option<MarketInfo> {
        // Must be accepting orders
        if !raw.accepting_orders.unwrap_or(false) {
            return None;
        }

        // Must have required fields
        let condition_id = raw.condition_id.as_ref()?;
        let slug = raw.slug.as_ref()?;
        let question = raw.question.as_ref()?;

        // Must have end_date and it must be in the future but within our window
        let end_date_str = raw.end_date.as_ref()?;
        let end_date = DateTime::parse_from_rfc3339(end_date_str)
            .ok()?
            .with_timezone(&Utc);
        if end_date <= now || end_date > max_end {
            return None;
        }

        // Parse start_date (optional)
        let start_date = raw.start_date.as_ref().and_then(|s| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        });

        // Must have 2 CLOB token IDs
        let token_ids_json = raw.clob_token_ids.as_ref()?;
        let token_ids: Vec<String> = serde_json::from_str(token_ids_json).ok()?;
        if token_ids.len() != 2 {
            return None;
        }

        // Check liquidity threshold
        let liquidity = raw
            .liquidity
            .as_ref()
            .and_then(|s| s.parse::<Decimal>().ok())
            .unwrap_or(Decimal::ZERO);
        if liquidity < self.config.min_liquidity_usd {
            return None;
        }

        // Parse optional fields with defaults
        let min_order_size = raw
            .order_min_size
            .and_then(|f| Decimal::try_from(f).ok())
            .unwrap_or(Decimal::new(5, 0));
        let tick_size = raw
            .order_price_min_tick_size
            .and_then(|f| Decimal::try_from(f).ok())
            .unwrap_or(Decimal::new(1, 2));
        let fee_rate_bps = raw.maker_base_fee.map(|f| f.round() as u32).unwrap_or(0);

        debug!(
            condition_id,
            %liquidity,
            %end_date,
            "Market passed filters"
        );

        Some(MarketInfo {
            id: condition_id.clone(),
            slug: slug.clone(),
            question: question.clone(),
            start_date,
            end_date,
            token_ids: TokenIds {
                outcome_a: token_ids[0].clone(),
                outcome_b: token_ids[1].clone(),
            },
            accepting_orders: true,
            neg_risk: raw.neg_risk.unwrap_or(false),
            min_order_size,
            tick_size,
            fee_rate_bps,
        })
    }

    /// Start background scanning task that periodically discovers markets
    /// and pushes new ones to the pending queue.
    ///
    /// Returns a `JoinHandle` that can be aborted to stop scanning.
    pub fn start_scanner(
        config: DutchBookConfig,
        pending_subscriptions: Arc<Mutex<Vec<MarketInfo>>>,
        known_market_ids: Arc<Mutex<HashSet<String>>>,
    ) -> tokio::task::JoinHandle<()> {
        let interval = Duration::from_secs(config.scan_interval_secs);

        tokio::spawn(async move {
            info!(
                interval_secs = config.scan_interval_secs,
                "Dutch Book scanner started"
            );

            let scanner = match GammaScanner::new(config) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "Failed to create GammaScanner — scanner will not run");
                    return;
                }
            };

            // Run first scan immediately, then loop on interval
            loop {
                let known = known_market_ids.lock().await.clone();
                match scanner.scan_markets(&known).await {
                    Ok(new_markets) => {
                        let count = new_markets.len();
                        if count > 0 {
                            let mut known = known_market_ids.lock().await;
                            let mut pending = pending_subscriptions.lock().await;
                            for market in new_markets {
                                known.insert(market.id.clone());
                                pending.push(market);
                            }
                            info!(count, "Queued new markets for subscription");
                        } else {
                            debug!("No new markets found in scan");
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Gamma scan failed");
                    }
                }

                tokio::time::sleep(interval).await;
            }
        })
    }
}
