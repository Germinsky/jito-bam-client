/// BAM-Ready RPC Provider management for production Solana execution.
///
/// Standard public RPCs do not support `sendBundle` or `sendPrivateTransaction`.
/// This module provides a registry of BAM-enabled endpoints (Triton, Helius,
/// QuickNode, Jito native) with:
///
/// - **API-key injection** — keys loaded from env vars or config, never
///   hardcoded.
/// - **Health checking** — periodic latency probes with automatic failover.
/// - **Region-aware selection** — choose the lowest-latency endpoint for
///   your deployment region.
/// - **Provider abstraction** — switch between Helius BAM Relay, Triton,
///   QuickNode, or bare Jito block-engine without code changes.
///
/// # Usage
///
/// ```rust,no_run
/// use jito_bam_client::bam_rpc::{BamEndpointRegistry, BamProvider};
///
/// let mut registry = BamEndpointRegistry::new();
///
/// // Add providers from env vars (API keys stay in env, not in code)
/// registry.add_from_env(BamProvider::Helius, "HELIUS_API_KEY");
/// registry.add_from_env(BamProvider::Triton, "TRITON_API_KEY");
/// registry.add_jito_native(None); // no UUID required for free tier
///
/// // Select the best endpoint (lowest recent latency)
/// let endpoint = registry.best_endpoint();
/// println!("Using: {} ({}ms avg)", endpoint.url, endpoint.avg_latency_ms);
/// ```
use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

// ═══════════════════════════════════════════════════════════════════════════
//  Provider definitions
// ═══════════════════════════════════════════════════════════════════════════

/// Supported BAM-enabled RPC providers.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum BamProvider {
    /// Jito block-engine native endpoints (no relay needed).
    JitoNative,
    /// Helius BAM Relay — `https://mainnet.bam.helius.xyz`
    Helius,
    /// Triton BAM Relay — `https://mainnet.bam.triton.one`
    Triton,
    /// QuickNode BAM Relay — `https://bam.<slug>.quiknode.pro`
    QuickNode,
    /// Custom / self-hosted BAM relay.
    Custom,
}

impl std::fmt::Display for BamProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::JitoNative => write!(f, "Jito Native"),
            Self::Helius => write!(f, "Helius BAM"),
            Self::Triton => write!(f, "Triton BAM"),
            Self::QuickNode => write!(f, "QuickNode BAM"),
            Self::Custom => write!(f, "Custom BAM"),
        }
    }
}

/// Known deployment regions for latency-aware selection.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum Region {
    UsEast,
    UsWest,
    Europe,
    Asia,
    Auto,
}

impl std::fmt::Display for Region {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UsEast => write!(f, "US-East"),
            Self::UsWest => write!(f, "US-West"),
            Self::Europe => write!(f, "Europe"),
            Self::Asia => write!(f, "Asia"),
            Self::Auto => write!(f, "Auto"),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  BAM endpoint
// ═══════════════════════════════════════════════════════════════════════════

/// A single BAM-enabled endpoint with health metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BamEndpoint {
    /// Provider identifier.
    pub provider: BamProvider,
    /// Full URL for the Jito/BAM block-engine.
    pub url: String,
    /// Full URL for Solana RPC (blockhash, account reads).
    pub rpc_url: String,
    /// Optional UUID for authenticated Jito access.
    pub uuid: Option<String>,
    /// Region this endpoint is closest to.
    pub region: Region,
    /// Whether this endpoint is currently healthy.
    pub healthy: bool,
    /// Average latency in milliseconds (rolling window).
    pub avg_latency_ms: u64,
    /// Total health check probes sent.
    pub probes_sent: u64,
    /// Total successful probes.
    pub probes_ok: u64,
    /// Timestamp of last successful probe (not serializable).
    #[serde(skip)]
    pub last_healthy: Option<Instant>,
    /// Whether this endpoint supports `sendBundle`.
    pub supports_send_bundle: bool,
    /// Whether this endpoint supports `sendPrivateTransaction`.
    pub supports_send_private_tx: bool,
}

impl BamEndpoint {
    /// Uptime ratio (0.0–1.0).
    pub fn uptime(&self) -> f64 {
        if self.probes_sent == 0 {
            return 0.0;
        }
        self.probes_ok as f64 / self.probes_sent as f64
    }

    /// Whether the endpoint has been recently probed and is responsive.
    pub fn is_fresh(&self, max_age: Duration) -> bool {
        self.last_healthy
            .map(|t| t.elapsed() < max_age)
            .unwrap_or(false)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Well-known endpoint templates
// ═══════════════════════════════════════════════════════════════════════════

/// Well-known BAM endpoint URL templates.
pub mod templates {
    // ── Jito block-engine (native) ──────────────────────────────────────
    pub const JITO_MAINNET: &str = "https://mainnet.block-engine.jito.wtf/api/v1";
    pub const JITO_AMSTERDAM: &str = "https://amsterdam.mainnet.block-engine.jito.wtf/api/v1";
    pub const JITO_FRANKFURT: &str = "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1";
    pub const JITO_NY: &str = "https://ny.mainnet.block-engine.jito.wtf/api/v1";
    pub const JITO_TOKYO: &str = "https://tokyo.mainnet.block-engine.jito.wtf/api/v1";
    pub const JITO_SLC: &str = "https://slc.mainnet.block-engine.jito.wtf/api/v1";

    // ── Helius BAM Relay ────────────────────────────────────────────────
    /// Template: replace `{API_KEY}` with your Helius key.
    pub const HELIUS_BAM_TEMPLATE: &str = "https://mainnet.bam.helius.xyz/?api-key={API_KEY}";
    pub const HELIUS_RPC_TEMPLATE: &str = "https://mainnet.helius-rpc.com/?api-key={API_KEY}";

    // ── Triton BAM Relay ────────────────────────────────────────────────
    pub const TRITON_BAM_TEMPLATE: &str = "https://mainnet.bam.triton.one/{API_KEY}";
    pub const TRITON_RPC_TEMPLATE: &str = "https://mainnet.triton.one/{API_KEY}";

    // ── QuickNode BAM Relay ─────────────────────────────────────────────
    /// Template: replace `{API_KEY}` with your QuickNode endpoint slug.
    pub const QUICKNODE_BAM_TEMPLATE: &str = "https://bam.{API_KEY}.quiknode.pro";
    pub const QUICKNODE_RPC_TEMPLATE: &str = "https://{API_KEY}.quiknode.pro";

    // ── Standard public RPCs (blockhash/reads only) ─────────────────────
    pub const SOLANA_MAINNET: &str = "https://api.mainnet-beta.solana.com";

    /// Instantiate a template URL by replacing `{API_KEY}`.
    pub fn resolve(template: &str, api_key: &str) -> String {
        template.replace("{API_KEY}", api_key)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Endpoint registry
// ═══════════════════════════════════════════════════════════════════════════

/// Registry of BAM-enabled endpoints with health tracking and selection.
///
/// ```rust,no_run
/// use jito_bam_client::bam_rpc::{BamEndpointRegistry, BamProvider};
///
/// let mut reg = BamEndpointRegistry::new();
/// reg.add_from_env(BamProvider::Helius, "HELIUS_API_KEY");
/// let best = reg.best_endpoint();
/// ```
pub struct BamEndpointRegistry {
    endpoints: Vec<BamEndpoint>,
    /// Preferred region for selection.
    preferred_region: Region,
    /// Maximum age before a probe is considered stale.
    max_probe_age: Duration,
    /// Latency samples per endpoint (ring buffer).
    latency_history: HashMap<String, Vec<u64>>,
    /// Maximum latency samples to retain.
    max_latency_samples: usize,
}

impl BamEndpointRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            endpoints: Vec::new(),
            preferred_region: Region::Auto,
            max_probe_age: Duration::from_secs(60),
            latency_history: HashMap::new(),
            max_latency_samples: 50,
        }
    }

    /// Create with a preferred region.
    pub fn with_region(region: Region) -> Self {
        let mut reg = Self::new();
        reg.preferred_region = region;
        reg
    }

    // ── Adding endpoints ────────────────────────────────────────────────

    /// Add a Jito native block-engine endpoint (no API key required).
    pub fn add_jito_native(&mut self, uuid: Option<String>) {
        self.add_jito_regional(templates::JITO_MAINNET, Region::Auto, uuid.clone());
        self.add_jito_regional(templates::JITO_NY, Region::UsEast, uuid.clone());
        self.add_jito_regional(templates::JITO_SLC, Region::UsWest, uuid.clone());
        self.add_jito_regional(templates::JITO_AMSTERDAM, Region::Europe, uuid.clone());
        self.add_jito_regional(templates::JITO_FRANKFURT, Region::Europe, uuid.clone());
        self.add_jito_regional(templates::JITO_TOKYO, Region::Asia, uuid);
        info!(count = 6, "added Jito native endpoints");
    }

    fn add_jito_regional(&mut self, url: &str, region: Region, uuid: Option<String>) {
        self.endpoints.push(BamEndpoint {
            provider: BamProvider::JitoNative,
            url: url.to_string(),
            rpc_url: templates::SOLANA_MAINNET.to_string(),
            uuid,
            region,
            healthy: true,
            avg_latency_ms: 0,
            probes_sent: 0,
            probes_ok: 0,
            last_healthy: None,
            supports_send_bundle: true,
            supports_send_private_tx: true,
        });
    }

    /// Add a provider endpoint with an API key loaded from an environment
    /// variable. This is the **recommended** pattern — keys never appear
    /// in source code.
    ///
    /// Returns `true` if the env var was found and the endpoint was added.
    pub fn add_from_env(&mut self, provider: BamProvider, env_var: &str) -> bool {
        match std::env::var(env_var) {
            Ok(key) if !key.is_empty() => {
                self.add_with_key(provider, &key);
                info!(
                    provider = %provider,
                    env_var,
                    "added BAM endpoint from env"
                );
                true
            }
            _ => {
                warn!(
                    provider = %provider,
                    env_var,
                    "env var not set — skipping provider"
                );
                false
            }
        }
    }

    /// Add a provider endpoint with an explicit API key.
    pub fn add_with_key(&mut self, provider: BamProvider, api_key: &str) {
        let (bam_url, rpc_url, region) = match provider {
            BamProvider::Helius => (
                templates::resolve(templates::HELIUS_BAM_TEMPLATE, api_key),
                templates::resolve(templates::HELIUS_RPC_TEMPLATE, api_key),
                Region::UsEast,
            ),
            BamProvider::Triton => (
                templates::resolve(templates::TRITON_BAM_TEMPLATE, api_key),
                templates::resolve(templates::TRITON_RPC_TEMPLATE, api_key),
                Region::UsEast,
            ),
            BamProvider::QuickNode => (
                templates::resolve(templates::QUICKNODE_BAM_TEMPLATE, api_key),
                templates::resolve(templates::QUICKNODE_RPC_TEMPLATE, api_key),
                Region::Auto,
            ),
            BamProvider::JitoNative => {
                self.add_jito_native(Some(api_key.to_string()));
                return;
            }
            BamProvider::Custom => {
                // For custom, treat `api_key` as the full BAM URL
                self.add_custom(api_key, templates::SOLANA_MAINNET);
                return;
            }
        };

        self.endpoints.push(BamEndpoint {
            provider,
            url: bam_url,
            rpc_url,
            uuid: None,
            region,
            healthy: true,
            avg_latency_ms: 0,
            probes_sent: 0,
            probes_ok: 0,
            last_healthy: None,
            supports_send_bundle: true,
            supports_send_private_tx: true,
        });
    }

    /// Add a custom BAM endpoint with explicit URLs.
    pub fn add_custom(&mut self, bam_url: &str, rpc_url: &str) {
        self.endpoints.push(BamEndpoint {
            provider: BamProvider::Custom,
            url: bam_url.to_string(),
            rpc_url: rpc_url.to_string(),
            uuid: None,
            region: Region::Auto,
            healthy: true,
            avg_latency_ms: 0,
            probes_sent: 0,
            probes_ok: 0,
            last_healthy: None,
            supports_send_bundle: true,
            supports_send_private_tx: true,
        });
    }

    // ── Selection ───────────────────────────────────────────────────────

    /// Select the best available endpoint.
    ///
    /// Selection priority:
    /// 1. Healthy endpoints only (unless none are healthy)
    /// 2. Preferred region match (if set)
    /// 3. Lowest average latency
    /// 4. Highest uptime ratio
    pub fn best_endpoint(&self) -> &BamEndpoint {
        assert!(!self.endpoints.is_empty(), "no endpoints registered");

        let healthy: Vec<&BamEndpoint> = self
            .endpoints
            .iter()
            .filter(|e| e.healthy)
            .collect();

        let candidates = if healthy.is_empty() {
            // All unhealthy → fall back to the full list
            warn!("no healthy endpoints — falling back to all");
            self.endpoints.iter().collect::<Vec<_>>()
        } else {
            healthy
        };

        // Prefer region match
        let region_match: Vec<&&BamEndpoint> = candidates
            .iter()
            .filter(|e| {
                self.preferred_region == Region::Auto
                    || e.region == self.preferred_region
                    || e.region == Region::Auto
            })
            .collect();

        let pool = if region_match.is_empty() {
            &candidates
        } else {
            // Flatten &&BamEndpoint → &BamEndpoint
            &region_match.iter().map(|e| **e).collect::<Vec<_>>()
        };

        // Sort by: lowest latency, then highest uptime
        pool.iter()
            .min_by(|a, b| {
                a.avg_latency_ms
                    .cmp(&b.avg_latency_ms)
                    .then_with(|| {
                        b.uptime()
                            .partial_cmp(&a.uptime())
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            })
            .expect("at least one endpoint")
    }

    /// Get all endpoints (for status display).
    pub fn endpoints(&self) -> &[BamEndpoint] {
        &self.endpoints
    }

    /// Get endpoints for a specific provider.
    pub fn endpoints_for(&self, provider: BamProvider) -> Vec<&BamEndpoint> {
        self.endpoints.iter().filter(|e| e.provider == provider).collect()
    }

    /// Number of registered endpoints.
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// Number of currently healthy endpoints.
    pub fn healthy_count(&self) -> usize {
        self.endpoints.iter().filter(|e| e.healthy).count()
    }

    // ── Health probing ──────────────────────────────────────────────────

    /// Record a latency probe result for an endpoint.
    pub fn record_probe(&mut self, url: &str, latency_ms: u64, success: bool) {
        if let Some(ep) = self.endpoints.iter_mut().find(|e| e.url == url) {
            ep.probes_sent += 1;
            if success {
                ep.probes_ok += 1;
                ep.last_healthy = Some(Instant::now());
                ep.healthy = true;
            } else if ep.probes_sent > 3 && ep.uptime() < 0.5 {
                // Mark unhealthy if more than half of recent probes failed
                ep.healthy = false;
                warn!(url, uptime = ep.uptime(), "endpoint marked unhealthy");
            }

            // Update rolling latency average
            let samples = self
                .latency_history
                .entry(url.to_string())
                .or_insert_with(Vec::new);

            if samples.len() >= self.max_latency_samples {
                samples.remove(0);
            }
            samples.push(latency_ms);

            ep.avg_latency_ms =
                samples.iter().sum::<u64>() / samples.len() as u64;
        }
    }

    /// Mark an endpoint as unhealthy (e.g., after repeated bundle rejections).
    pub fn mark_unhealthy(&mut self, url: &str) {
        if let Some(ep) = self.endpoints.iter_mut().find(|e| e.url == url) {
            ep.healthy = false;
            warn!(url, provider = %ep.provider, "endpoint force-marked unhealthy");
        }
    }

    /// Mark an endpoint as healthy (e.g., after a successful recovery probe).
    pub fn mark_healthy(&mut self, url: &str) {
        if let Some(ep) = self.endpoints.iter_mut().find(|e| e.url == url) {
            ep.healthy = true;
            ep.last_healthy = Some(Instant::now());
        }
    }

    /// Get a summary for logging/monitoring.
    pub fn summary(&self) -> RegistrySummary {
        RegistrySummary {
            total: self.endpoints.len(),
            healthy: self.healthy_count(),
            providers: self
                .endpoints
                .iter()
                .map(|e| e.provider)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            best_url: if self.endpoints.is_empty() {
                "none".to_string()
            } else {
                self.best_endpoint().url.clone()
            },
            best_latency_ms: if self.endpoints.is_empty() {
                0
            } else {
                self.best_endpoint().avg_latency_ms
            },
        }
    }
}

impl Default for BamEndpointRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Registry health summary for logging.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistrySummary {
    pub total: usize,
    pub healthy: usize,
    pub providers: usize,
    pub best_url: String,
    pub best_latency_ms: u64,
}

// ═══════════════════════════════════════════════════════════════════════════
//  Engine config builder from registry
// ═══════════════════════════════════════════════════════════════════════════

/// Build an `EngineConfig` from the best available BAM endpoint.
///
/// This is the bridge between the provider registry and the
/// `PrivateExecutionEngine`.
pub fn engine_config_from_registry(
    registry: &BamEndpointRegistry,
) -> crate::engine::EngineConfig {
    let best = registry.best_endpoint();
    crate::engine::EngineConfig {
        jito_url: best.url.clone(),
        uuid: best.uuid.clone(),
        rpc_url: best.rpc_url.clone(),
        ..Default::default()
    }
}

/// Build a `PrivateExecutionEngine` directly from the registry.
pub fn engine_from_registry(
    registry: &BamEndpointRegistry,
) -> crate::engine::PrivateExecutionEngine {
    let config = engine_config_from_registry(registry);
    info!(
        url = %config.jito_url,
        rpc = %config.rpc_url,
        "creating engine from registry best endpoint"
    );
    crate::engine::PrivateExecutionEngine::with_config(config)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_resolution() {
        let url = templates::resolve(templates::HELIUS_BAM_TEMPLATE, "MY_KEY_123");
        assert_eq!(url, "https://mainnet.bam.helius.xyz/?api-key=MY_KEY_123");
    }

    #[test]
    fn template_triton() {
        let url = templates::resolve(templates::TRITON_BAM_TEMPLATE, "abc");
        assert_eq!(url, "https://mainnet.bam.triton.one/abc");
    }

    #[test]
    fn template_quicknode() {
        let url = templates::resolve(templates::QUICKNODE_BAM_TEMPLATE, "my-slug");
        assert_eq!(url, "https://bam.my-slug.quiknode.pro");
    }

    #[test]
    fn empty_registry_panics_on_best() {
        let reg = BamEndpointRegistry::new();
        let result = std::panic::catch_unwind(|| reg.best_endpoint());
        assert!(result.is_err());
    }

    #[test]
    fn add_jito_native_populates_6_endpoints() {
        let mut reg = BamEndpointRegistry::new();
        reg.add_jito_native(None);
        assert_eq!(reg.len(), 6);
        assert!(reg.endpoints().iter().all(|e| e.provider == BamProvider::JitoNative));
    }

    #[test]
    fn add_custom_endpoint() {
        let mut reg = BamEndpointRegistry::new();
        reg.add_custom("https://my-bam.example.com", "https://my-rpc.example.com");
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.endpoints()[0].provider, BamProvider::Custom);
        assert_eq!(reg.endpoints()[0].url, "https://my-bam.example.com");
        assert_eq!(reg.endpoints()[0].rpc_url, "https://my-rpc.example.com");
    }

    #[test]
    fn add_provider_with_key() {
        let mut reg = BamEndpointRegistry::new();
        reg.add_with_key(BamProvider::Helius, "test-key-123");
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.endpoints()[0].provider, BamProvider::Helius);
        assert!(reg.endpoints()[0].url.contains("test-key-123"));
        assert!(reg.endpoints()[0].rpc_url.contains("test-key-123"));
    }

    #[test]
    fn best_endpoint_prefers_lowest_latency() {
        let mut reg = BamEndpointRegistry::new();
        reg.add_custom("https://slow.example.com", "https://rpc.com");
        reg.add_custom("https://fast.example.com", "https://rpc.com");

        reg.record_probe("https://slow.example.com", 200, true);
        reg.record_probe("https://fast.example.com", 50, true);

        let best = reg.best_endpoint();
        assert_eq!(best.url, "https://fast.example.com");
    }

    #[test]
    fn unhealthy_endpoints_deprioritized() {
        let mut reg = BamEndpointRegistry::new();
        reg.add_custom("https://bad.example.com", "https://rpc.com");
        reg.add_custom("https://good.example.com", "https://rpc.com");

        // Bad: fast but unhealthy
        reg.record_probe("https://bad.example.com", 10, true);
        reg.mark_unhealthy("https://bad.example.com");

        // Good: slower but healthy
        reg.record_probe("https://good.example.com", 100, true);

        let best = reg.best_endpoint();
        assert_eq!(best.url, "https://good.example.com");
    }

    #[test]
    fn record_probe_updates_latency() {
        let mut reg = BamEndpointRegistry::new();
        reg.add_custom("https://test.com", "https://rpc.com");

        reg.record_probe("https://test.com", 100, true);
        reg.record_probe("https://test.com", 200, true);

        let ep = &reg.endpoints()[0];
        assert_eq!(ep.avg_latency_ms, 150);
        assert_eq!(ep.probes_sent, 2);
        assert_eq!(ep.probes_ok, 2);
    }

    #[test]
    fn uptime_calculation() {
        let mut reg = BamEndpointRegistry::new();
        reg.add_custom("https://test.com", "https://rpc.com");

        reg.record_probe("https://test.com", 50, true);
        reg.record_probe("https://test.com", 50, true);
        reg.record_probe("https://test.com", 50, false);
        reg.record_probe("https://test.com", 50, true);

        let ep = &reg.endpoints()[0];
        assert_eq!(ep.probes_sent, 4);
        assert_eq!(ep.probes_ok, 3);
        assert!((ep.uptime() - 0.75).abs() < 0.01);
    }

    #[test]
    fn region_selection_prefers_match() {
        let mut reg = BamEndpointRegistry::with_region(Region::Asia);
        reg.add_jito_native(None);

        // Give them all some latency data
        let urls: Vec<String> = reg.endpoints.iter().map(|e| e.url.clone()).collect();
        for url in &urls {
            // All same latency so only region matters
            reg.record_probe(url, 50, true);
        }

        let best = reg.best_endpoint();
        assert!(
            best.url.contains("tokyo") || best.region == Region::Asia || best.region == Region::Auto,
            "should prefer Asia/Auto: got {} ({})",
            best.url,
            best.region
        );
    }

    #[test]
    fn healthy_count_tracks_correctly() {
        let mut reg = BamEndpointRegistry::new();
        reg.add_custom("https://a.com", "https://rpc.com");
        reg.add_custom("https://b.com", "https://rpc.com");

        assert_eq!(reg.healthy_count(), 2);

        reg.mark_unhealthy("https://a.com");
        assert_eq!(reg.healthy_count(), 1);

        reg.mark_healthy("https://a.com");
        assert_eq!(reg.healthy_count(), 2);
    }

    #[test]
    fn summary_works() {
        let mut reg = BamEndpointRegistry::new();
        reg.add_custom("https://a.com", "https://rpc.com");

        let summary = reg.summary();
        assert_eq!(summary.total, 1);
        assert_eq!(summary.healthy, 1);
        assert_eq!(summary.best_url, "https://a.com");
    }

    #[test]
    fn provider_display() {
        assert_eq!(format!("{}", BamProvider::JitoNative), "Jito Native");
        assert_eq!(format!("{}", BamProvider::Helius), "Helius BAM");
        assert_eq!(format!("{}", BamProvider::Triton), "Triton BAM");
        assert_eq!(format!("{}", BamProvider::QuickNode), "QuickNode BAM");
        assert_eq!(format!("{}", BamProvider::Custom), "Custom BAM");
    }

    #[test]
    fn env_var_missing_returns_false() {
        let mut reg = BamEndpointRegistry::new();
        let added = reg.add_from_env(
            BamProvider::Helius,
            "NONEXISTENT_ENV_VAR_12345_FOR_TEST",
        );
        assert!(!added);
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn engine_config_from_registry_works() {
        let mut reg = BamEndpointRegistry::new();
        reg.add_custom("https://bam.test.com", "https://rpc.test.com");

        let config = engine_config_from_registry(&reg);
        assert_eq!(config.jito_url, "https://bam.test.com");
        assert_eq!(config.rpc_url, "https://rpc.test.com");
    }

    #[test]
    fn endpoints_for_filters_correctly() {
        let mut reg = BamEndpointRegistry::new();
        reg.add_with_key(BamProvider::Helius, "key1");
        reg.add_custom("https://custom.com", "https://rpc.com");

        assert_eq!(reg.endpoints_for(BamProvider::Helius).len(), 1);
        assert_eq!(reg.endpoints_for(BamProvider::Custom).len(), 1);
        assert_eq!(reg.endpoints_for(BamProvider::Triton).len(), 0);
    }
}
