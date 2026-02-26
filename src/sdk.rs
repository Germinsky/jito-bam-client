/// High-level Jito BAM SDK facade.
///
/// This module re-exports everything an integrator needs and provides the
/// `JitoBamSdk` struct — a batteries-included entry point that mirrors the
/// conceptual pattern:
///
/// ```rust,no_run
/// # use jito_bam_client::sdk::JitoBamSdk;
/// # use solana_sdk::{instruction::Instruction, signature::Keypair, hash::Hash};
/// # #[tokio::main] async fn main() {
/// let sdk = JitoBamSdk::mainnet().await.unwrap();
/// // sdk.execute(&[swap_ix], &payer, 50_000, blockhash).await
/// # }
/// ```
///
/// It internally owns a [`BamClient`](crate::bam::BamClient) and applies
/// sensible defaults (tip amount, retry policy, bundle polling).
use std::time::Duration;

use solana_sdk::{
    hash::Hash,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::VersionedTransaction,
};
use tracing::{info, instrument};

use crate::bam::{self, BamClient, BamClientConfig};
use crate::bundle::{BundleResult, BundleStatus};
use crate::error::Result;
use crate::tip;
use crate::tx_builder::{self, TxBuildConfig};

/// Batteries-included SDK for Jito BAM private transaction submission.
pub struct JitoBamSdk {
    client: BamClient,
}

impl JitoBamSdk {
    // ── Constructors ────────────────────────────────────────────────────

    /// Connect to the **mainnet** BAM node with default settings.
    pub async fn mainnet() -> Result<Self> {
        Self::from_endpoint(bam::endpoints::MAINNET).await
    }

    /// Connect to a BAM node by URL.
    pub async fn from_endpoint(url: &str) -> Result<Self> {
        let client = BamClient::new(url).await?;
        Ok(Self { client })
    }

    /// Connect with full configuration.
    pub async fn with_config(config: BamClientConfig) -> Result<Self> {
        let client = BamClient::connect(config).await?;
        Ok(Self { client })
    }

    // ── Primary execution method ────────────────────────────────────────

    /// **The main bot entry-point.**
    ///
    /// Takes your application instructions (e.g. a swap), wraps them with
    /// a Jito tip as the final instruction, signs, and submits the bundle
    /// privately through BAM — completely bypassing the public mempool.
    ///
    /// # Arguments
    /// * `instructions`    – your swap / arb / liquidation instructions
    /// * `payer`           – fee-payer keypair (also tips)
    /// * `tip_lamports`    – tip amount in lamports for leader priority
    /// * `recent_blockhash` – a fresh blockhash
    ///
    /// # Returns
    /// A [`BundleResult`] with the bundle ID and per-tx signatures.
    ///
    /// # Example
    /// ```rust,no_run
    /// # use jito_bam_client::sdk::JitoBamSdk;
    /// # use solana_sdk::{system_instruction, signature::Keypair, signer::Signer, pubkey::Pubkey, hash::Hash};
    /// # async fn run() {
    /// let sdk = JitoBamSdk::mainnet().await.unwrap();
    /// let payer = Keypair::new();
    /// let swap_ix = system_instruction::transfer(
    ///     &payer.pubkey(),
    ///     &Pubkey::new_unique(),
    ///     1_000_000,
    /// );
    ///
    /// let result = sdk.execute(
    ///     &[swap_ix],
    ///     &payer,
    ///     50_000,                   // 0.00005 SOL tip
    ///     Hash::new_unique(),
    /// ).await.unwrap();
    ///
    /// println!("Bundle {} submitted", result.bundle_id);
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
        recent_blockhash: Hash,
    ) -> Result<BundleResult> {
        self.execute_with_signers(instructions, payer, &[], tip_lamports, recent_blockhash)
            .await
    }

    /// Like [`execute`](Self::execute) but accepts additional signers.
    pub async fn execute_with_signers(
        &self,
        instructions: &[Instruction],
        payer: &Keypair,
        extra_signers: &[&Keypair],
        tip_lamports: u64,
        recent_blockhash: Hash,
    ) -> Result<BundleResult> {
        let config = TxBuildConfig {
            tip_lamports,
            ..Default::default()
        };

        self.client
            .build_and_submit(instructions, payer, extra_signers, recent_blockhash, &config)
            .await
    }

    // ── Multi-transaction bundles ───────────────────────────────────────

    /// Submit a pre-built multi-transaction bundle.
    ///
    /// The tip should already be the last instruction of the last
    /// transaction.  Use [`tip::build_tip_instruction`] to construct it.
    pub async fn send_bundle(
        &self,
        transactions: Vec<VersionedTransaction>,
    ) -> Result<BundleResult> {
        self.client.send_bundle(transactions).await
    }

    // ── Bundle status ───────────────────────────────────────────────────

    /// Execute instructions **and** poll until the bundle lands or fails.
    ///
    /// Combines [`execute`](Self::execute) with automatic status polling.
    /// Returns the final `BundleStatus` alongside the submission result.
    #[instrument(skip_all, fields(tip = tip_lamports))]
    pub async fn execute_and_confirm(
        &self,
        instructions: &[Instruction],
        payer: &Keypair,
        tip_lamports: u64,
        recent_blockhash: Hash,
    ) -> Result<(BundleResult, BundleStatus)> {
        let result = self
            .execute(instructions, payer, tip_lamports, recent_blockhash)
            .await?;

        info!(bundle_id = %result.bundle_id, "polling for confirmation");

        let status = self
            .client
            .poll_bundle_status(
                &result.bundle_id,
                20,                           // max polls
                Duration::from_millis(500),   // poll interval
            )
            .await?;

        Ok((result, status))
    }

    /// Poll an existing bundle until it reaches a terminal state.
    pub async fn confirm_bundle(&self, bundle_id: &str) -> Result<BundleStatus> {
        self.client
            .poll_bundle_status(bundle_id, 20, Duration::from_millis(500))
            .await
    }

    // ── Helpers ─────────────────────────────────────────────────────────

    /// Build a Jito tip instruction for manual bundle construction.
    pub fn tip_instruction(
        payer: &Pubkey,
        tip_lamports: u64,
    ) -> Result<Instruction> {
        tip::build_tip_instruction(payer, tip_lamports, None)
    }

    /// Get a reference to the underlying BAM client for advanced use.
    pub fn client(&self) -> &BamClient {
        &self.client
    }
}
