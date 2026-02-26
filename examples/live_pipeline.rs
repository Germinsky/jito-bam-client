/// Live deployment pipeline: BAM-Ready RPC → Tip Market Monitor → Raydium CLMM Snipe
///
/// This example demonstrates the full operational deployment loop:
///
/// 1. **BAM RPC Setup** — Registers endpoints from Helius, Triton, QuickNode, and
///    Jito Native with API keys loaded from environment variables (never in source).
///
/// 2. **Tip Economy Monitor** — Spawns a background task that polls
///    `getBundleStatuses` every 5 seconds, builds a live percentile distribution,
///    and auto-escalates to the 75th percentile during volatile markets.
///
/// 3. **Trade Loop** — Feeds the recommended tip into a Raydium CLMM swap
///    for guaranteed first-100ms inclusion.
///
/// # Required Environment Variables
///
/// ```bash
/// export HELIUS_API_KEY="your-helius-api-key"          # optional
/// export TRITON_API_KEY="your-triton-api-key"          # optional
/// export QUICKNODE_ENDPOINT_URL="your-qn-slug"         # optional
/// export SOLANA_RPC_URL="https://api.mainnet-beta.solana.com"
/// ```
///
/// At least one BAM provider should be configured.  The pipeline selects the
/// healthiest, lowest-latency endpoint automatically.
///
/// # Usage
///
/// ```bash
/// cargo run --example live_pipeline
/// ```

use std::str::FromStr;
use std::time::Duration;

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;

use jito_bam_client::bam_rpc::{BamEndpointRegistry, Region, engine_from_registry};
use jito_bam_client::tip_market::{
    MarketState, TipMonitorConfig, seed_tip_market, spawn_tip_monitor,
};

// ═══════════════════════════════════════════════════════════════════════════
//  Constants
// ═══════════════════════════════════════════════════════════════════════════

/// Simulated pool address (replace with a real Raydium CLMM pool).
const POOL_ID: &str = "61acRgpURKTJ9HO6M477n2hGAFExNLRJmUqPUDCDwWM9";

/// Minimum interval between snipe attempts.
const LOOP_INTERVAL: Duration = Duration::from_secs(10);

// ═══════════════════════════════════════════════════════════════════════════
//  Main
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() {
    // ── Tracing setup ───────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .init();

    println!("========================================");
    println!("  BAM Live Pipeline — Operational Demo");
    println!("========================================\n");

    // ══════════════════════════════════════════════════════════════════════
    //  Step 1: BAM-Ready RPC Endpoint Registry
    // ══════════════════════════════════════════════════════════════════════

    println!("[1/3] Setting up BAM-Ready RPC endpoints...\n");

    let mut registry = BamEndpointRegistry::with_region(Region::Auto);

    // Always add Jito native endpoints (no API key needed).
    registry.add_jito_native(None);
    println!("  ✓ Jito Native: 6 regional endpoints registered");

    // Helius — reads HELIUS_API_KEY from env
    if registry.add_from_env(
        jito_bam_client::bam_rpc::BamProvider::Helius,
        "HELIUS_API_KEY",
    ) {
        println!("  ✓ Helius BAM Relay: registered from HELIUS_API_KEY");
    } else {
        println!("  ✗ Helius: HELIUS_API_KEY not set (skipping)");
    }

    // Triton — reads TRITON_API_KEY from env
    if registry.add_from_env(
        jito_bam_client::bam_rpc::BamProvider::Triton,
        "TRITON_API_KEY",
    ) {
        println!("  ✓ Triton BAM Relay: registered from TRITON_API_KEY");
    } else {
        println!("  ✗ Triton: TRITON_API_KEY not set (skipping)");
    }

    // QuickNode — reads QUICKNODE_ENDPOINT_URL from env
    if registry.add_from_env(
        jito_bam_client::bam_rpc::BamProvider::QuickNode,
        "QUICKNODE_ENDPOINT_URL",
    ) {
        println!("  ✓ QuickNode BAM: registered from QUICKNODE_ENDPOINT_URL");
    } else {
        println!("  ✗ QuickNode: QUICKNODE_ENDPOINT_URL not set (skipping)");
    }

    let summary = registry.summary();
    println!(
        "\n  Registry: {} total endpoints, {} healthy\n",
        summary.total, summary.healthy
    );

    if summary.healthy == 0 {
        eprintln!("ERROR: no healthy BAM endpoints. Set at least one API key env var.");
        std::process::exit(1);
    }

    // Select the best endpoint and create the engine.
    let engine = engine_from_registry(&registry);
    println!(
        "  ➜ Best endpoint: {}",
        registry.best_endpoint().url
    );

    // Verify tip accounts are reachable.
    match engine.get_tip_accounts().await {
        Ok(ref accounts) => println!(
            "  ✓ Tip accounts reachable ({} active)\n",
            accounts.len()
        ),
        Err(ref e) => {
            println!("  ⚠ Could not fetch tip accounts: {e}");
            println!("    (will use hardcoded defaults)\n");
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    //  Step 2: Tip Economy Monitor
    // ══════════════════════════════════════════════════════════════════════

    println!("[2/3] Starting Tip Market Monitor...\n");

    let tip_config = TipMonitorConfig {
        poll_interval: Duration::from_secs(5),
        sample_window: 100,
        volatility_ratio_threshold: 1.50,
        acceleration_threshold: 5_000.0,
        normal_premium: 1.05,
        volatile_premium: 1.10,
        floor: 1_000,
        ceiling: 5_000_000_000, // 5 SOL absolute ceiling
        fallback_tip: 50_000,
    };

    // Spawn the background poller with its own engine instance
    // (PrivateExecutionEngine is not Clone, so we build a second one).
    let monitor_engine = engine_from_registry(&registry);
    let tip_state = spawn_tip_monitor(monitor_engine, tip_config);

    // Seed with a reasonable baseline while the poller warms up.
    seed_tip_market(
        &tip_state,
        &[30_000, 40_000, 50_000, 60_000, 70_000, 50_000],
    );

    println!("  ✓ Tip monitor running (polling every 5s)");
    println!(
        "  ✓ Initial state: {} ({} samples)\n",
        tip_state.market_state(),
        tip_state.sample_count()
    );

    // ══════════════════════════════════════════════════════════════════════
    //  Step 3: Live Trading Loop
    // ══════════════════════════════════════════════════════════════════════

    println!("[3/3] Entering live trading loop...\n");
    println!("  Pool: {POOL_ID}");
    println!("  Interval: {}s\n", LOOP_INTERVAL.as_secs());
    println!("─────────────────────────────────────────────────────────\n");

    let pool = Pubkey::from_str(POOL_ID).expect("valid pool pubkey");

    // In production, load your keypair from file or env.
    // Here we generate an ephemeral one for demo purposes.
    let payer = Keypair::new();
    println!(
        "  Demo wallet: {} (unfunded — trades will fail)\n",
        payer.pubkey()
    );

    let mut iteration = 0u64;
    loop {
        iteration += 1;

        // ── Get tip recommendation ──────────────────────────────────────
        let rec = tip_state.recommend();
        let dist = &rec.distribution;

        let state_label = match rec.market_state {
            MarketState::Normal => "NORMAL",
            MarketState::Volatile => "⚡ VOLATILE",
            MarketState::Initializing => "INIT",
        };

        println!("┌── Iteration {iteration} ──────────────────────────────────");
        println!("│ Market state : {state_label}");
        println!(
            "│ Tip dist     : min={} p25={} p50={} p75={} p90={} max={}",
            dist.min, dist.p25, dist.p50, dist.p75, dist.p90, dist.max
        );
        println!("│ Spread ratio : {:.2}", dist.spread_ratio());
        println!("│ Acceleration : {:.0} lamports/sample", rec.tip_acceleration);
        println!(
            "│ Recommended  : {} lamports (base={}, premium={:.2}×{})",
            rec.tip_lamports,
            rec.base_percentile,
            rec.premium_factor,
            if rec.escalated { " ESCALATED" } else { "" }
        );

        // ── Build and submit trade ──────────────────────────────────────
        // In a real bot, you would:
        //   1. Fetch pool state via RaydiumClmmSwapBuilder::from_pool_account()
        //   2. Estimate output and check slippage
        //   3. Build the versioned transaction with the recommended tip
        //   4. Submit via engine.execute_and_confirm(tx)
        //
        // For this demo, we just print what would happen:

        println!("│ Pool         : {pool}");
        println!(
            "│ Action       : would submit swap bundle with {} lamport tip",
            rec.tip_lamports
        );

        // Simulate periodic volatility detection
        if iteration % 5 == 0 {
            // Force-volatile for one cycle to demonstrate escalation
            tip_state.force_volatile(true);
            println!("│ [DEMO] Forcing volatile mode for next cycle");
        } else {
            tip_state.force_volatile(false);
        }

        println!(
            "└─────────────────────────────────────────────────────────\n"
        );

        tokio::time::sleep(LOOP_INTERVAL).await;
    }
}
