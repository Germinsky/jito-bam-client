/// Example: Bot execution loop using Jito BAM private submission.
///
/// Demonstrates the complete flow of a MEV / trading bot:
///
///   1. Set up the Jito BAM SDK
///   2. Build a swap instruction (placeholder for Raydium / Orca / Jupiter)
///   3. Wrap it with a Jito tip as the final instruction
///   4. Route to BAM Node instead of standard RPC → bypasses public mempool
///   5. Poll for bundle confirmation
///   6. Loop
///
/// # Running
/// ```bash
/// cargo run --example bot_loop
/// ```
use std::time::Duration;
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::VersionedTransaction,
};
use tracing::{error, info, warn};

use jito_bam_client::bam::{self, BamClient, BamClientConfig};
use jito_bam_client::bundle::BundleStatus;
use jito_bam_client::sdk::JitoBamSdk;
use jito_bam_client::tip;
use jito_bam_client::tx_builder::TxBuildConfig;

// ─── Configuration ──────────────────────────────────────────────────────────

/// Tip amount in lamports.  50 000 = 0.00005 SOL — adjust based on
/// network congestion and your profit margin.
const TIP_LAMPORTS: u64 = 50_000;

/// How long to wait between execution loop iterations.
const LOOP_INTERVAL: Duration = Duration::from_millis(400);

/// Maximum consecutive errors before the bot pauses.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// Pause duration after too many consecutive errors.
const ERROR_PAUSE: Duration = Duration::from_secs(10);

// ─── Entry point ────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialise tracing
    tracing_subscriber::fmt()
        .with_env_filter("info,jito_bam_client=debug")
        .init();

    info!("=== Jito BAM Bot Starting ===");

    // ── 1. Set up the Jito-BAM SDK ──────────────────────────────────────
    //
    // Option A: Simple one-liner (recommended)
    let sdk = JitoBamSdk::mainnet().await?;
    //
    // Option B: Direct BamClient with custom config
    // let bam_client = BamClient::connect(BamClientConfig {
    //     endpoint: "https://mainnet.bam.jito.wtf".into(),
    //     max_retries: 5,
    //     timeout: Duration::from_secs(15),
    //     ..Default::default()
    // }).await?;
    //
    // Option C: The exact pattern from the spec
    // let bam_client = BamClient::new("https://mainnet.bam.jito.wtf").await?;

    info!("BAM SDK connected");

    // Load your keypair (in production, from a file or HSM)
    let payer = Keypair::new();
    info!(payer = %payer.pubkey(), "bot wallet loaded");

    // ── 2. Bot execution loop ───────────────────────────────────────────
    let mut consecutive_errors: u32 = 0;

    loop {
        // In a real bot, you'd fetch a fresh blockhash from your RPC node:
        //   let recent_blockhash = rpc_client.get_latest_blockhash().await?;
        let recent_blockhash = Hash::new_unique(); // placeholder

        // ── 2a. Detect opportunity (your strategy logic) ────────────────
        let opportunity = detect_opportunity().await;

        if opportunity.is_none() {
            tokio::time::sleep(LOOP_INTERVAL).await;
            continue;
        }

        let opp = opportunity.unwrap();
        info!(
            pair = %opp.pair,
            expected_profit = opp.expected_profit_lamports,
            "opportunity detected"
        );

        // ── 2b. Build swap instruction(s) ───────────────────────────────
        let swap_instruction = build_swap_instruction(&payer, &opp);

        // ── 2c. Submit privately via BAM ────────────────────────────────
        //
        // The SDK wraps the swap + Jito tip into a VersionedTransaction,
        // signs it, and routes it to the BAM Node instead of standard RPC.
        // The transaction NEVER enters the public mempool.

        match sdk.execute_and_confirm(
            &[swap_instruction],
            &payer,
            TIP_LAMPORTS,
            recent_blockhash,
        ).await {
            Ok((result, status)) => {
                consecutive_errors = 0;

                match status {
                    BundleStatus::Landed { slot } => {
                        info!(
                            bundle_id = %result.bundle_id,
                            slot,
                            signatures = ?result.signatures,
                            "bundle LANDED — transaction confirmed"
                        );
                    }
                    BundleStatus::Failed { slot, reason } => {
                        warn!(
                            bundle_id = %result.bundle_id,
                            slot, reason,
                            "bundle FAILED — will retry on next opportunity"
                        );
                    }
                    other => {
                        warn!(
                            bundle_id = %result.bundle_id,
                            status = %other,
                            "bundle status inconclusive"
                        );
                    }
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                error!(
                    error = %e,
                    consecutive = consecutive_errors,
                    "submission failed"
                );

                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    warn!(
                        "too many consecutive errors, pausing for {:?}",
                        ERROR_PAUSE
                    );
                    tokio::time::sleep(ERROR_PAUSE).await;
                    consecutive_errors = 0;
                }
            }
        }

        tokio::time::sleep(LOOP_INTERVAL).await;
    }
}

// ─── Low-level alternative (matches the user's conceptual code) ─────────────

/// This function shows the **exact pattern** from the spec:
///
/// ```rust,no_run
/// # use solana_sdk::*;
/// let private_tx = Transaction::new_signed_with_payer(
///     &[swap_instruction, jito_tip_instruction],
///     Some(&payer.pubkey()),
///     &[&payer],
///     recent_blockhash,
/// );
/// let bam_client = BamClient::new("https://mainnet.bam.jito.wtf");
/// bam_client.send_bundle(private_tx).await?;
/// ```
///
/// Translated to the production SDK with VersionedTransaction:
#[allow(dead_code)]
async fn low_level_example() -> jito_bam_client::error::Result<()> {
    use solana_sdk::message::{v0, VersionedMessage};

    let payer = Keypair::new();
    let recent_blockhash = Hash::new_unique();

    // Your swap instruction (Raydium / Orca / Jupiter / etc.)
    let swap_instruction = system_instruction::transfer(
        &payer.pubkey(),
        &Pubkey::new_unique(),
        1_000_000,
    );

    // Jito tip instruction — MUST be the last instruction
    let jito_tip_instruction = tip::build_tip_instruction(
        &payer.pubkey(),
        50_000,  // 0.00005 SOL tip for BAM prioritisation
        None,    // random tip account
    )?;

    // Build a V0 VersionedTransaction (the modern Solana tx format)
    let message = v0::Message::try_compile(
        &payer.pubkey(),
        &[swap_instruction, jito_tip_instruction], // tip is LAST
        &[],  // address lookup tables
        recent_blockhash,
    )
    .expect("compile message");

    let private_tx = VersionedTransaction::try_new(
        VersionedMessage::V0(message),
        &[&payer],
    )?;

    // ── Route to BAM Node instead of standard RPC ───────────────────────
    let bam_client = BamClient::new("https://mainnet.bam.jito.wtf").await?;
    let result = bam_client.send_bundle(vec![private_tx]).await?;

    info!(
        bundle_id = %result.bundle_id,
        signatures = ?result.signatures,
        "bundle submitted via BAM — mempool bypassed"
    );

    // Optionally poll for confirmation
    let status = bam_client
        .poll_bundle_status(&result.bundle_id, 20, Duration::from_millis(500))
        .await?;

    info!(status = %status, "final bundle status");

    Ok(())
}

// ─── Or even simpler with the SDK helper: ─────────────────────────────────

#[allow(dead_code)]
async fn simplest_example() -> jito_bam_client::error::Result<()> {
    let sdk = JitoBamSdk::mainnet().await?;
    let payer = Keypair::new();

    let swap_ix = system_instruction::transfer(
        &payer.pubkey(),
        &Pubkey::new_unique(),
        1_000_000,
    );

    // One call does it all:  build tx → append tip → sign → send_bundle via BAM
    let result = sdk.execute(
        &[swap_ix],
        &payer,
        50_000,              // tip
        Hash::new_unique(),  // blockhash
    ).await?;

    info!("Done! bundle_id={}", result.bundle_id);
    Ok(())
}

// ─── Placeholder strategy types ─────────────────────────────────────────────

struct Opportunity {
    pair: String,
    expected_profit_lamports: u64,
    target_program: Pubkey,
    input_mint: Pubkey,
    output_mint: Pubkey,
    amount_in: u64,
}

/// Placeholder — your actual strategy logic goes here.
async fn detect_opportunity() -> Option<Opportunity> {
    // In a real bot this would:
    //   - Subscribe to account updates via geyser / websocket
    //   - Monitor DEX pools for price discrepancies
    //   - Check oracle prices vs on-chain prices
    //   - Evaluate liquidation opportunities
    //   - etc.
    None
}

/// Placeholder — build your actual swap instruction here.
///
/// For Raydium V4:  `raydium_amm::instruction::swap(...)`
/// For Orca:        `whirlpool::instruction::swap(...)`
/// For Jupiter:     Use the Jupiter Quote API → swap instruction
fn build_swap_instruction(payer: &Keypair, opp: &Opportunity) -> Instruction {
    // This is a placeholder.  Replace with your DEX SDK call.
    // The key point: this instruction does NOT include the tip.
    // The tip is automatically appended by the SDK as the LAST instruction.
    Instruction {
        program_id: opp.target_program,
        accounts: vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(opp.input_mint, false),
            AccountMeta::new(opp.output_mint, false),
        ],
        data: {
            let mut data = vec![0x09]; // placeholder swap discriminator
            data.extend_from_slice(&opp.amount_in.to_le_bytes());
            data
        },
    }
}
