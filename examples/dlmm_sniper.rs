/// Example: Meteora DLMM production sniper with dynamic Jito tips.
///
/// Demonstrates a full production trading loop:
///
///   1. Initialize the Private Execution Engine (jito-sdk-rust)
///   2. Set up the DynamicTipCalculator (median + 5% edge)
///   3. Detect a swap opportunity on a Meteora DLMM pool
///   4. Build a swap ix with 0.5% slippage-protected `min_amount_out`
///   5. Submit privately through the Jito TEE with a competitive tip
///   6. Record the landed tip for future calibration
///   7. Repeat
///
/// # Running
/// ```bash
/// cargo run --example dlmm_sniper
/// ```
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::Result;
use solana_sdk::{
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
};
use tracing::{error, info, warn};

use jito_bam_client::dynamic_tip::DynamicTipCalculator;
use jito_bam_client::engine::{self, EngineConfig, PrivateExecutionEngine};
use jito_bam_client::meteora_dlmm::{
    self, apply_slippage, bin_id_to_price, derive_ata, DlmmSwapBuilder,
    DlmmSwapConfig, DEFAULT_SLIPPAGE_BPS,
};

// ─── Configuration ──────────────────────────────────────────────────────────

/// Example DLMM pool (SOL/USDC) — replace with your target pool.
const TARGET_POOL: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";

/// Swap amount in lamports (0.1 SOL).
const SWAP_AMOUNT: u64 = 100_000_000;

/// Slippage tolerance (0.5% = 50 bps).
const SLIPPAGE_BPS: u16 = DEFAULT_SLIPPAGE_BPS;

/// Dynamic tip window: track the last 10 successful bundles.
const TIP_WINDOW_SIZE: usize = 10;

/// Loop interval between opportunity checks.
const LOOP_INTERVAL: Duration = Duration::from_millis(400);

/// Maximum consecutive errors before circuit-breaker pause.
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// Circuit-breaker pause duration.
const ERROR_PAUSE: Duration = Duration::from_secs(10);

// ─── Simulated opportunity detection ────────────────────────────────────────

struct SniperOpportunity {
    pool: Pubkey,
    amount_in: u64,
    swap_for_y: bool,
}

fn detect_opportunity(iteration: u64) -> Option<SniperOpportunity> {
    // In production: check price feeds, pool state, mempool, etc.
    // Simulated: trigger every 3rd iteration
    if iteration % 3 == 0 {
        Some(SniperOpportunity {
            pool: Pubkey::from_str(TARGET_POOL).unwrap(),
            amount_in: SWAP_AMOUNT,
            swap_for_y: true, // sell SOL for USDC
        })
    } else {
        None
    }
}

// ─── Main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,jito_bam_client=debug")
        .init();

    info!("=== Meteora DLMM Sniper with Dynamic Tips ===");

    // ── 1. Initialize the Private Execution Engine ──────────────────────
    let config = EngineConfig {
        jito_url: engine::endpoints::MAINNET.to_string(),
        uuid: None,
        rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
        max_status_polls: 20,
        poll_interval: Duration::from_secs(2),
        default_tip_lamports: 50_000,
    };
    let engine = PrivateExecutionEngine::with_config(config);
    info!("engine initialized");

    // ── 2. Initialize the dynamic tip calculator ────────────────────────
    let mut tip_calc = DynamicTipCalculator::new(TIP_WINDOW_SIZE);

    // Seed with historical data (in production, load from persistence)
    let seed_tips = [45_000u64, 48_000, 50_000, 52_000, 55_000];
    for tip in seed_tips {
        tip_calc.record_tip(tip);
    }
    let initial_snap = tip_calc.snapshot();
    info!(
        median = initial_snap.median_tip,
        recommended = initial_snap.recommended_tip,
        samples = initial_snap.sample_count,
        "tip calculator seeded"
    );

    // ── 3. Load wallet ──────────────────────────────────────────────────
    let payer = Keypair::new();
    info!(pubkey = %payer.pubkey(), "ephemeral wallet (replace with real keypair)");

    // ── 4. Pre-derive user token accounts ───────────────────────────────
    // In production: resolve mint addresses from the pool, then derive ATAs
    let spl_token = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")?;
    let sol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")?;
    let usdc_mint = Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")?;

    let user_token_x = derive_ata(&payer.pubkey(), &sol_mint, &spl_token);
    let user_token_y = derive_ata(&payer.pubkey(), &usdc_mint, &spl_token);
    info!(
        token_x_ata = %user_token_x,
        token_y_ata = %user_token_y,
        "derived user token accounts"
    );

    // ── 5. Sniper loop ──────────────────────────────────────────────────
    let mut consecutive_errors: u32 = 0;
    let mut total_attempts: u64 = 0;
    let mut landed_count: u64 = 0;
    let start = Instant::now();

    for iteration in 0..15 {
        // Circuit breaker
        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
            warn!(
                errors = consecutive_errors,
                "circuit breaker — pausing {}s",
                ERROR_PAUSE.as_secs()
            );
            tokio::time::sleep(ERROR_PAUSE).await;
            consecutive_errors = 0;
        }

        // Detect opportunity
        let opp = match detect_opportunity(iteration) {
            Some(o) => o,
            None => {
                tokio::time::sleep(LOOP_INTERVAL).await;
                continue;
            }
        };

        // Refresh tip calculator from Jito (check if previous bundles landed)
        if let Err(e) = tip_calc.refresh_from_jito(&engine).await {
            warn!(error = %e, "tip refresh failed (continuing with cached data)");
        }

        // Get dynamic tip
        let dynamic_tip = tip_calc.recommended_tip();
        let snap = tip_calc.snapshot();
        info!(
            iteration,
            dynamic_tip,
            median = snap.median_tip,
            samples = snap.sample_count,
            "dynamic tip calculated"
        );

        // ── Build the DLMM swap instruction ─────────────────────────────
        //
        // The DlmmSwapBuilder fetches on-chain pool state, active bins,
        // and computes min_amount_out with slippage protection.
        //
        // In offline mode (no RPC), we demonstrate the config setup:
        let swap_config = DlmmSwapConfig {
            lb_pair: opp.pool,
            amount_in: opp.amount_in,
            swap_for_y: opp.swap_for_y,
            slippage_bps: SLIPPAGE_BPS,
            user_token_x,
            user_token_y,
            user_authority: payer.pubkey(),
            host_fee_in: None,
            lookup_table_addresses: vec![], // Add your ALTs here
        };

        info!(
            pool = %swap_config.lb_pair,
            amount_in = swap_config.amount_in,
            slippage_bps = swap_config.slippage_bps,
            swap_for_y = swap_config.swap_for_y,
            "building DLMM swap instruction"
        );

        // In production with RPC:
        //   let builder = DlmmSwapBuilder::new(engine.rpc_client());
        //   let swap_output = builder.build(&swap_config)?;
        //   let swap_ix = swap_output.instruction;
        //   let min_out = swap_output.min_amount_out;

        // Offline simulation: estimate min_amount_out from a notional price
        let notional_price = 150.0; // SOL/USDC ~$150
        let expected_usdc = (opp.amount_in as f64 / 1e9 * notional_price * 1e6) as u64;
        let min_out = apply_slippage(expected_usdc, SLIPPAGE_BPS);
        info!(
            expected_usdc_atoms = expected_usdc,
            min_out,
            "slippage-protected min_amount_out"
        );

        // Build a placeholder swap ix (replace with swap_output.instruction)
        let swap_ix = solana_sdk::system_instruction::transfer(
            &payer.pubkey(),
            &opp.pool,
            opp.amount_in,
        );

        // ── Submit via the engine with dynamic tip ──────────────────────
        total_attempts += 1;

        match engine.execute(&[swap_ix], &payer, dynamic_tip).await {
            Ok(bundle_uuid) => {
                info!(
                    bundle_uuid = %bundle_uuid,
                    tip = dynamic_tip,
                    "bundle submitted"
                );
                consecutive_errors = 0;

                // Track the bundle for tip calibration
                tip_calc.track_bundle(&bundle_uuid, dynamic_tip);

                // Poll for confirmation
                match engine.poll_bundle_status(&bundle_uuid).await {
                    Ok(confirmation) => {
                        landed_count += 1;

                        // Record the landed tip to calibrate future tips
                        tip_calc.record_tip(dynamic_tip);

                        info!(
                            status = %confirmation.status,
                            slot = ?confirmation.slot,
                            tip = dynamic_tip,
                            "LANDED — swap confirmed on-chain"
                        );

                        if let Some(sig) = confirmation.transactions.first() {
                            info!("https://solscan.io/tx/{sig}");
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "bundle did not land");
                    }
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                error!(
                    error = %e,
                    consecutive = consecutive_errors,
                    "execution failed"
                );
            }
        }

        tokio::time::sleep(LOOP_INTERVAL).await;
    }

    // ── Summary ─────────────────────────────────────────────────────────
    let elapsed = start.elapsed();
    let final_snap = tip_calc.snapshot();

    info!("=== Sniper Session Summary ===");
    info!("  Duration:         {:.1}s", elapsed.as_secs_f64());
    info!("  Total attempts:   {total_attempts}");
    info!("  Bundles landed:   {landed_count}");
    info!(
        "  Success rate:     {:.1}%",
        if total_attempts > 0 {
            landed_count as f64 / total_attempts as f64 * 100.0
        } else {
            0.0
        }
    );
    info!("  Tip window:       {} samples", final_snap.sample_count);
    info!("  Final median tip: {} lamports", final_snap.median_tip);
    info!("  Final rec. tip:   {} lamports", final_snap.recommended_tip);
    info!("  Tip range:        {}–{} lamports", final_snap.min_tip, final_snap.max_tip);

    Ok(())
}
