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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    #[serde(default = "default_event_bus_capacity")]
    pub event_bus_capacity: usize,
    #[serde(default = "default_health_interval")]
    pub health_check_interval_secs: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            event_bus_capacity: default_event_bus_capacity(),
            health_check_interval_secs: default_health_interval(),
        }
    }
}

fn default_event_bus_capacity() -> usize {
    4096
}
fn default_health_interval() -> u64 {
    30
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct PolymarketConfig {
    pub private_key: Option<String>,
    pub safe_address: Option<String>,
    pub builder_api_key: Option<String>,
    pub builder_api_secret: Option<String>,
    pub builder_api_passphrase: Option<String>,
}

impl std::fmt::Debug for PolymarketConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolymarketConfig")
            .field("private_key", &self.private_key.as_ref().map(|_| "[REDACTED]"))
            .field("safe_address", &self.safe_address)
            .field("builder_api_key", &self.builder_api_key.as_ref().map(|_| "[REDACTED]"))
            .field("builder_api_secret", &self.builder_api_secret.as_ref().map(|_| "[REDACTED]"))
            .field("builder_api_passphrase", &self.builder_api_passphrase.as_ref().map(|_| "[REDACTED]"))
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
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_initial_balance")]
    pub initial_balance: Decimal,
}

impl Default for PaperConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            initial_balance: default_initial_balance(),
        }
    }
}

fn default_initial_balance() -> Decimal {
    Decimal::new(10_000, 0)
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
        self
    }
}
