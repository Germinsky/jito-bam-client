/// JitoBAM Private Execution Engine — binary entry-point.
///
/// Demonstrates the full TEE-routed swap flow:
///   1. Initialize `PrivateExecutionEngine` with real `jito-sdk-rust`
///   2. Fetch TEE-attested tip accounts from the block-engine
///   3. Get latest blockhash from Solana RPC
///   4. Build a swap instruction + Jito tip
///   5. Sign, base64-encode, submit as a private Jito bundle
///   6. Poll bundle status until landed or failed
///
/// # Usage
/// ```bash
/// # With default mainnet endpoints:
/// cargo run
///
/// # With custom endpoints:
/// JITO_URL=https://ny.mainnet.block-engine.jito.wtf/api/v1 \
/// RPC_URL=https://my-rpc.example.com \
/// cargo run
/// ```
use std::env;

use anyhow::Result;
use solana_sdk::{
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
};
use tracing::{error, info};

use jito_bam_client::engine::{self, PrivateExecutionEngine};

// ─── Configuration ──────────────────────────────────────────────────────────

/// Jito tip amount (0.00005 SOL — adjust for network conditions).
const TIP_LAMPORTS: u64 = 50_000;

/// SOL amount to swap (placeholder — replace with real swap logic).
const SWAP_AMOUNT: u64 = LAMPORTS_PER_SOL / 100; // 0.01 SOL

#[tokio::main]
async fn main() -> Result<()> {
    // ── Tracing setup ───────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter("info,jito_bam_client=debug")
        .init();

    info!("=== JitoBAM Private Execution Engine ===");

    // ── 1. Initialize the engine ────────────────────────────────────────
    let jito_url = env::var("JITO_URL")
        .unwrap_or_else(|_| engine::endpoints::MAINNET.to_string());
    let rpc_url = env::var("RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let uuid = env::var("JITO_UUID").ok();

    let engine = PrivateExecutionEngine::new(&jito_url, uuid, &rpc_url);
    info!(jito = %jito_url, rpc = %rpc_url, "engine initialized");

    // ── 2. Load keypair ─────────────────────────────────────────────────
    // In production: load from file or HSM
    //   let payer = solana_sdk::signature::read_keypair_file("~/.config/solana/id.json")?;
    let payer = Keypair::new();
    info!(pubkey = %payer.pubkey(), "wallet loaded (ephemeral keypair)");

    // ── 3. Fetch TEE-attested tip accounts ──────────────────────────────
    match engine.get_tip_accounts().await {
        Ok(accounts) => {
            info!(
                count = accounts.len(),
                "fetched TEE-attested tip accounts"
            );
            for (i, acct) in accounts.iter().enumerate() {
                info!("  tip_account[{i}] = {acct}");
            }
        }
        Err(e) => {
            // Non-fatal: the engine's execute() will also fetch a tip
            // account on its own. This is just for demonstration.
            info!("tip account fetch skipped (expected offline): {e}");
        }
    }

    // ── 4. Build swap instruction ───────────────────────────────────────
    // Placeholder: replace with your actual DEX swap instruction
    // (e.g. Raydium, Orca, Jupiter route, etc.)
    let target = Pubkey::new_unique();
    let swap_ix = system_instruction::transfer(
        &payer.pubkey(),
        &target,
        SWAP_AMOUNT,
    );
    info!(
        amount = SWAP_AMOUNT,
        target = %target,
        "built swap instruction"
    );

    // ── 5. Execute privately via the engine ──────────────────────────────
    info!(
        tip = TIP_LAMPORTS,
        "submitting private bundle via Jito TEE..."
    );

    match engine.execute(&[swap_ix], &payer, TIP_LAMPORTS).await {
        Ok(bundle_uuid) => {
            info!(bundle_uuid = %bundle_uuid, "bundle accepted by block-engine");

            // ── 6. Poll for confirmation ────────────────────────────────
            info!("polling bundle status...");
            match engine.poll_bundle_status(&bundle_uuid).await {
                Ok(confirmation) => {
                    info!(
                        status = %confirmation.status,
                        txs = confirmation.transactions.len(),
                        slot = ?confirmation.slot,
                        "bundle finalized on-chain!"
                    );
                    if let Some(sig) = confirmation.transactions.first() {
                        info!("https://solscan.io/tx/{sig}");
                    }
                }
                Err(e) => {
                    error!(
                        bundle_uuid = %bundle_uuid,
                        error = %e,
                        "bundle status polling failed"
                    );
                }
            }
        }
        Err(e) => {
            // Expected when running offline / without a funded wallet
            error!(error = %e, "bundle submission failed");
            info!("(this is expected when running without a funded wallet or network)");
        }
    }

    info!("=== Engine shutdown ===");
    Ok(())
}
