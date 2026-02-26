/// Private Execution Engine — TEE-routed bundle submission via Jito BAM.
///
/// This module wraps [`jito_sdk_rust::JitoJsonRpcSDK`] with a high-level,
/// type-safe interface that integrates with the rest of the crate (tip
/// construction, versioned-transaction building, bundle status types).
///
/// # Architecture
///
/// ```text
///   Your Bot
///     │
///     ▼
///  ┌────────────────────────────┐
///  │  PrivateExecutionEngine    │   ← this module
///  │  ┌──────────┐ ┌─────────┐ │
///  │  │ JitoJson │ │ Solana  │ │
///  │  │ RpcSDK   │ │ RpcClient│ │
///  │  └────┬─────┘ └────┬────┘ │
///  └───────┼────────────┼──────┘
///          │            │
///          ▼            ▼
///    Jito Block      Solana
///    Engine (TEE)    RPC Node
/// ```
///
/// # Security model
///
/// The Jito block-engine endpoint runs inside a Trusted Execution
/// Environment (Intel TDX).  Transactions submitted via `send_bundle()`
/// or `send_transaction()` travel over HTTPS directly to the TEE —
/// transaction contents are **never** visible to the node operator or
/// any intermediary.
///
/// # Quick start
///
/// ```rust,no_run
/// use jito_bam_client::engine::PrivateExecutionEngine;
/// use solana_sdk::{signature::Keypair, signer::Signer, system_instruction};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let engine = PrivateExecutionEngine::mainnet(
///         "https://api.mainnet-beta.solana.com",
///     );
///
///     let payer = Keypair::new();
///     let swap_ix = system_instruction::transfer(
///         &payer.pubkey(),
///         &solana_sdk::pubkey::Pubkey::new_unique(),
///         1_000_000,
///     );
///
///     let bundle_uuid = engine.execute(
///         &[swap_ix],
///         &payer,
///         50_000,   // tip lamports
///     ).await?;
///
///     println!("Bundle submitted: {bundle_uuid}");
///     Ok(())
/// }
/// ```
use std::str::FromStr;
use std::time::Duration;

use base64::{engine::general_purpose, Engine as _};
use jito_sdk_rust::JitoJsonRpcSDK;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    hash::Hash,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::VersionedTransaction,
};
use tracing::{debug, info, instrument, warn};

use crate::error::{JitoBamError, Result};
use crate::tx_builder::TxBuildConfig;

// ─── Well-known endpoints ───────────────────────────────────────────────────

/// Jito block-engine API endpoints.
pub mod endpoints {
    /// Mainnet block-engine (primary).
    pub const MAINNET: &str = "https://mainnet.block-engine.jito.wtf/api/v1";
    /// Amsterdam region.
    pub const AMSTERDAM: &str = "https://amsterdam.mainnet.block-engine.jito.wtf/api/v1";
    /// Frankfurt region.
    pub const FRANKFURT: &str = "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1";
    /// New York region.
    pub const NY: &str = "https://ny.mainnet.block-engine.jito.wtf/api/v1";
    /// Tokyo region.
    pub const TOKYO: &str = "https://tokyo.mainnet.block-engine.jito.wtf/api/v1";
    /// Salt Lake City region.
    pub const SLC: &str = "https://slc.mainnet.block-engine.jito.wtf/api/v1";
}

// ─── Bundle confirmation ────────────────────────────────────────────────────

/// Confirmation status returned from bundle status polling.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleConfirmation {
    /// The bundle UUID returned by the block-engine.
    pub bundle_uuid: String,
    /// Final confirmation status: `"confirmed"`, `"finalized"`, etc.
    pub status: String,
    /// Transaction signatures included in the landed bundle.
    pub transactions: Vec<String>,
    /// Slot the bundle landed in (if available).
    pub slot: Option<u64>,
    /// Any error from the bundle execution.
    pub error: Option<String>,
}

/// In-flight status of a bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InFlightStatus {
    /// Bundle is still queued / being auctioned.
    Pending,
    /// Bundle has landed on-chain.
    Landed,
    /// Bundle failed (invalid, timed out, etc.).
    Failed,
    /// Block-engine returned an unexpected status.
    Unknown(String),
}

impl std::fmt::Display for InFlightStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "Pending"),
            Self::Landed => write!(f, "Landed"),
            Self::Failed => write!(f, "Failed"),
            Self::Unknown(s) => write!(f, "Unknown({s})"),
        }
    }
}

// ─── Engine configuration ───────────────────────────────────────────────────

/// Configuration for the Private Execution Engine.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Jito block-engine URL (default: mainnet).
    pub jito_url: String,
    /// Optional UUID for authenticated Jito access.
    pub uuid: Option<String>,
    /// Solana RPC URL for blockhash fetch / confirmations.
    pub rpc_url: String,
    /// Maximum polls when confirming a bundle status.
    pub max_status_polls: u32,
    /// Delay between status polls.
    pub poll_interval: Duration,
    /// Default tip amount in lamports.
    pub default_tip_lamports: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            jito_url: endpoints::MAINNET.to_string(),
            uuid: None,
            rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
            max_status_polls: 30,
            poll_interval: Duration::from_secs(2),
            default_tip_lamports: 50_000,
        }
    }
}

// ─── Private Execution Engine ───────────────────────────────────────────────

/// The Private Execution Engine routes transactions through the Jito
/// block-engine's Trusted Execution Environment (TEE), completely
/// bypassing the public mempool.
///
/// It wraps two underlying clients:
/// - [`JitoJsonRpcSDK`] — for bundle/tx submission and status queries.
/// - [`RpcClient`] — for blockhash fetch and on-chain confirmations.
pub struct PrivateExecutionEngine {
    jito: JitoJsonRpcSDK,
    rpc: RpcClient,
    config: EngineConfig,
}

impl PrivateExecutionEngine {
    // ── Constructors ────────────────────────────────────────────────────

    /// Create an engine connected to **mainnet** defaults.
    ///
    /// ```rust,no_run
    /// # use jito_bam_client::engine::PrivateExecutionEngine;
    /// let engine = PrivateExecutionEngine::mainnet(
    ///     "https://api.mainnet-beta.solana.com",
    /// );
    /// ```
    pub fn mainnet(rpc_url: &str) -> Self {
        Self::new(endpoints::MAINNET, None, rpc_url)
    }

    /// Create an engine with explicit endpoints.
    pub fn new(jito_url: &str, uuid: Option<String>, rpc_url: &str) -> Self {
        let jito = JitoJsonRpcSDK::new(jito_url, uuid.clone());
        let rpc = RpcClient::new(rpc_url.to_string());
        Self {
            jito,
            rpc,
            config: EngineConfig {
                jito_url: jito_url.to_string(),
                uuid,
                rpc_url: rpc_url.to_string(),
                ..Default::default()
            },
        }
    }

    /// Create an engine with full configuration.
    pub fn with_config(config: EngineConfig) -> Self {
        let jito = JitoJsonRpcSDK::new(&config.jito_url, config.uuid.clone());
        let rpc = RpcClient::new(config.rpc_url.clone());
        Self { jito, rpc, config }
    }

    // ── Tip accounts (TEE-attested, dynamically rotated) ────────────────

    /// Fetch the current tip accounts from the Jito block-engine.
    ///
    /// These accounts are TEE-attested and rotate periodically.
    /// Use them instead of hardcoded tip accounts for maximum reliability.
    #[instrument(skip_all)]
    pub async fn get_tip_accounts(&self) -> Result<Vec<Pubkey>> {
        let response = self
            .jito
            .get_tip_accounts()
            .await
            .map_err(|e| JitoBamError::RpcError(format!("get_tip_accounts: {e}")))?;

        let accounts = response["result"]
            .as_array()
            .ok_or_else(|| {
                JitoBamError::RpcError("tip accounts response missing 'result' array".into())
            })?;

        accounts
            .iter()
            .map(|v| {
                let s = v.as_str().ok_or_else(|| {
                    JitoBamError::RpcError("tip account is not a string".into())
                })?;
                Pubkey::from_str(s).map_err(|e| {
                    JitoBamError::RpcError(format!("invalid tip account pubkey: {e}"))
                })
            })
            .collect()
    }

    /// Get a single random tip account from the block-engine.
    #[instrument(skip_all)]
    pub async fn get_random_tip_account(&self) -> Result<Pubkey> {
        let account_str = self
            .jito
            .get_random_tip_account()
            .await
            .map_err(|e| JitoBamError::RpcError(format!("get_random_tip_account: {e}")))?;

        Pubkey::from_str(&account_str)
            .map_err(|e| JitoBamError::RpcError(format!("invalid tip account pubkey: {e}")))
    }

    // ── Blockhash ───────────────────────────────────────────────────────

    /// Fetch the latest blockhash from the Solana RPC node.
    #[instrument(skip_all)]
    pub fn get_latest_blockhash(&self) -> Result<Hash> {
        self.rpc
            .get_latest_blockhash()
            .map_err(|e| JitoBamError::BlockhashFetch(e.to_string()))
    }

    // ── Bundle submission ───────────────────────────────────────────────

    /// Submit a bundle of 1–5 signed `VersionedTransaction`s privately
    /// through the Jito TEE.
    ///
    /// Returns the bundle UUID for status polling.
    ///
    /// The transactions are serialized with `bincode`, base64-encoded,
    /// and sent to the block-engine's `sendBundle` JSON-RPC method.
    #[instrument(skip_all, fields(bundle_size = transactions.len()))]
    pub async fn send_bundle(
        &self,
        transactions: Vec<VersionedTransaction>,
    ) -> Result<String> {
        if transactions.is_empty() {
            return Err(JitoBamError::BundleTooLarge { count: 0, max: 5 });
        }
        if transactions.len() > 5 {
            return Err(JitoBamError::BundleTooLarge {
                count: transactions.len(),
                max: 5,
            });
        }

        // Serialize each transaction → bincode → base64
        let encoded_txs: Vec<Value> = transactions
            .iter()
            .map(|tx| {
                let bytes = bincode::serialize(tx)
                    .map_err(|e| JitoBamError::Serialization(e.to_string()))?;
                Ok(Value::String(general_purpose::STANDARD.encode(bytes)))
            })
            .collect::<Result<Vec<_>>>()?;

        let params = json!([
            encoded_txs,
            { "encoding": "base64" }
        ]);

        info!(
            bundle_size = transactions.len(),
            "submitting bundle to Jito block-engine (TEE)"
        );

        let response = self
            .jito
            .send_bundle(Some(params), self.config.uuid.as_deref())
            .await
            .map_err(|e| JitoBamError::BundleRejected(e.to_string()))?;

        // Check for JSON-RPC error
        if let Some(err) = response.get("error") {
            return Err(JitoBamError::BundleRejected(format!(
                "block-engine error: {}",
                err
            )));
        }

        let bundle_uuid = response["result"]
            .as_str()
            .ok_or_else(|| {
                JitoBamError::BundleRejected(
                    "missing 'result' (bundle UUID) in response".into(),
                )
            })?
            .to_string();

        info!(bundle_uuid = %bundle_uuid, "bundle accepted by block-engine");
        Ok(bundle_uuid)
    }

    // ── Single transaction submission ───────────────────────────────────

    /// Submit a single signed `VersionedTransaction` privately.
    ///
    /// * `bundle_only = true`  → tx is only submitted as a Jito bundle
    ///   (guaranteed private, no public mempool fallback).
    /// * `bundle_only = false` → tx may also be forwarded to the leader
    ///   directly (faster but slightly less private).
    ///
    /// Returns the transaction signature.
    #[instrument(skip_all, fields(bundle_only))]
    pub async fn send_transaction(
        &self,
        tx: &VersionedTransaction,
        bundle_only: bool,
    ) -> Result<String> {
        let bytes = bincode::serialize(tx)
            .map_err(|e| JitoBamError::Serialization(e.to_string()))?;
        let encoded = general_purpose::STANDARD.encode(bytes);

        let params = json!({ "tx": encoded });

        info!(bundle_only, "submitting private transaction");

        let response = self
            .jito
            .send_txn(Some(params), bundle_only)
            .await
            .map_err(|e| JitoBamError::RpcError(format!("send_txn: {e}")))?;

        if let Some(err) = response.get("error") {
            return Err(JitoBamError::RpcError(format!(
                "send_txn error: {err}"
            )));
        }

        let signature = response["result"]
            .as_str()
            .ok_or_else(|| {
                JitoBamError::RpcError("missing 'result' (signature) in response".into())
            })?
            .to_string();

        info!(signature = %signature, "transaction accepted");
        Ok(signature)
    }

    // ── Bundle status polling ───────────────────────────────────────────

    /// Poll the in-flight status of a bundle until it lands or fails.
    ///
    /// Uses the block-engine's `getInflightBundleStatuses` and
    /// `getBundleStatuses` JSON-RPC methods.
    #[instrument(skip_all, fields(bundle_uuid = %bundle_uuid))]
    pub async fn poll_bundle_status(
        &self,
        bundle_uuid: &str,
    ) -> Result<BundleConfirmation> {
        let max_polls = self.config.max_status_polls;
        let interval = self.config.poll_interval;

        // Phase 1: poll in-flight status until Landed or Failed
        for attempt in 1..=max_polls {
            debug!(attempt, max_polls, "polling in-flight bundle status");

            let status_response = self
                .jito
                .get_in_flight_bundle_statuses(vec![bundle_uuid.to_string()])
                .await
                .map_err(|e| JitoBamError::RpcError(format!("in-flight status: {e}")))?;

            let in_flight = Self::parse_in_flight_status(&status_response);

            match in_flight {
                InFlightStatus::Landed => {
                    info!(bundle_uuid, "bundle landed — checking final status");
                    return self.poll_final_status(bundle_uuid).await;
                }
                InFlightStatus::Failed => {
                    return Err(JitoBamError::BundleRejected(
                        "block-engine reported bundle Failed".into(),
                    ));
                }
                InFlightStatus::Pending => {
                    debug!(bundle_uuid, "bundle pending");
                }
                InFlightStatus::Unknown(ref s) => {
                    warn!(bundle_uuid, status = %s, "unexpected in-flight status");
                }
            }

            if attempt < max_polls {
                tokio::time::sleep(interval).await;
            }
        }

        Err(JitoBamError::BundleNotLanded {
            attempts: max_polls,
        })
    }

    /// Phase 2: poll `getBundleStatuses` for finalized confirmation.
    async fn poll_final_status(
        &self,
        bundle_uuid: &str,
    ) -> Result<BundleConfirmation> {
        let max_final_polls = 10;
        let interval = self.config.poll_interval;

        for attempt in 1..=max_final_polls {
            debug!(attempt, max_final_polls, "polling final bundle status");

            let status_response = self
                .jito
                .get_bundle_statuses(vec![bundle_uuid.to_string()])
                .await
                .map_err(|e| JitoBamError::RpcError(format!("bundle status: {e}")))?;

            if let Some(confirmation) =
                Self::parse_bundle_confirmation(bundle_uuid, &status_response)
            {
                match confirmation.status.as_str() {
                    "finalized" => {
                        info!(
                            bundle_uuid,
                            txs = confirmation.transactions.len(),
                            "bundle finalized on-chain"
                        );
                        return Ok(confirmation);
                    }
                    "confirmed" => {
                        info!(bundle_uuid, "bundle confirmed, waiting for finalization");
                    }
                    other => {
                        warn!(bundle_uuid, status = other, "unexpected final status");
                    }
                }
            }

            if attempt < max_final_polls {
                tokio::time::sleep(interval).await;
            }
        }

        Err(JitoBamError::BundleNotLanded {
            attempts: max_final_polls,
        })
    }

    // ── High-level execute: build + tip + sign + submit ─────────────────

    /// **Primary bot entry-point.**
    ///
    /// Takes your application instructions, fetches a fresh blockhash,
    /// retrieves a TEE-attested tip account, builds a tipped versioned
    /// transaction, signs it, and submits it privately as a Jito bundle.
    ///
    /// Returns the bundle UUID.
    ///
    /// ```rust,no_run
    /// # use jito_bam_client::engine::PrivateExecutionEngine;
    /// # use solana_sdk::{signature::Keypair, signer::Signer, system_instruction, pubkey::Pubkey};
    /// # async fn run() -> anyhow::Result<()> {
    /// let engine = PrivateExecutionEngine::mainnet("https://api.mainnet-beta.solana.com");
    /// let payer = Keypair::new();
    ///
    /// let swap_ix = system_instruction::transfer(
    ///     &payer.pubkey(),
    ///     &Pubkey::new_unique(),
    ///     1_000_000,
    /// );
    ///
    /// let bundle_uuid = engine.execute(&[swap_ix], &payer, 50_000).await?;
    /// println!("Submitted bundle: {bundle_uuid}");
    /// # Ok(())
    /// # }
    /// ```
    #[instrument(skip_all, fields(
        n_ixs = instructions.len(),
        tip = tip_lamports,
        payer = %payer.pubkey()
    ))]
    pub async fn execute(
        &self,
        instructions: &[Instruction],
        payer: &Keypair,
        tip_lamports: u64,
    ) -> Result<String> {
        self.execute_with_signers(instructions, payer, &[], tip_lamports)
            .await
    }

    /// Like [`execute`](Self::execute) but with additional signers.
    pub async fn execute_with_signers(
        &self,
        instructions: &[Instruction],
        payer: &Keypair,
        extra_signers: &[&Keypair],
        tip_lamports: u64,
    ) -> Result<String> {
        // 1. Fetch latest blockhash from Solana RPC
        let blockhash = self.get_latest_blockhash()?;
        debug!(blockhash = %blockhash, "fetched latest blockhash");

        // 2. Get a TEE-attested tip account from the block-engine
        let tip_account = self.get_random_tip_account().await?;
        debug!(tip_account = %tip_account, "selected tip account");

        // 3. Build the tipped versioned transaction
        let config = TxBuildConfig {
            tip_lamports,
            tip_account: Some(tip_account),
            ..Default::default()
        };

        let tx = crate::tx_builder::build_tipped_versioned_transaction(
            instructions,
            payer,
            extra_signers,
            blockhash,
            &config,
        )?;

        // 4. Submit as a 1-tx bundle through the TEE
        self.send_bundle(vec![tx]).await
    }

    /// Execute instructions and wait for on-chain confirmation.
    ///
    /// Combines [`execute`](Self::execute) with [`poll_bundle_status`](Self::poll_bundle_status).
    #[instrument(skip_all, fields(tip = tip_lamports))]
    pub async fn execute_and_confirm(
        &self,
        instructions: &[Instruction],
        payer: &Keypair,
        tip_lamports: u64,
    ) -> Result<BundleConfirmation> {
        let bundle_uuid = self.execute(instructions, payer, tip_lamports).await?;
        self.poll_bundle_status(&bundle_uuid).await
    }

    // ── JSON response parsers ───────────────────────────────────────────

    fn parse_in_flight_status(response: &Value) -> InFlightStatus {
        let status_str = response
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|entry| entry.get("status"))
            .and_then(|s| s.as_str());

        match status_str {
            Some("Landed") => InFlightStatus::Landed,
            Some("Pending") => InFlightStatus::Pending,
            Some("Failed") | Some("Invalid") => InFlightStatus::Failed,
            Some(other) => InFlightStatus::Unknown(other.to_string()),
            None => InFlightStatus::Pending, // treat missing as pending
        }
    }

    fn parse_bundle_confirmation(
        bundle_uuid: &str,
        response: &Value,
    ) -> Option<BundleConfirmation> {
        let entry = response
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())?;

        let status = entry
            .get("confirmation_status")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string();

        let transactions = entry
            .get("transactions")
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let slot = entry.get("slot").and_then(|s| s.as_u64());

        let error = entry.get("err").and_then(|e| {
            if e.is_null() || e.get("Ok").is_some() {
                None
            } else {
                Some(e.to_string())
            }
        });

        Some(BundleConfirmation {
            bundle_uuid: bundle_uuid.to_string(),
            status,
            transactions,
            slot,
            error,
        })
    }

    // ── Accessors ───────────────────────────────────────────────────────

    /// Reference to the underlying Jito JSON-RPC SDK.
    pub fn jito_sdk(&self) -> &JitoJsonRpcSDK {
        &self.jito
    }

    /// Reference to the Solana RPC client.
    pub fn rpc_client(&self) -> &RpcClient {
        &self.rpc
    }

    /// Engine configuration.
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_config_defaults() {
        let cfg = EngineConfig::default();
        assert_eq!(cfg.jito_url, endpoints::MAINNET);
        assert!(cfg.uuid.is_none());
        assert_eq!(cfg.max_status_polls, 30);
        assert_eq!(cfg.default_tip_lamports, 50_000);
    }

    #[test]
    fn mainnet_constructor() {
        let engine = PrivateExecutionEngine::mainnet("https://api.mainnet-beta.solana.com");
        assert_eq!(engine.config().jito_url, endpoints::MAINNET);
        assert_eq!(engine.config().rpc_url, "https://api.mainnet-beta.solana.com");
    }

    #[test]
    fn custom_constructor() {
        let engine = PrivateExecutionEngine::new(
            endpoints::TOKYO,
            Some("my-uuid".to_string()),
            "https://myrpc.example.com",
        );
        assert_eq!(engine.config().jito_url, endpoints::TOKYO);
        assert_eq!(engine.config().uuid.as_deref(), Some("my-uuid"));
        assert_eq!(engine.config().rpc_url, "https://myrpc.example.com");
    }

    #[test]
    fn with_config_constructor() {
        let config = EngineConfig {
            jito_url: endpoints::FRANKFURT.to_string(),
            uuid: None,
            rpc_url: "https://rpc.example.com".to_string(),
            max_status_polls: 10,
            poll_interval: Duration::from_millis(500),
            default_tip_lamports: 100_000,
        };
        let engine = PrivateExecutionEngine::with_config(config.clone());
        assert_eq!(engine.config().max_status_polls, 10);
        assert_eq!(engine.config().default_tip_lamports, 100_000);
    }

    #[test]
    fn parse_in_flight_landed() {
        let response = json!({
            "result": {
                "value": [
                    { "bundle_id": "abc", "status": "Landed" }
                ]
            }
        });
        assert_eq!(
            PrivateExecutionEngine::parse_in_flight_status(&response),
            InFlightStatus::Landed
        );
    }

    #[test]
    fn parse_in_flight_pending() {
        let response = json!({
            "result": {
                "value": [
                    { "bundle_id": "abc", "status": "Pending" }
                ]
            }
        });
        assert_eq!(
            PrivateExecutionEngine::parse_in_flight_status(&response),
            InFlightStatus::Pending
        );
    }

    #[test]
    fn parse_in_flight_failed() {
        let response = json!({
            "result": {
                "value": [
                    { "bundle_id": "abc", "status": "Failed" }
                ]
            }
        });
        assert_eq!(
            PrivateExecutionEngine::parse_in_flight_status(&response),
            InFlightStatus::Failed
        );
    }

    #[test]
    fn parse_in_flight_invalid_is_failed() {
        let response = json!({
            "result": {
                "value": [
                    { "bundle_id": "abc", "status": "Invalid" }
                ]
            }
        });
        assert_eq!(
            PrivateExecutionEngine::parse_in_flight_status(&response),
            InFlightStatus::Failed
        );
    }

    #[test]
    fn parse_in_flight_missing_defaults_to_pending() {
        let response = json!({});
        assert_eq!(
            PrivateExecutionEngine::parse_in_flight_status(&response),
            InFlightStatus::Pending
        );
    }

    #[test]
    fn parse_bundle_confirmation_finalized() {
        let response = json!({
            "result": {
                "value": [{
                    "confirmation_status": "finalized",
                    "transactions": ["sig1abc", "sig2def"],
                    "slot": 12345678,
                    "err": { "Ok": null }
                }]
            }
        });
        let conf =
            PrivateExecutionEngine::parse_bundle_confirmation("uuid-123", &response)
                .expect("should parse");
        assert_eq!(conf.status, "finalized");
        assert_eq!(conf.bundle_uuid, "uuid-123");
        assert_eq!(conf.transactions, vec!["sig1abc", "sig2def"]);
        assert_eq!(conf.slot, Some(12345678));
        assert!(conf.error.is_none());
    }

    #[test]
    fn parse_bundle_confirmation_with_error() {
        let response = json!({
            "result": {
                "value": [{
                    "confirmation_status": "finalized",
                    "transactions": [],
                    "err": { "InstructionError": [0, "Custom(1)"] }
                }]
            }
        });
        let conf =
            PrivateExecutionEngine::parse_bundle_confirmation("uuid-456", &response)
                .expect("should parse");
        assert!(conf.error.is_some());
    }

    #[test]
    fn parse_bundle_confirmation_missing_returns_none() {
        let response = json!({ "result": { "value": [] } });
        assert!(
            PrivateExecutionEngine::parse_bundle_confirmation("uuid", &response).is_none()
        );
    }

    #[test]
    fn in_flight_status_display() {
        assert_eq!(format!("{}", InFlightStatus::Landed), "Landed");
        assert_eq!(format!("{}", InFlightStatus::Pending), "Pending");
        assert_eq!(format!("{}", InFlightStatus::Failed), "Failed");
        assert_eq!(
            format!("{}", InFlightStatus::Unknown("Foo".into())),
            "Unknown(Foo)"
        );
    }

    #[test]
    fn bundle_confirmation_serde_roundtrip() {
        let conf = BundleConfirmation {
            bundle_uuid: "abc-123".into(),
            status: "finalized".into(),
            transactions: vec!["sig1".into()],
            slot: Some(999),
            error: None,
        };
        let json_str = serde_json::to_string(&conf).unwrap();
        let back: BundleConfirmation = serde_json::from_str(&json_str).unwrap();
        assert_eq!(back.bundle_uuid, "abc-123");
        assert_eq!(back.status, "finalized");
        assert_eq!(back.slot, Some(999));
    }

    #[test]
    fn endpoints_are_correct() {
        assert!(endpoints::MAINNET.contains("mainnet.block-engine.jito.wtf"));
        assert!(endpoints::TOKYO.contains("tokyo"));
        assert!(endpoints::NY.contains("ny"));
        assert!(endpoints::AMSTERDAM.contains("amsterdam"));
        assert!(endpoints::FRANKFURT.contains("frankfurt"));
        assert!(endpoints::SLC.contains("slc"));
    }
}
