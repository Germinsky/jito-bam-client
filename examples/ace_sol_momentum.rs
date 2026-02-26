/// ACE Plugin Example: SOL momentum trigger via Pyth → top-of-block BAM swap.
///
/// Strategy:
///   "If the price of $SOL on Pyth moves > 0.1% in 400ms,
///    execute my swap at the very top of the next BAM-assembled block."
///
/// This example demonstrates:
///   1. Configuring an ACE plugin with a `PriceMoveTrigger`
///   2. Simulating a Pyth price feed (WebSocket in production)
///   3. Running the trigger engine at high frequency (~10ms ticks)
///   4. When the trigger fires → building a swap tx with Jito tip
///      and submitting at `TopOfBlock` placement via BAM
///   5. Post-trade attestation verification
///
/// # Running
/// ```bash
/// RUST_LOG=info cargo run --example ace_sol_momentum
/// ```
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
};
use tracing::{debug, error, info, warn};

use jito_bam_client::ace::{
    AcePlugin, AcePluginConfig, CompositeTrigger, OrderPlacement,
    PriceDirection, PriceMoveTrigger, TriggerCondition, TriggerEngine,
    TriggerEvaluation,
};
use jito_bam_client::bam::BamClient;
use jito_bam_client::bundle::BundleStatus;
use jito_bam_client::sdk::JitoBamSdk;

// ═══════════════════════════════════════════════════════════════════════════
//  Strategy Configuration
// ═══════════════════════════════════════════════════════════════════════════

/// Pyth SOL/USD feed ID (mainnet-beta).
/// Production: "0xef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d"
const SOL_USD_FEED: &str = "SOL/USD";

/// Trigger threshold: 0.1% = 10 basis points.
const THRESHOLD_BPS: u32 = 10;

/// Observation window: 400 milliseconds.
const WINDOW_MS: u64 = 400;

/// Tip for top-of-block placement.  Aggressive tip to win the auction.
const TIP_LAMPORTS: u64 = 100_000; // 0.0001 SOL

/// Tick interval for the trigger evaluation loop.
const TICK_INTERVAL: Duration = Duration::from_millis(10);

/// How many simulated ticks to run (demo).
const MAX_TICKS: u64 = 200;

/// Maximum fires per minute (rate limit / risk guard).
const MAX_FIRES_PER_MINUTE: u32 = 6;

// ═══════════════════════════════════════════════════════════════════════════
//  Simulated Pyth price feed
// ═══════════════════════════════════════════════════════════════════════════

/// In production, replace with a Pyth Hermes WebSocket subscription:
///
/// ```text
/// wss://hermes.pyth.network/ws
/// → subscribe { type: "subscribe", ids: ["0xef0d8b6f..."] }
/// ← price updates every ~400ms
/// ```
///
/// This simulation generates a realistic SOL/USD price series with
/// occasional sharp moves that trigger the plugin.
struct SimulatedPythFeed {
    base_price: i64,      // Pyth-style i64 (e.g. 15000000000 = $150.00 with expo=-8)
    current_price: i64,
    tick: u64,
    rng_state: u64,
}

impl SimulatedPythFeed {
    fn new(base_price_usd: f64) -> Self {
        // Pyth uses exponent -8, so $150.00 = 15_000_000_000
        let base = (base_price_usd * 1e8) as i64;
        Self {
            base_price: base,
            current_price: base,
            tick: 0,
            rng_state: 0xDEAD_BEEF_CAFE_1234,
        }
    }

    fn next_price(&mut self) -> (i64, u64) {
        self.tick += 1;

        // Simple xorshift64 for deterministic "randomness"
        self.rng_state ^= self.rng_state << 13;
        self.rng_state ^= self.rng_state >> 7;
        self.rng_state ^= self.rng_state << 17;

        // Normal small noise: ±0.01%
        let noise = ((self.rng_state % 200) as i64 - 100) * (self.base_price / 1_000_000);

        // Inject sharp moves at specific ticks to demonstrate trigger firing
        let spike = match self.tick {
            // Tick 50: +0.15% spike (should trigger UP)
            50 => (self.base_price as f64 * 0.0015) as i64,
            51..=53 => (self.base_price as f64 * 0.0015) as i64,

            // Tick 100: −0.12% drop (should trigger DOWN)
            100 => -(self.base_price as f64 * 0.0012) as i64,
            101..=103 => -(self.base_price as f64 * 0.0012) as i64,

            // Tick 150: +0.2% spike (should trigger UP)
            150 => (self.base_price as f64 * 0.002) as i64,
            151..=155 => (self.base_price as f64 * 0.002) as i64,

            _ => 0,
        };

        self.current_price = self.base_price + noise + spike;

        let oracle_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        (self.current_price, oracle_ts)
    }

    fn current_usd(&self) -> f64 {
        self.current_price as f64 / 1e8
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Swap instruction builder (placeholder)
// ═══════════════════════════════════════════════════════════════════════════

/// In production, replace with Jupiter / Raydium / Orca swap instruction.
///
/// This builds a placeholder instruction that represents the swap you'd
/// execute when the momentum trigger fires.
fn build_swap_instruction(
    payer: &Pubkey,
    direction: &PriceDirection,
    amount_lamports: u64,
) -> Instruction {
    // Placeholder program ID (would be Jupiter aggregator, Raydium AMM, etc.)
    let swap_program = Pubkey::new_unique();

    // Encode direction + amount in instruction data
    let mut data = Vec::with_capacity(16);
    data.push(match direction {
        PriceDirection::Up => 0x01,   // Buy SOL (momentum follow)
        PriceDirection::Down => 0x02, // Sell SOL
    });
    data.extend_from_slice(&amount_lamports.to_le_bytes());

    Instruction {
        program_id: swap_program,
        accounts: vec![
            AccountMeta::new(*payer, true),       // signer, writable
            AccountMeta::new(Pubkey::new_unique(), false), // pool
            AccountMeta::new(Pubkey::new_unique(), false), // token A vault
            AccountMeta::new(Pubkey::new_unique(), false), // token B vault
        ],
        data,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Trade execution
// ═══════════════════════════════════════════════════════════════════════════

/// Execute a swap when the trigger fires.
///
/// In production this would:
///   1. Fetch a fresh blockhash from the RPC
///   2. Build the real swap instruction (Jupiter route, etc.)
///   3. Append Jito tip as last instruction
///   4. Submit via BAM with TopOfBlock placement
///   5. Verify inclusion attestation
async fn execute_triggered_swap(
    payer: &Keypair,
    direction: &PriceDirection,
    observed_bps: f64,
    fire_count: u32,
) -> anyhow::Result<()> {
    info!(
        "╔═════════════════════════════════════════════════════════╗"
    );
    info!(
        "║  ACE TRIGGER FIRED — executing top-of-block swap      ║"
    );
    info!(
        "╚═════════════════════════════════════════════════════════╝"
    );
    info!(
        "  Direction: {direction}  Move: {observed_bps:+.1} bps  Fire #{fire_count}"
    );

    // 1. Build swap instruction
    let swap_amount = LAMPORTS_PER_SOL / 10; // 0.1 SOL per trade
    let swap_ix = build_swap_instruction(&payer.pubkey(), direction, swap_amount);

    info!(
        "  Swap: {} {:.4} SOL via momentum follow",
        match direction {
            PriceDirection::Up => "BUY",
            PriceDirection::Down => "SELL",
        },
        swap_amount as f64 / LAMPORTS_PER_SOL as f64,
    );

    // 2. In production: connect to BAM and submit
    //
    //   let sdk = JitoBamSdk::mainnet().await?;
    //   let (result, status) = sdk.execute_and_confirm(
    //       &[swap_ix],
    //       &payer,
    //       TIP_LAMPORTS,
    //       recent_blockhash,
    //   ).await?;
    //
    //   info!("Bundle {} → {status}", result.bundle_id);
    //
    // For this demo, we simulate the submission:

    let simulated_bundle_id = format!("ace_bundle_{fire_count}_{}", rand::random::<u32>());
    info!(
        "  → Submitted to BAM: bundle_id={simulated_bundle_id}"
    );
    info!("  → Placement: TOP_OF_BLOCK (position 0)");
    info!("  → Tip: {} lamports ({:.6} SOL)",
        TIP_LAMPORTS,
        TIP_LAMPORTS as f64 / LAMPORTS_PER_SOL as f64,
    );

    // 3. Simulate confirmation
    tokio::time::sleep(Duration::from_millis(50)).await;
    info!("  → Status: Landed (simulated)");
    info!("  → Attestation: verified ✓");

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
//  Main: ACE Plugin execution loop
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,jito_bam_client=debug")
        .init();

    info!("╔═══════════════════════════════════════════════════════════╗");
    info!("║   Jito BAM ACE Plugin: SOL Momentum Strategy             ║");
    info!("║                                                           ║");
    info!("║   Trigger: SOL/USD moves > 0.1% in 400ms                 ║");
    info!("║   Action:  Swap at TOP OF BLOCK via BAM                   ║");
    info!("╚═══════════════════════════════════════════════════════════╝");
    info!("");

    // ── 1. Set up the ACE plugin ────────────────────────────────────────

    let sol_momentum_trigger = TriggerCondition::PriceMove(PriceMoveTrigger {
        feed_id: SOL_USD_FEED.into(),
        threshold_bps: THRESHOLD_BPS,
        window_ms: WINDOW_MS,
        direction: None, // trigger on both up and down
    });

    let mut plugin = AcePlugin::builder()
        .plugin_id("sol_momentum_v1")
        .label("SOL 0.1%/400ms momentum")
        .trigger(sol_momentum_trigger)
        .placement(OrderPlacement::TopOfBlock)
        .tip_lamports(TIP_LAMPORTS)
        .max_fires_per_minute(MAX_FIRES_PER_MINUTE)
        .build();

    info!("Plugin configured: {}", plugin.config.label);
    info!("  Trigger: {}", plugin.trigger);
    info!("  Placement: {}", plugin.config.placement);
    info!("  Tip: {} lamports", plugin.config.tip_lamports);
    info!("  Rate limit: {}/min", plugin.config.max_fires_per_minute);

    // Print the registration payload (what we'd send to BAM TEE)
    let reg = plugin.registration_payload();
    let reg_json = serde_json::to_string_pretty(&reg)?;
    info!("Registration payload:\n{reg_json}");
    info!("");

    // ── 2. Set up the trigger engine + price feed ───────────────────────

    let mut engine = TriggerEngine::new(2000); // hold ~20 seconds of 10ms ticks
    let mut pyth = SimulatedPythFeed::new(150.0); // $150 SOL
    let payer = Keypair::new();

    // ── 3. Run the high-frequency evaluation loop ───────────────────────

    info!("Starting evaluation loop ({MAX_TICKS} ticks × {TICK_INTERVAL:?})...\n");

    let mut total_fires = 0u32;
    let loop_start = Instant::now();

    for tick in 0..MAX_TICKS {
        // a) Get latest Pyth price
        let (price, oracle_ts) = pyth.next_price();
        engine.record_price(SOL_USD_FEED, price, 5000, oracle_ts);

        // b) Evaluate trigger
        let eval = plugin.should_fire(&mut engine);

        // c) Log price periodically (every 20 ticks)
        if tick % 20 == 0 {
            debug!(
                tick,
                price = format!("${:.2}", pyth.current_usd()),
                "price update"
            );
        }

        // d) If trigger fired → execute the swap
        if eval.fired {
            total_fires += 1;
            let direction = eval
                .observed_direction
                .clone()
                .unwrap_or(PriceDirection::Up);
            let bps = eval.observed_bps.unwrap_or(0.0);

            info!("");
            info!("Tick {tick}: TRIGGER FIRED — {eval}");

            execute_triggered_swap(&payer, &direction, bps, total_fires).await?;

            info!("");
        }

        // e) Wait for next tick
        tokio::time::sleep(TICK_INTERVAL).await;
    }

    // ── 4. Session summary ──────────────────────────────────────────────

    let elapsed = loop_start.elapsed();
    info!("╔═══════════════════════════════════════════════════════════╗");
    info!("║   Session Complete                                       ║");
    info!("╚═══════════════════════════════════════════════════════════╝");
    info!("  Duration: {elapsed:.1?}");
    info!("  Ticks: {MAX_TICKS}");
    info!("  Triggers fired: {total_fires}");
    info!(
        "  Avg tick: {:.1}ms",
        elapsed.as_millis() as f64 / MAX_TICKS as f64
    );

    if total_fires > 0 {
        info!("  All {total_fires} triggered swaps submitted at TOP_OF_BLOCK");
    } else {
        info!("  No momentum events detected (adjust threshold or feed)");
    }

    // ── 5. Show the composite trigger variant for advanced use ──────────

    info!("");
    info!("── Advanced: Composite trigger example ──");

    let advanced_trigger = CompositeTrigger::And(vec![
        // SOL must move > 0.1% in 400ms
        CompositeTrigger::Single(TriggerCondition::PriceMove(PriceMoveTrigger {
            feed_id: "SOL/USD".into(),
            threshold_bps: 10,
            window_ms: 400,
            direction: None,
        })),
        // AND SOL must be above $140
        CompositeTrigger::Single(TriggerCondition::PriceLevel(
            jito_bam_client::ace::PriceLevelTrigger {
                feed_id: "SOL/USD".into(),
                level: 14_000_000_000, // $140.00 in Pyth expo=-8
                above: true,
            },
        )),
    ]);

    info!("  {advanced_trigger}");
    info!("  → Only fires if SOL moves 0.1%+ AND is above $140");

    Ok(())
}
