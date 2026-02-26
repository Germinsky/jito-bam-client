/// Example: Raydium CLMM sniper with Jito BAM private execution.
///
/// Demonstrates a full production flow:
///
///   1. Initialize the Private Execution Engine (Jito TEE)
///   2. Fetch on-chain CLMM pool state (zero-copy)
///   3. Estimate swap output from `sqrt_price_x64`
///   4. Build a `SwapBaseIn` instruction with dynamic slippage protection
///   5. Append a Jito tip and compile into a VersionedTransaction (V0)
///   6. Submit privately via `send_bundle()`
///   7. Poll for on-chain confirmation
///
/// # Running
/// ```bash
/// cargo run --example raydium_clmm_sniper
/// ```
use std::str::FromStr;
use std::time::Duration;

use anyhow::Result;
use solana_sdk::{
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
};
use tracing::{error, info, warn};

use jito_bam_client::dynamic_tip::DynamicTipCalculator;
use jito_bam_client::engine::{self, EngineConfig, PrivateExecutionEngine};
use jito_bam_client::raydium_clmm::{
    self, apply_slippage, derive_ata, sqrt_price_x64_to_price, tick_to_price,
    ClmmSwapConfig, RaydiumClmmSwapBuilder,
    DEFAULT_SLIPPAGE_BPS, MIN_SQRT_PRICE_X64, MAX_SQRT_PRICE_X64,
};

// ─── Configuration ──────────────────────────────────────────────────────────

/// Example Raydium CLMM pool (SOL/USDC) — replace with your target.
const TARGET_POOL: &str = "2QdhepnKRTLjjSqPL1PtKNwqrUkoLee2Hbo9hLQx15Qi";

/// Swap amount in lamports (0.1 SOL = 100_000_000 lamports).
const SWAP_AMOUNT: u64 = 100_000_000;

/// Swap direction: true = sell token_0 (SOL) for token_1 (USDC).
const SWAP_A_TO_B: bool = true;

/// Slippage tolerance: 0.5%.
const SLIPPAGE_BPS: u16 = DEFAULT_SLIPPAGE_BPS;

/// Jito tip for BAM priority.
const TIP_LAMPORTS: u64 = 50_000;

/// Dynamic tip window size.
const TIP_WINDOW: usize = 10;

// ─── Main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,jito_bam_client=debug")
        .init();

    info!("=== Raydium CLMM Sniper with Jito BAM ===");

    // ── 1. Initialize engine ────────────────────────────────────────────
    let config = EngineConfig {
        jito_url: engine::endpoints::MAINNET.to_string(),
        uuid: None,
        rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
        max_status_polls: 20,
        poll_interval: Duration::from_secs(2),
        default_tip_lamports: TIP_LAMPORTS,
    };
    let engine = PrivateExecutionEngine::with_config(config);
    info!("engine initialized");

    // ── 2. Initialize dynamic tip calculator ────────────────────────────
    let mut tip_calc = DynamicTipCalculator::new(TIP_WINDOW);
    tip_calc.record_tip(45_000);
    tip_calc.record_tip(50_000);
    tip_calc.record_tip(55_000);
    info!(
        recommended = tip_calc.recommended_tip(),
        "tip calculator seeded"
    );

    // ── 3. Load ephemeral wallet (replace with your keypair) ────────────
    let payer = Keypair::new();
    info!(pubkey = %payer.pubkey(), "ephemeral wallet");

    // ── 4. Derive user ATAs ─────────────────────────────────────────────
    let spl_token = Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")?;
    let sol_mint = Pubkey::from_str("So11111111111111111111111111111111111111112")?;
    let usdc_mint = Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")?;

    let user_sol_ata = derive_ata(&payer.pubkey(), &sol_mint, &spl_token);
    let user_usdc_ata = derive_ata(&payer.pubkey(), &usdc_mint, &spl_token);
    info!(
        sol_ata = %user_sol_ata,
        usdc_ata = %user_usdc_ata,
        "derived user token accounts"
    );

    // ── 5. Build CLMM swap config ───────────────────────────────────────
    let pool_address = Pubkey::from_str(TARGET_POOL)?;
    let dynamic_tip = tip_calc.recommended_tip();

    let swap_config = ClmmSwapConfig {
        pool_address,
        amount_in: SWAP_AMOUNT,
        a_to_b: SWAP_A_TO_B,
        slippage_bps: SLIPPAGE_BPS,
        user_input_token: user_sol_ata,
        user_output_token: user_usdc_ata,
        user_authority: payer.pubkey(),
        lookup_table_addresses: vec![], // Add your ALTs here for V0 compression
        tip_lamports: dynamic_tip,
        tip_account: None,
    };

    info!(
        pool = %swap_config.pool_address,
        amount_in = swap_config.amount_in,
        direction = if swap_config.a_to_b { "a→b" } else { "b→a" },
        slippage_bps = swap_config.slippage_bps,
        tip = swap_config.tip_lamports,
        "swap configuration ready"
    );

    // ── 6. Build versioned transaction ──────────────────────────────────
    //
    // In production with a live RPC:
    //
    //   let builder = RaydiumClmmSwapBuilder::new(engine.rpc_client());
    //
    //   // Option A: Get just the instruction (integrate into your own tx):
    //   let swap_output = builder.build_swap_instruction(&swap_config)?;
    //   println!("Expected output: {} tokens", swap_output.expected_amount_out);
    //   println!("Min output:      {} tokens", swap_output.min_amount_out);
    //   println!("Tick arrays:     {:?}", swap_output.tick_arrays);
    //
    //   // Option B: Build a complete VersionedTransaction with Jito tip:
    //   let blockhash = engine.get_latest_blockhash()?;
    //   let versioned = builder.build_versioned_transaction(
    //       &swap_config,
    //       &payer,
    //       blockhash,
    //   )?;
    //
    //   // Submit to TEE
    //   let uuid = engine.send_bundle(vec![versioned.transaction]).await?;
    //
    //   // Track for tip calibration
    //   tip_calc.track_bundle(&uuid, dynamic_tip);
    //   let confirmation = engine.poll_bundle_status(&uuid).await?;
    //   tip_calc.record_tip(dynamic_tip);
    //
    //   println!("Bundle landed in slot {:?}", confirmation.slot);

    // ── Offline demonstration ───────────────────────────────────────────
    info!("--- Offline demonstration (no live RPC) ---");

    // Demonstrate price utilities
    let simulated_sqrt_price_x64: u128 = 2_268_512_309_178_278_000 << 16;
    let price = sqrt_price_x64_to_price(simulated_sqrt_price_x64, 9, 6);
    info!(
        sqrt_price_x64 = simulated_sqrt_price_x64,
        price_usd = format!("{price:.4}"),
        "simulated pool price (SOL/USDC)"
    );

    // Demonstrate output estimation
    let simulated_expected_out = (SWAP_AMOUNT as f64 / 1e9 * price * 1e6) as u64;
    let min_out = apply_slippage(simulated_expected_out, SLIPPAGE_BPS);
    info!(
        expected_usdc = simulated_expected_out,
        min_usdc = min_out,
        delta = simulated_expected_out - min_out,
        "slippage-protected output"
    );

    // Demonstrate tick array derivation
    let tick_current = -23028;
    let tick_spacing = 10u16;
    let tick_arrays = RaydiumClmmSwapBuilder::derive_tick_arrays(
        &pool_address,
        tick_current,
        tick_spacing,
        SWAP_A_TO_B,
    );
    info!(
        tick = tick_current,
        spacing = tick_spacing,
        "derived tick array PDAs:"
    );
    for (i, ta) in tick_arrays.iter().enumerate() {
        info!("  tick_array_{i}: {ta}");
    }

    // Demonstrate tick-to-price
    let tick_price = tick_to_price(tick_current);
    info!(
        tick = tick_current,
        price = format!("{tick_price:.8}"),
        "tick → price"
    );

    // Demonstrate instruction encoding
    let swap_data = raydium_clmm::encode_swap_data(
        SWAP_AMOUNT,
        min_out,
        MIN_SQRT_PRICE_X64,
    );
    info!(
        data_len = swap_data.len(),
        hex = hex::encode(&swap_data[..8]),
        "encoded SwapBaseIn instruction data ({} bytes)", swap_data.len()
    );

    // ── Summary ─────────────────────────────────────────────────────────
    info!("=== Summary ===");
    info!("  Pool:            {pool_address}");
    info!("  Direction:       {}", if SWAP_A_TO_B { "token_0 → token_1" } else { "token_1 → token_0" });
    info!("  Amount in:       {} lamports ({:.4} SOL)", SWAP_AMOUNT, SWAP_AMOUNT as f64 / 1e9);
    info!("  Slippage:        {}bps ({:.2}%)", SLIPPAGE_BPS, SLIPPAGE_BPS as f64 / 100.0);
    info!("  Expected out:    {} USDC atoms", simulated_expected_out);
    info!("  Min out:         {} USDC atoms", min_out);
    info!("  Jito tip:        {} lamports ({:.6} SOL)", dynamic_tip, dynamic_tip as f64 / 1e9);
    info!("  Tick arrays:     3 PDAs derived");
    info!("  Tx format:       VersionedTransaction V0 with ALT support");

    Ok(())
}
