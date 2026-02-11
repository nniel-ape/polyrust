//! API connectivity verification (`--verify` CLI command).
//!
//! Lightweight smoke tests for external service connectivity.
//! Each check runs with a timeout and reports PASS/FAIL/SKIP.

use std::str::FromStr;
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

    // Run all independent checks concurrently
    let (gamma, chainlink, clob_auth, approvals) = tokio::join!(
        check_gamma_api(),
        check_chainlink(),
        check_clob_auth(),
        check_approvals(),
    );

    let results = [gamma, chainlink, clob_auth, approvals];

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

/// Load private key from POLY_PRIVATE_KEY env var.
fn load_private_key() -> Option<String> {
    std::env::var("POLY_PRIVATE_KEY").ok().filter(|s| !s.is_empty())
}

/// Load RPC URLs from POLY_RPC_URLS env var.
fn load_rpc_urls() -> Vec<String> {
    std::env::var("POLY_RPC_URLS")
        .ok()
        .map(|urls| {
            urls.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
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
        Ok(Ok(_market)) => CheckResult::Pass("Gamma API — market lookup succeeded".to_string()),
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
    let rpc_urls = load_rpc_urls();

    if rpc_urls.is_empty() {
        return CheckResult::Skip("Chainlink Oracle — POLY_RPC_URLS not set".to_string());
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
            CheckResult::Pass(format!("Chainlink Oracle — BTC price: ${}", price.price))
        }
        Ok(Err(e)) => CheckResult::Fail(format!("Chainlink Oracle — {e}")),
        Err(_) => CheckResult::Fail("Chainlink Oracle — timeout (15s)".to_string()),
    }
}

/// Check CLOB API authentication (requires POLY_PRIVATE_KEY).
///
/// Mirrors the auth flow in `LiveBackend::new()`: parses the private key,
/// creates a CLOB client, optionally sets the Safe funder, and authenticates.
async fn check_clob_auth() -> CheckResult {
    use polymarket_client_sdk::POLYGON;
    use polymarket_client_sdk::auth::{LocalSigner, Signer as _};
    use polymarket_client_sdk::clob::types::SignatureType;
    use polymarket_client_sdk::clob::{Client, Config as SdkConfig};
    use polymarket_client_sdk::types::Address as SdkAddress;

    let private_key = match load_private_key() {
        Some(pk) => pk,
        None => return CheckResult::Skip("CLOB Auth — POLY_PRIVATE_KEY not set".to_string()),
    };

    let result = tokio::time::timeout(Duration::from_secs(15), async {
        let signer = LocalSigner::from_str(&private_key)
            .map_err(|e| format!("Invalid private key: {e}"))?
            .with_chain_id(Some(POLYGON));

        let sdk_config = SdkConfig::builder().use_server_time(true).build();
        let client = Client::new("https://clob.polymarket.com", sdk_config)
            .map_err(|e| format!("Failed to create SDK client: {e}"))?;

        let mut auth_builder = client.authentication_builder(&signer);

        if let Ok(safe_addr) = std::env::var("POLY_SAFE_ADDRESS")
            && !safe_addr.is_empty()
        {
            let funder = SdkAddress::from_str(&safe_addr)
                .map_err(|e| format!("Invalid safe address: {e}"))?;
            auth_builder = auth_builder
                .funder(funder)
                .signature_type(SignatureType::GnosisSafe);
        }

        let authenticated = auth_builder
            .authenticate()
            .await
            .map_err(|e| format!("Authentication failed: {e}"))?;

        Ok::<_, String>(authenticated.address().to_string())
    })
    .await;

    match result {
        Ok(Ok(address)) => CheckResult::Pass(format!("CLOB Auth — authenticated as {address}")),
        Ok(Err(e)) => CheckResult::Fail(format!("CLOB Auth — {e}")),
        Err(_) => CheckResult::Fail("CLOB Auth — timeout (15s)".to_string()),
    }
}

/// Check on-chain token approvals (requires POLY_PRIVATE_KEY + POLY_RPC_URLS).
///
/// Determines the owner address (Safe if set, else EOA from private key) and
/// calls `check_approvals_readonly()` to verify all 7 required approvals.
async fn check_approvals() -> CheckResult {
    use alloy::primitives::Address;
    use alloy::signers::local::PrivateKeySigner;

    let private_key = match load_private_key() {
        Some(pk) => pk,
        None => return CheckResult::Skip("Approvals — POLY_PRIVATE_KEY not set".to_string()),
    };

    let rpc_urls = load_rpc_urls();
    if rpc_urls.is_empty() {
        return CheckResult::Skip("Approvals — POLY_RPC_URLS not set".to_string());
    }

    let result = tokio::time::timeout(Duration::from_secs(15), async {
        // Determine owner: Safe address if set, else EOA from private key
        let owner: Address =
            if let Ok(safe_addr) = std::env::var("POLY_SAFE_ADDRESS")
                && !safe_addr.is_empty()
            {
                safe_addr
                    .parse()
                    .map_err(|e| format!("Invalid POLY_SAFE_ADDRESS: {e}"))?
            } else {
                let signer: PrivateKeySigner = private_key
                    .parse()
                    .map_err(|e| format!("Invalid private key: {e}"))?;
                signer.address()
            };

        let statuses = polyrust_execution::check_approvals_readonly(&rpc_urls[0], owner)
            .await
            .map_err(|e| format!("{e}"))?;

        let missing: Vec<&str> = statuses
            .iter()
            .filter(|s| !s.approved)
            .map(|s| s.name)
            .collect();

        if missing.is_empty() {
            Ok(format!("all 7 approvals set for {owner}"))
        } else {
            Err(format!(
                "missing {} approval(s) for {owner}: {}",
                missing.len(),
                missing.join(", ")
            ))
        }
    })
    .await;

    match result {
        Ok(Ok(msg)) => CheckResult::Pass(format!("Approvals — {msg}")),
        Ok(Err(e)) => CheckResult::Fail(format!("Approvals — {e}")),
        Err(_) => CheckResult::Fail("Approvals — timeout (15s)".to_string()),
    }
}
