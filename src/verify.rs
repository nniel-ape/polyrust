//! API connectivity verification (`--verify` CLI command).
//!
//! Lightweight smoke tests for external service connectivity.
//! Each check runs with a timeout and reports PASS/FAIL/SKIP.

use std::time::Duration;

use tracing::info;

/// Individual check result.
#[derive(Debug)]
enum CheckResult {
    Pass(String),
    Fail(String),
    Skip(String),
}

impl std::fmt::Display for CheckResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckResult::Pass(msg) => write!(f, "  PASS  {msg}"),
            CheckResult::Fail(msg) => write!(f, "  FAIL  {msg}"),
            CheckResult::Skip(msg) => write!(f, "  SKIP  {msg}"),
        }
    }
}

/// Run all verification checks and report results.
pub async fn run_verify() -> anyhow::Result<()> {
    info!("Running API connectivity checks...");
    println!("\n=== Polyrust API Connectivity Verification ===\n");

    let mut results = Vec::new();

    // Run independent checks concurrently
    let (gamma, chainlink) = tokio::join!(check_gamma_api(), check_chainlink());
    results.push(gamma);
    results.push(chainlink);

    // Print results
    println!();
    let mut pass_count = 0;
    let mut fail_count = 0;
    let mut skip_count = 0;
    for result in &results {
        println!("{result}");
        match result {
            CheckResult::Pass(_) => pass_count += 1,
            CheckResult::Fail(_) => fail_count += 1,
            CheckResult::Skip(_) => skip_count += 1,
        }
    }

    println!("\n--- Summary: {pass_count} passed, {fail_count} failed, {skip_count} skipped ---\n");

    if fail_count > 0 {
        anyhow::bail!("{fail_count} connectivity check(s) failed");
    }

    Ok(())
}

/// Check Gamma API: fetch a known market slug.
async fn check_gamma_api() -> CheckResult {
    use polymarket_client_sdk::gamma;

    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let client = gamma::Client::default();
        let request = gamma::types::request::MarketBySlugRequest::builder()
            .slug("will-bitcoin-go-up-or-down")
            .build();
        client.market_by_slug(&request).await
    })
    .await;

    match result {
        Ok(Ok(_market)) => {
            CheckResult::Pass("Gamma API — market lookup succeeded".to_string())
        }
        Ok(Err(e)) => {
            let msg = e.to_string();
            // A 404 still proves connectivity (slug may not exist)
            if msg.contains("404") || msg.contains("not found") || msg.contains("Not Found") {
                CheckResult::Pass(
                    "Gamma API — reachable (slug not found, but API responded)".to_string(),
                )
            } else {
                CheckResult::Fail(format!("Gamma API — {msg}"))
            }
        }
        Err(_) => CheckResult::Fail("Gamma API — timeout (15s)".to_string()),
    }
}

/// Check Chainlink Oracle (requires POLY_RPC_URLS).
async fn check_chainlink() -> CheckResult {
    let rpc_urls = match std::env::var("POLY_RPC_URLS") {
        Ok(urls) => urls
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>(),
        Err(_) => {
            return CheckResult::Skip(
                "Chainlink Oracle — POLY_RPC_URLS not set".to_string(),
            );
        }
    };

    if rpc_urls.is_empty() {
        return CheckResult::Skip("Chainlink Oracle — POLY_RPC_URLS empty".to_string());
    }

    let result = tokio::time::timeout(Duration::from_secs(15), async {
        use polyrust_market::ChainlinkHistoricalClient;
        let client = ChainlinkHistoricalClient::new(rpc_urls);
        let now = chrono::Utc::now().timestamp() as u64;
        client.get_price_at_timestamp("BTC", now, 10).await
    })
    .await;

    match result {
        Ok(Ok(price)) => {
            CheckResult::Pass(format!(
                "Chainlink Oracle — BTC price: ${}",
                price.price
            ))
        }
        Ok(Err(e)) => CheckResult::Fail(format!("Chainlink Oracle — {e}")),
        Err(_) => CheckResult::Fail("Chainlink Oracle — timeout (15s)".to_string()),
    }
}
