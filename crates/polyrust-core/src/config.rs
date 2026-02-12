use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub engine: EngineConfig,
    #[serde(default)]
    pub polymarket: PolymarketConfig,
    #[serde(default)]
    pub dashboard: DashboardConfig,
    #[serde(default)]
    pub store: StoreConfig,
    #[serde(default)]
    pub paper: PaperConfig,
    #[serde(default)]
    pub auto_claim: AutoClaimConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default = "default_event_bus_capacity")]
    pub event_bus_capacity: usize,
    #[serde(default = "default_health_interval")]
    pub health_check_interval_secs: u64,
    /// Order reconciliation interval in seconds (0 = disabled). Default: 15.
    /// Periodically polls open orders from the execution backend and publishes
    /// OpenOrderSnapshot events so strategies can detect filled GTC orders.
    #[serde(default = "default_reconcile_interval")]
    pub reconcile_interval_secs: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            event_bus_capacity: default_event_bus_capacity(),
            health_check_interval_secs: default_health_interval(),
            reconcile_interval_secs: default_reconcile_interval(),
        }
    }
}

fn default_event_bus_capacity() -> usize {
    4096
}
fn default_health_interval() -> u64 {
    30
}
fn default_reconcile_interval() -> u64 {
    15
}

#[derive(Clone, Deserialize)]
pub struct PolymarketConfig {
    pub private_key: Option<String>,
    pub safe_address: Option<String>,
    pub builder_api_key: Option<String>,
    pub builder_api_secret: Option<String>,
    pub builder_api_passphrase: Option<String>,
    /// Polygon RPC endpoints for on-chain queries (Chainlink oracles).
    /// Tried in order with automatic failover.
    #[serde(default = "default_rpc_urls")]
    pub rpc_urls: Vec<String>,
    /// Relayer API URL for gasless transactions.
    /// Defaults to "https://relayer-v2.polymarket.com".
    pub relayer_url: Option<String>,
    /// Route Safe transactions through Polymarket's gas-sponsored relayer.
    /// Defaults to true when builder credentials are present.
    #[serde(default = "default_use_relayer")]
    pub use_relayer: bool,
}

fn default_rpc_urls() -> Vec<String> {
    vec!["https://polygon-rpc.com".to_string()]
}
fn default_use_relayer() -> bool {
    true
}

impl Default for PolymarketConfig {
    fn default() -> Self {
        Self {
            private_key: None,
            safe_address: None,
            builder_api_key: None,
            builder_api_secret: None,
            builder_api_passphrase: None,
            rpc_urls: default_rpc_urls(),
            relayer_url: None,
            use_relayer: default_use_relayer(),
        }
    }
}

impl Serialize for PolymarketConfig {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("PolymarketConfig", 8)?;
        s.serialize_field(
            "private_key",
            &self.private_key.as_ref().map(|_| "[REDACTED]"),
        )?;
        s.serialize_field("safe_address", &self.safe_address)?;
        s.serialize_field(
            "builder_api_key",
            &self.builder_api_key.as_ref().map(|_| "[REDACTED]"),
        )?;
        s.serialize_field(
            "builder_api_secret",
            &self.builder_api_secret.as_ref().map(|_| "[REDACTED]"),
        )?;
        s.serialize_field(
            "builder_api_passphrase",
            &self.builder_api_passphrase.as_ref().map(|_| "[REDACTED]"),
        )?;
        s.serialize_field("rpc_urls", &self.rpc_urls)?;
        s.serialize_field("relayer_url", &self.relayer_url)?;
        s.serialize_field("use_relayer", &self.use_relayer)?;
        s.end()
    }
}

impl std::fmt::Debug for PolymarketConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolymarketConfig")
            .field(
                "private_key",
                &self.private_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("safe_address", &self.safe_address)
            .field(
                "builder_api_key",
                &self.builder_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "builder_api_secret",
                &self.builder_api_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "builder_api_passphrase",
                &self.builder_api_passphrase.as_ref().map(|_| "[REDACTED]"),
            )
            .field("rpc_urls", &self.rpc_urls)
            .field("relayer_url", &self.relayer_url)
            .field("use_relayer", &self.use_relayer)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    #[serde(default = "default_dashboard_enabled")]
    pub enabled: bool,
    #[serde(default = "default_dashboard_port")]
    pub port: u16,
    #[serde(default = "default_dashboard_host")]
    pub host: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: default_dashboard_enabled(),
            port: default_dashboard_port(),
            host: default_dashboard_host(),
        }
    }
}

fn default_dashboard_enabled() -> bool {
    true
}
fn default_dashboard_port() -> u16 {
    3000
}
fn default_dashboard_host() -> String {
    "127.0.0.1".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    #[serde(default = "default_db_path")]
    pub db_path: String,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
        }
    }
}

fn default_db_path() -> String {
    "polyrust.db".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperConfig {
    #[serde(default = "default_paper_enabled")]
    pub enabled: bool,
    #[serde(default = "default_initial_balance")]
    pub initial_balance: Decimal,
}

fn default_paper_enabled() -> bool {
    true
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            initial_balance: default_initial_balance(),
        }
    }
}

fn default_initial_balance() -> Decimal {
    Decimal::new(10_000, 0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoClaimConfig {
    #[serde(default = "default_auto_claim_enabled")]
    pub enabled: bool,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_backoff")]
    pub retry_backoff_secs: u64,
    #[serde(default = "default_gas_pause_duration")]
    pub gas_pause_duration_secs: u64,
    /// Max seconds to wait after first resolved claim before flushing batch.
    /// Set to 0 to disable accumulation (immediate flush, backward-compatible).
    #[serde(default = "default_batch_window")]
    pub batch_window_secs: u64,
    /// Flush early when this many resolved claims are ready (before window elapses).
    #[serde(default = "default_batch_min_count")]
    pub batch_min_count: usize,
    /// Minimum seconds after on-chain resolution before attempting redemption.
    /// Prevents racing with the settlement transaction (resolution detected but
    /// payout not yet finalized). Default 30s.
    #[serde(default = "default_settlement_delay")]
    pub settlement_delay_secs: u64,
}

impl Default for AutoClaimConfig {
    fn default() -> Self {
        Self {
            enabled: default_auto_claim_enabled(),
            poll_interval_secs: default_poll_interval(),
            max_retries: default_max_retries(),
            retry_backoff_secs: default_retry_backoff(),
            gas_pause_duration_secs: default_gas_pause_duration(),
            batch_window_secs: default_batch_window(),
            batch_min_count: default_batch_min_count(),
            settlement_delay_secs: default_settlement_delay(),
        }
    }
}

fn default_auto_claim_enabled() -> bool {
    true
}
fn default_poll_interval() -> u64 {
    300 // 5 minutes
}
fn default_max_retries() -> u32 {
    10
}
fn default_retry_backoff() -> u64 {
    60 // 1 minute
}
fn default_gas_pause_duration() -> u64 {
    900 // 15 minutes
}
fn default_batch_window() -> u64 {
    300 // 5 minutes
}
fn default_batch_min_count() -> usize {
    3
}
fn default_settlement_delay() -> u64 {
    30 // 30 seconds
}

impl Config {
    /// Load config from a TOML file.
    pub fn from_file(path: impl AsRef<Path>) -> crate::error::Result<Self> {
        let contents = std::fs::read_to_string(path.as_ref())
            .map_err(|e| crate::error::PolyError::Config(format!("Failed to read config: {e}")))?;
        toml::from_str(&contents)
            .map_err(|e| crate::error::PolyError::Config(format!("Failed to parse config: {e}")))
    }

    /// Apply POLY_* environment variable overrides.
    pub fn with_env_overrides(mut self) -> Self {
        // Detect if TOML file populated polymarket secrets (deprecated path)
        let toml_had_secrets = self.polymarket.private_key.is_some()
            || self.polymarket.builder_api_key.is_some();

        if let Ok(v) = std::env::var("POLY_PRIVATE_KEY") {
            self.polymarket.private_key = Some(v);
        }
        if let Ok(v) = std::env::var("POLY_SAFE_ADDRESS") {
            self.polymarket.safe_address = Some(v);
        }
        if let Ok(v) = std::env::var("POLY_BUILDER_API_KEY") {
            self.polymarket.builder_api_key = Some(v);
        }
        if let Ok(v) = std::env::var("POLY_BUILDER_API_SECRET") {
            self.polymarket.builder_api_secret = Some(v);
        }
        if let Ok(v) = std::env::var("POLY_BUILDER_API_PASSPHRASE") {
            self.polymarket.builder_api_passphrase = Some(v);
        }
        if let Ok(v) = std::env::var("POLY_DASHBOARD_PORT")
            && let Ok(port) = v.parse()
        {
            self.dashboard.port = port;
        }
        if let Ok(v) = std::env::var("POLY_DB_PATH") {
            self.store.db_path = v;
        }
        if let Ok(v) = std::env::var("POLY_PAPER_TRADING") {
            self.paper.enabled = v == "true" || v == "1";
        }
        // POLY_RELAYER_URL — custom relayer endpoint
        if let Ok(v) = std::env::var("POLY_RELAYER_URL") {
            self.polymarket.relayer_url = Some(v);
        }
        // POLY_USE_RELAYER — enable/disable relayer
        if let Ok(v) = std::env::var("POLY_USE_RELAYER") {
            self.polymarket.use_relayer = v == "true" || v == "1";
        }
        // POLY_RPC_URLS — comma-separated list of RPC endpoints
        if let Ok(urls) = std::env::var("POLY_RPC_URLS") {
            self.polymarket.rpc_urls = urls
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        // POLY_DASHBOARD_HOST — dashboard bind address
        if let Ok(v) = std::env::var("POLY_DASHBOARD_HOST") {
            self.dashboard.host = v;
        }

        if toml_had_secrets {
            tracing::warn!(
                "[polymarket] secrets in config.toml are deprecated — use POLY_* env vars in .env instead"
            );
        }

        self
    }
}
