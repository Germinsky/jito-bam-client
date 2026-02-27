# Jito BAM Client — Complete User Guide

> **Version**: 0.1.0 · **Rust Edition**: 2021 · **Last updated**: February 2026

A production-grade Rust SDK for submitting private, MEV-protected Solana
transactions through Jito's Block Auction Marketplace (BAM). Supports Raydium
CLMM swaps, Meteora DLMM swaps, dynamic tip management, multi-provider RPC
failover, TEE attestation verification, and conditional execution (ACE).

---

## Table of Contents

1. [Quick Start](#1-quick-start)
2. [Installation & Setup](#2-installation--setup)
3. [Architecture Overview](#3-architecture-overview)
4. [Core Concepts](#4-core-concepts)
5. [The Private Execution Engine](#5-the-private-execution-engine)
6. [BAM-Ready RPC Provider Setup](#6-bam-ready-rpc-provider-setup)
7. [Tip Strategies](#7-tip-strategies)
   - 7.1 [Static Tips](#71-static-tips)
   - 7.2 [Dynamic Tips](#72-dynamic-tips)
   - 7.3 [Tip Market Monitor](#73-tip-market-monitor-autonomous)
8. [Raydium CLMM Swaps](#8-raydium-clmm-swaps)
9. [Meteora DLMM Swaps](#9-meteora-dlmm-swaps)
10. [ACE — Conditional Execution](#10-ace--conditional-execution)
11. [TEE Attestation Verification](#11-tee-attestation-verification)
12. [Building a Complete Trading Bot](#12-building-a-complete-trading-bot)
13. [Configuration Reference](#13-configuration-reference)
14. [Examples](#14-examples)
15. [Troubleshooting](#15-troubleshooting)
16. [API Quick Reference](#16-api-quick-reference)

---

## 1. Quick Start

The absolute fastest path to submitting your first private bundle:

```rust
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::system_instruction;
use jito_bam_client::engine::PrivateExecutionEngine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Create engine (connects to Jito block-engine + Solana RPC)
    let engine = PrivateExecutionEngine::mainnet(
        "https://api.mainnet-beta.solana.com",
    );

    // 2. Load your keypair
    let payer = Keypair::new(); // ← replace with your real keypair

    // 3. Build your instruction(s)
    let ix = system_instruction::transfer(
        &payer.pubkey(),
        &payer.pubkey(), // ← replace with target
        1_000_000,       // 0.001 SOL
    );

    // 4. Execute with a 50,000 lamport Jito tip
    let confirmation = engine
        .execute_and_confirm(&[ix], &payer, 50_000)
        .await?;

    println!("Bundle status: {:?}", confirmation.status);
    Ok(())
}
```

That's it. The engine handles:
- Fetching a recent blockhash
- Picking a random Jito tip account
- Appending the tip as the final instruction
- Building a V0 `VersionedTransaction`
- Sending a single-tx bundle to Jito's block-engine
- Polling `getBundleStatuses` until landed or failed

---

## 2. Installation & Setup

### 2.1 Prerequisites

| Requirement | Version | Install |
|---|---|---|
| Rust | 1.75+ | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| Solana CLI (optional) | 2.x | `sh -c "$(curl -sSfL https://release.anza.xyz/stable/install)"` |
| A funded Solana wallet | — | `solana-keygen new -o ~/.config/solana/id.json` |

### 2.2 Add to your project

```toml
[dependencies]
jito-bam-client = { git = "https://github.com/Germinsky/jito-bam-client.git" }
solana-sdk = "2.1"
solana-client = "2.1"
tokio = { version = "1", features = ["full"] }
anyhow = "1"
tracing-subscriber = "0.3"
```

### 2.3 Clone & build from source

```bash
git clone https://github.com/Germinsky/jito-bam-client.git
cd jito-bam-client
cargo build --release
cargo test          # 185 tests should pass
```

### 2.4 Environment variables

```bash
# Required — your Solana keypair
export SOLANA_KEYPAIR_PATH="$HOME/.config/solana/id.json"

# Required — an RPC endpoint (free tier works for testing)
export SOLANA_RPC_URL="https://api.mainnet-beta.solana.com"

# Optional — BAM provider API keys (set any/all you have)
export HELIUS_API_KEY="your-helius-api-key"
export TRITON_API_KEY="your-triton-api-key"
export QUICKNODE_ENDPOINT_URL="your-quicknode-slug"
```

### 2.5 Loading your keypair

```rust
use solana_sdk::signature::Keypair;
use std::fs;

fn load_keypair() -> Keypair {
    let path = std::env::var("SOLANA_KEYPAIR_PATH")
        .unwrap_or_else(|_| {
            format!("{}/.config/solana/id.json", std::env::var("HOME").unwrap())
        });
    let bytes: Vec<u8> = serde_json::from_str(
        &fs::read_to_string(&path).expect("keypair file not found"),
    ).expect("invalid keypair JSON");
    Keypair::from_bytes(&bytes).expect("invalid keypair bytes")
}
```

---

## 3. Architecture Overview

```
Your Trading Bot
       │
       ▼
┌──────────────────────────────────────────────────┐
│           jito-bam-client SDK                    │
│                                                  │
│  ┌────────────┐  ┌─────────────┐  ┌──────────┐  │
│  │ BAM RPC    │  │ Tip Market  │  │ ACE      │  │
│  │ Registry   │  │ Monitor     │  │ Triggers │  │
│  │ (failover) │  │ (auto p75)  │  │          │  │
│  └─────┬──────┘  └──────┬──────┘  └────┬─────┘  │
│        │                │              │         │
│        ▼                ▼              ▼         │
│  ┌─────────────────────────────────────────────┐ │
│  │         PrivateExecutionEngine              │ │
│  │  (Jito JSON-RPC + Solana RPC + tip append)  │ │
│  └──────────────┬──────────────────────────────┘ │
│                 │                                │
│  ┌──────────────┼──────────────────────────────┐ │
│  │  Swap Builders                              │ │
│  │  ├── RaydiumClmmSwapBuilder                 │ │
│  │  ├── DlmmSwapBuilder (Meteora)              │ │
│  │  └── tx_builder (generic tipped V0 txs)     │ │
│  └─────────────────────────────────────────────┘ │
│                                                  │
│  ┌─────────────────────────────────────────────┐ │
│  │  Attestation Verifier                       │ │
│  │  (Ed25519 sig, Merkle proof, TEE check)     │ │
│  └─────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────┘
       │
       ▼
  Jito Block Engine  ──▶  Solana Validators
```

### Module Map (14 modules)

| Module | Purpose |
|---|---|
| `engine` | **Core** — `PrivateExecutionEngine` wrapping Jito JSON-RPC |
| `bam_rpc` | Multi-provider endpoint registry with health checks |
| `tip_market` | Autonomous background tip monitor (5s polling, p75 escalation) |
| `dynamic_tip` | Rolling-window tip calculator (median × premium) |
| `tip` | Static tip helpers (8 hardcoded Jito accounts) |
| `tx_builder` | Generic tipped V0 transaction builder |
| `raydium_clmm` | Raydium Concentrated Liquidity swap builder |
| `meteora_dlmm` | Meteora DLMM swap builder |
| `ace` | Conditional execution triggers (price moves, timers, composites) |
| `attestation` | TEE inclusion proof verification |
| `bam` | Low-level gRPC BAM client |
| `sdk` | High-level facade over `BamClient` |
| `bundle` | Bundle result/status types |
| `error` | Central `JitoBamError` enum |

---

## 4. Core Concepts

### 4.1 What is BAM?

Jito's **Block Auction Marketplace** lets you submit transactions privately
to block-engine validators. Your transactions are:

- **Private** — not visible in the public mempool (no frontrunning)
- **Atomic** — all-or-nothing bundle execution
- **Ordered** — you control the instruction sequence within your bundle
- **MEV-protected** — validators can't extract value from your trades

### 4.2 Bundles vs. single transactions

| | Single Tx | Bundle |
|---|---|---|
| Privacy | ✅ via `sendPrivateTransaction` | ✅ via `sendBundle` |
| Atomicity | ❌ | ✅ All txs land or none |
| Multi-tx | ❌ | ✅ Up to 5 txs |
| Ordering | ❌ | ✅ Strict ordering |
| Typical use | Quick swaps | Arbitrage, liquidations |

### 4.3 Tips

Every bundle must include a **tip** — a SOL transfer to a Jito tip account.
This incentivizes validators to include your bundle. Higher tips = higher
priority in the block auction.

The SDK provides three tip strategies (see §7):
1. **Static**: fixed amount (simplest)
2. **Dynamic**: median of recent landed bundles × 1.05
3. **Market Monitor**: continuous background polling with p75 escalation

### 4.4 Two execution paths

The SDK offers two ways to interact with Jito:

| Path | Module | Protocol | When to use |
|---|---|---|---|
| `PrivateExecutionEngine` | `engine` | JSON-RPC | **Recommended** — simpler, Jito SDK native |
| `BamClient` / `JitoBamSdk` | `bam`, `sdk` | gRPC | TEE attestation, custom channel config |

For most trading bots, use `PrivateExecutionEngine`.

---

## 5. The Private Execution Engine

The engine is your primary interface for sending private transactions.

### 5.1 Creating an engine

```rust
use jito_bam_client::engine::{PrivateExecutionEngine, EngineConfig, endpoints};

// Option A: Quick start with mainnet defaults
let engine = PrivateExecutionEngine::mainnet(
    "https://api.mainnet-beta.solana.com",
);

// Option B: Specific Jito region (lower latency)
let engine = PrivateExecutionEngine::new(
    endpoints::NY,                           // Jito block-engine URL
    Some("my-uuid".to_string()),             // Optional auth UUID
    "https://api.mainnet-beta.solana.com",   // Solana RPC
);

// Option C: Full configuration
let config = EngineConfig {
    jito_url: endpoints::TOKYO.to_string(),
    uuid: None,
    rpc_url: "https://my-rpc.example.com".to_string(),
    max_status_polls: 20,              // poll up to 20 times
    poll_interval: Duration::from_secs(3), // 3s between polls
    default_tip_lamports: 100_000,     // 0.0001 SOL
};
let engine = PrivateExecutionEngine::with_config(config);
```

### 5.2 Available Jito endpoints

| Constant | Region | URL |
|---|---|---|
| `endpoints::MAINNET` | Auto | `https://mainnet.block-engine.jito.wtf` |
| `endpoints::NY` | US East | `https://ny.mainnet.block-engine.jito.wtf` |
| `endpoints::SLC` | US West | `https://slc.mainnet.block-engine.jito.wtf` |
| `endpoints::AMSTERDAM` | Europe | `https://amsterdam.mainnet.block-engine.jito.wtf` |
| `endpoints::FRANKFURT` | Europe | `https://frankfurt.mainnet.block-engine.jito.wtf` |
| `endpoints::TOKYO` | Asia | `https://tokyo.mainnet.block-engine.jito.wtf` |

**Tip**: Choose the endpoint closest to your server for lowest latency.

### 5.3 Execution methods

#### `execute()` — fire and forget

```rust
// Send a bundle (returns immediately with UUID)
let bundle_uuid = engine
    .execute(&[swap_ix, another_ix], &payer, 50_000)
    .await?;
println!("Submitted bundle: {bundle_uuid}");
```

#### `execute_and_confirm()` — fire and wait

```rust
// Send + poll until landed/failed
let confirmation = engine
    .execute_and_confirm(&[swap_ix], &payer, 50_000)
    .await?;

match confirmation.status {
    InFlightStatus::Landed => {
        println!("✅ Landed in slot {:?}", confirmation.slot);
    }
    InFlightStatus::Failed => {
        println!("❌ Failed: {:?}", confirmation.error);
    }
    InFlightStatus::Pending => {
        println!("⏳ Still pending after max polls");
    }
    _ => {}
}
```

#### `execute_with_signers()` — multiple signers

```rust
let mint_authority = Keypair::new();
let bundle_uuid = engine
    .execute_with_signers(
        &[create_token_ix, mint_ix],
        &payer,
        &[&mint_authority],  // additional signers
        100_000,
    )
    .await?;
```

#### `send_bundle()` — pre-built transactions

```rust
// Build your own transaction(s) and send as a bundle
let tx = build_tipped_versioned_transaction(
    &instructions,
    &payer,
    &[],
    blockhash,
    &TxBuildConfig {
        tip_lamports: 50_000,
        tip_account: None,
        address_lookup_tables: vec![],
    },
)?;

let uuid = engine.send_bundle(vec![tx]).await?;
```

#### `send_transaction()` — single private tx (no bundle)

```rust
let sig = engine.send_transaction(&tx, true).await?;
```

### 5.4 Utility methods

```rust
// Get all 8 Jito tip accounts
let tip_accounts: Vec<Pubkey> = engine.get_tip_accounts().await?;

// Get a random tip account
let tip_account: Pubkey = engine.get_random_tip_account()?;

// Get a fresh blockhash
let blockhash: Hash = engine.get_latest_blockhash().await?;

// Poll an existing bundle
let status = engine.poll_bundle_status("your-uuid").await?;

// Access the inner clients
let rpc: &RpcClient = engine.rpc_client();
let jito: &JitoJsonRpcSDK = engine.jito_sdk();
let config: &EngineConfig = engine.config();
```

---

## 6. BAM-Ready RPC Provider Setup

In production, you want **multiple BAM endpoints** for failover and
latency optimization. The `bam_rpc` module manages this.

### 6.1 Setting up the registry

```rust
use jito_bam_client::bam_rpc::{
    BamEndpointRegistry, BamProvider, Region,
    engine_from_registry,
};

let mut registry = BamEndpointRegistry::with_region(Region::UsEast);

// Always add Jito native (free, no API key needed)
registry.add_jito_native(None); // Adds all 6 regional endpoints

// Add premium providers from env vars (keys never in source!)
registry.add_from_env(BamProvider::Helius, "HELIUS_API_KEY");
registry.add_from_env(BamProvider::Triton, "TRITON_API_KEY");
registry.add_from_env(BamProvider::QuickNode, "QUICKNODE_ENDPOINT_URL");

// Or add with an explicit key (less secure — avoid in production)
registry.add_with_key(BamProvider::Helius, "your-api-key");

// Add a fully custom endpoint
registry.add_custom(
    "https://my-bam-relay.example.com",
    "https://my-rpc.example.com",
);
```

### 6.2 Endpoint selection

The registry automatically selects the best endpoint based on:

1. **Healthy** endpoints only (falls back to all if none healthy)
2. **Region match** (if you set a preferred region)
3. **Lowest latency** (from recorded probes)
4. **Highest uptime** (successful probes / total probes)

```rust
// Get the best endpoint
let best = registry.best_endpoint();
println!("Using: {} ({}, {}ms)", best.url, best.provider, best.avg_latency_ms);

// Build an engine from it
let engine = engine_from_registry(&registry);
```

### 6.3 Health monitoring

```rust
// Record probe results (call this from your health-check loop)
registry.record_probe("https://mainnet.block-engine.jito.wtf", 45, true);
registry.record_probe("https://ny.mainnet.block-engine.jito.wtf", 120, false);

// Manual override
registry.mark_unhealthy("https://broken-endpoint.example.com");
registry.mark_healthy("https://recovered-endpoint.example.com");

// Get a summary
let summary = registry.summary();
println!("{} endpoints, {} healthy", summary.total, summary.healthy);
```

### 6.4 Inspecting endpoints

```rust
// All endpoints
for ep in registry.endpoints() {
    println!("{} | {} | healthy={} | latency={}ms | uptime={:.0}%",
        ep.provider, ep.url, ep.healthy, ep.avg_latency_ms, ep.uptime() * 100.0);
}

// Filter by provider
let helius_eps = registry.endpoints_for(BamProvider::Helius);
```

---

## 7. Tip Strategies

### 7.1 Static tips

The simplest approach — just pick a fixed number:

```rust
use jito_bam_client::tip::{build_tip_instruction, random_tip_account, MIN_TIP_LAMPORTS};

// Build a tip instruction manually
let tip_ix = build_tip_instruction(
    &payer.pubkey(),
    50_000,    // 0.00005 SOL
    None,      // random tip account
)?;

// Or specify a tip account
let tip_account = random_tip_account()?;
let tip_ix = build_tip_instruction(
    &payer.pubkey(),
    50_000,
    Some(tip_account),
)?;
```

**When to use**: Testing, low-frequency trades, stable market conditions.

**Typical values**:

| Scenario | Tip (lamports) | Tip (SOL) |
|---|---|---|
| Minimum | 1,000 | 0.000001 |
| Low priority | 10,000 | 0.00001 |
| Normal | 50,000 | 0.00005 |
| Competitive | 500,000 | 0.0005 |
| High priority | 5,000,000 | 0.005 |

### 7.2 Dynamic tips

Tracks recent landed bundle tips and recommends `median × 1.05`:

```rust
use jito_bam_client::dynamic_tip::DynamicTipCalculator;

// Create with a 20-sample rolling window
let mut calc = DynamicTipCalculator::new(20);

// Seed with known tip values
calc.record_tip(30_000);
calc.record_tip(50_000);
calc.record_tip(45_000);

// Get the recommended tip
let tip = calc.recommended_tip(); // median(30k,45k,50k) × 1.05 = 47,250

// Track a bundle you submitted
calc.track_bundle("bundle-uuid-here", 50_000);

// Later, refresh from Jito to see what landed
let landed_count = calc.refresh_from_jito(&engine).await?;

// Get full statistics
let snap = calc.snapshot();
println!("Samples: {} | Median: {} | Recommended: {}",
    snap.sample_count, snap.median_tip, snap.recommended_tip);
```

#### Customizing the calculator

```rust
let mut calc = DynamicTipCalculator::custom(
    50,         // window size (50 samples)
    100_000,    // fallback tip (when no data)
    1.10,       // premium factor (median × 1.10)
    5_000,      // floor (minimum tip)
    10_000_000, // ceiling (maximum tip)
);

// Or adjust after creation
calc.set_premium_factor(1.15);
calc.set_fallback_tip(75_000);
calc.set_floor(10_000);
calc.set_ceiling(2_000_000);
```

### 7.3 Tip Market Monitor (autonomous)

The most advanced strategy. Runs a background task that:
- Polls `getBundleStatuses` every 5 seconds
- Builds a full percentile distribution (p25, p50, p75, p90, p99)
- Detects market volatility via spread ratio and tip acceleration
- Auto-escalates from `p50 × 1.05` to `p75 × 1.10` during volatile periods

```rust
use jito_bam_client::tip_market::{
    TipMonitorConfig, TipMarketState, MarketState,
    spawn_tip_monitor, seed_tip_market,
};
use std::time::Duration;

// Configure the monitor
let config = TipMonitorConfig {
    poll_interval: Duration::from_secs(5),     // poll every 5s
    sample_window: 100,                         // keep 100 data points
    volatility_ratio_threshold: 1.50,          // p75/p50 > 1.5 = volatile
    acceleration_threshold: 5_000.0,           // lamports/sample slope
    normal_premium: 1.05,                       // normal: median × 1.05
    volatile_premium: 1.10,                     // volatile: p75 × 1.10
    floor: 1_000,                               // minimum tip
    ceiling: 5_000_000_000,                     // maximum tip (5 SOL)
    fallback_tip: 50_000,                       // initial tip before data
};

// Spawn the monitor (takes ownership of the engine)
let monitor_engine = engine_from_registry(&registry);
let tip_state = spawn_tip_monitor(monitor_engine, config);

// Optionally seed with historical data for instant warm-up
seed_tip_market(&tip_state, &[30_000, 40_000, 50_000, 60_000, 70_000]);
```

#### Using the recommendation in your trade loop

```rust
loop {
    let rec = tip_state.recommend();

    println!("State: {} | Tip: {} lamports | Base: {} | Premium: {:.2}×",
        rec.market_state, rec.tip_lamports,
        rec.base_percentile, rec.premium_factor);

    if rec.escalated {
        println!("⚡ VOLATILE — using p75 escalation!");
    }

    // Use the recommended tip in your trade
    let confirmation = engine
        .execute_and_confirm(&[swap_ix], &payer, rec.tip_lamports)
        .await?;

    tokio::time::sleep(Duration::from_secs(1)).await;
}
```

#### Reading the distribution

```rust
let dist = tip_state.distribution();
println!("Tip distribution ({} samples):", dist.sample_count);
println!("  Min: {}", dist.min);
println!("  p25: {}", dist.p25);
println!("  p50: {} (median)", dist.p50);
println!("  p75: {}", dist.p75);
println!("  p90: {}", dist.p90);
println!("  p99: {}", dist.p99);
println!("  Max: {}", dist.max);
println!("  Spread: {:.2}×", dist.spread_ratio());
```

#### Forcing volatile mode

During known high-volatility events (token launches, liquidation cascades):

```rust
// Force p75 escalation regardless of detected state
tip_state.force_volatile(true);

// Resume normal detection
tip_state.force_volatile(false);
```

---

## 8. Raydium CLMM Swaps

Build Raydium Concentrated Liquidity Market Maker swaps with automatic
tick-array derivation, output estimation, and Jito tip inclusion.

### 8.1 Full swap flow

```rust
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use jito_bam_client::raydium_clmm::{
    RaydiumClmmSwapBuilder, ClmmSwapConfig,
    apply_slippage, derive_ata, tick_to_price,
    DEFAULT_SLIPPAGE_BPS, TOKEN_PROGRAM_ID,
};
use jito_bam_client::engine::PrivateExecutionEngine;

let rpc = engine.rpc_client();
let payer = load_keypair();
let pool_address: Pubkey = "YourPoolAddress...".parse().unwrap();

// 1. Create the swap builder
let builder = RaydiumClmmSwapBuilder::new(rpc);

// 2. Fetch on-chain pool state
let pool_state = builder.fetch_pool_state(&pool_address).await?;
println!("Pool: {} → {}", pool_state.token_mint_0, pool_state.token_mint_1);
println!("Current tick: {}", pool_state.tick_current);
println!("Price: {:.6}", tick_to_price(pool_state.tick_current));

// 3. Estimate output
let amount_in = 1_000_000; // 1 token (adjust for decimals)
let a_to_b = true;         // swap token_0 → token_1
let expected_out = RaydiumClmmSwapBuilder::estimate_output(
    &pool_state, amount_in, a_to_b,
)?;
println!("Expected output: {expected_out}");

// 4. Derive user token accounts
let user_input  = derive_ata(&payer.pubkey(), &pool_state.token_mint_0, &TOKEN_PROGRAM_ID);
let user_output = derive_ata(&payer.pubkey(), &pool_state.token_mint_1, &TOKEN_PROGRAM_ID);

// 5. Configure the swap
let swap_config = ClmmSwapConfig {
    pool_address,
    amount_in,
    a_to_b,
    slippage_bps: 50,                     // 0.5% slippage tolerance
    user_input_token: user_input,
    user_output_token: user_output,
    user_authority: payer.pubkey(),
    lookup_table_addresses: vec![],       // ALTs for compressed txs
    tip_lamports: 100_000,                // Jito tip
    tip_account: None,                    // random tip account
};

// 6. Build the full versioned transaction
let blockhash = engine.get_latest_blockhash().await?;
let swap = builder
    .build_versioned_transaction(&swap_config, &payer, blockhash)
    .await?;

println!("Min output: {}", swap.min_amount_out);
println!("Expected output: {}", swap.expected_amount_out);

// 7. Send as a bundle
let uuid = engine.send_bundle(vec![swap.transaction]).await?;
let status = engine.poll_bundle_status(&uuid).await?;
```

### 8.2 Just get the instruction (no tx building)

```rust
let swap_output = builder
    .build_swap_instruction(&swap_config)
    .await?;

// Use the instruction in your own transaction
let ix = swap_output.swap_instruction;
let min_out = swap_output.min_amount_out;
```

### 8.3 Utility functions

```rust
// Slippage calculation
let min_out = apply_slippage(1_000_000, 50); // 0.5% → 995,000

// Tick to human-readable price
let price = tick_to_price(100);  // 1.0001^100 ≈ 1.01005

// Sqrt price to human price (with decimal adjustment)
let price = sqrt_price_x64_to_price(
    pool_state.sqrt_price_x64,
    pool_state.mint_decimals_0,
    pool_state.mint_decimals_1,
);

// Derive tick arrays for the swap
let tick_arrays = RaydiumClmmSwapBuilder::derive_tick_arrays(
    &pool_address,
    pool_state.tick_current,
    pool_state.tick_spacing,
    true, // a_to_b
);

// Derive associated token account
let ata = derive_ata(&owner, &mint, &TOKEN_PROGRAM_ID);
```

### 8.4 Key constants

| Constant | Value | Purpose |
|---|---|---|
| `RAYDIUM_CLMM_PROGRAM_ID` | `CAMMCzo5YL...` | Main CLMM program |
| `DEFAULT_SLIPPAGE_BPS` | 50 | 0.5% default slippage |
| `MAX_SLIPPAGE_BPS` | 1000 | 10% max slippage |
| `TICK_ARRAY_SIZE` | 60 | Ticks per array |
| `MIN_SQRT_PRICE_X64` | 4295048016 | Lower price bound |
| `MAX_SQRT_PRICE_X64` | 79226673515401279992447579055 | Upper price bound |

---

## 9. Meteora DLMM Swaps

Build Meteora Dynamic Liquidity Market Maker swaps with bin-walk
simulation and on-chain pool resolution.

### 9.1 Full swap flow

```rust
use jito_bam_client::meteora_dlmm::{
    DlmmSwapBuilder, DlmmSwapConfig,
    bin_id_to_price, apply_slippage, derive_ata,
};

let rpc = engine.rpc_client();
let payer = load_keypair();
let lb_pair: Pubkey = "YourDlmmPoolAddress...".parse().unwrap();

// 1. Create the builder
let builder = DlmmSwapBuilder::new(rpc);

// 2. Fetch and resolve pool state (bins, bin arrays, event authority)
let resolved = builder.fetch_pool_state(&lb_pair).await?;
println!("Active bin: {} (price: {:.6})",
    resolved.state.active_id,
    bin_id_to_price(resolved.state.active_id, resolved.state.bin_step),
);

// 3. Derive user token accounts
let user_token_x = derive_ata(
    &payer.pubkey(),
    &resolved.state.token_x_mint,
    &resolved.state.token_x_program,
);
let user_token_y = derive_ata(
    &payer.pubkey(),
    &resolved.state.token_y_mint,
    &resolved.state.token_y_program,
);

// 4. Configure the swap
let config = DlmmSwapConfig {
    lb_pair,
    amount_in: 1_000_000,
    swap_for_y: true,                  // X → Y
    slippage_bps: 50,                  // 0.5%
    user_token_x,
    user_token_y,
    user_authority: payer.pubkey(),
    host_fee_in: None,
    lookup_table_addresses: vec![],
};

// 5. Build the swap
let output = builder.build(&config).await?;
println!("Expected out: {} | Min out: {} | Bin price: {:.6}",
    output.expected_amount_out, output.min_amount_out, output.active_bin_price);

// 6. Use the instruction
engine.execute_and_confirm(
    &[output.instruction],
    &payer,
    100_000, // tip
).await?;
```

### 9.2 Key constants

| Constant | Value | Purpose |
|---|---|---|
| `DLMM_PROGRAM_ID` | `LBUZKhRx...` | Meteora DLMM program |
| `DEFAULT_SLIPPAGE_BPS` | 50 | 0.5% |
| `MAX_SLIPPAGE_BPS` | 1000 | 10% |
| `BIN_ARRAY_SIZE` | 70 | Bins per array |

---

## 10. ACE — Conditional Execution

The Autonomous Conditional Execution (ACE) framework lets you define
triggers that fire when market conditions are met.

### 10.1 Creating a trigger

```rust
use jito_bam_client::ace::*;
use std::time::Duration;

// Price move trigger: fire when SOL moves 50+ bps in 200ms
let trigger = TriggerCondition::PriceMove(PriceMoveTrigger {
    feed_id: "SOL/USD".to_string(),
    threshold_bps: 50,
    window_ms: 200,
    direction: None, // fire on either direction
});

// Or restrict to upward moves only
let up_trigger = TriggerCondition::PriceMove(PriceMoveTrigger {
    feed_id: "SOL/USD".to_string(),
    threshold_bps: 30,
    window_ms: 500,
    direction: Some(PriceDirection::Up),
});

// Price level trigger: fire when SOL > $200
let level_trigger = TriggerCondition::PriceLevel(PriceLevelTrigger {
    feed_id: "SOL/USD".to_string(),
    level: 200_000_000, // price in micro-cents or feed units
    above: true,
});

// Timer trigger: fire every 10 seconds
let timer = TriggerCondition::Timer(TimerTrigger {
    interval_ms: 10_000,
});

// Always-fire trigger (testing)
let always = TriggerCondition::Always;
```

### 10.2 Composite triggers

```rust
// AND: both conditions must be true
let and_trigger = CompositeTrigger::And(vec![
    CompositeTrigger::Single(TriggerCondition::PriceMove(PriceMoveTrigger {
        feed_id: "SOL/USD".to_string(),
        threshold_bps: 50,
        window_ms: 200,
        direction: Some(PriceDirection::Up),
    })),
    CompositeTrigger::Single(TriggerCondition::PriceLevel(PriceLevelTrigger {
        feed_id: "SOL/USD".to_string(),
        level: 150_000_000,
        above: true,
    })),
]);

// OR: either condition triggers
let or_trigger = CompositeTrigger::Or(vec![
    CompositeTrigger::Single(TriggerCondition::Timer(TimerTrigger {
        interval_ms: 60_000,
    })),
    CompositeTrigger::Single(TriggerCondition::PriceMove(PriceMoveTrigger {
        feed_id: "SOL/USD".to_string(),
        threshold_bps: 100,
        window_ms: 1000,
        direction: None,
    })),
]);
```

### 10.3 Building an ACE plugin

```rust
let mut plugin = AcePlugin::builder()
    .plugin_id("sol-momentum-sniper")
    .label("SOL Momentum")
    .trigger(TriggerCondition::PriceMove(PriceMoveTrigger {
        feed_id: "SOL/USD".to_string(),
        threshold_bps: 50,
        window_ms: 200,
        direction: None,
    }))
    .placement(OrderPlacement::TopOfBlock)
    .tip_lamports(100_000)
    .max_fires_per_minute(6)
    .build();
```

### 10.4 Running the trigger engine

```rust
let mut trigger_engine = TriggerEngine::new(2000); // 2000-sample price window

// Feed prices (from Pyth, Switchboard, or your own oracle)
trigger_engine.record_price("SOL/USD", 160_500_000, 50_000, timestamp_ms);

// Evaluate the plugin
let eval = plugin.should_fire(&mut trigger_engine);
if eval.fired {
    println!("TRIGGERED: {}", eval.reason);
    if let Some(bps) = eval.observed_bps {
        println!("  Move: {:.1} bps {:?}", bps, eval.observed_direction);
    }

    // Execute your trade!
    engine.execute_and_confirm(&[swap_ix], &payer, plugin.config.tip_lamports).await?;
}
```

### 10.5 Price window utilities

```rust
let mut window = PriceWindow::new(1000);

window.push(PriceSnapshot {
    price: 160_500_000,
    confidence: 50_000,
    timestamp: Instant::now(),
    oracle_timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
});

// Check price movement over a 200ms window
if let Some((bps, direction)) = window.move_bps(Duration::from_millis(200)) {
    println!("Price moved {:.1} bps {}", bps, direction);
}
```

---

## 11. TEE Attestation Verification

Verify that your transactions were actually included in a block by
validating TEE (Trusted Execution Environment) attestation proofs.

### 11.1 Full verification pipeline

```rust
use jito_bam_client::attestation::*;

// You receive these from Jito's attestation service
let attestation = InclusionAttestation {
    enclave_pubkey: [/* 32 bytes */],
    slot: 250_000_000,
    merkle_root: [/* 32 bytes */],
    signature: [/* 64 bytes */],
    tee_measurement: "a1b2c3d4...".to_string(),
    timestamp: chrono::Utc::now().timestamp() as u64,
};

let proof = InclusionProof {
    tx_hash: hash_transaction(&your_tx_bytes),
    position: 0,
    total_txs: 5,
    proof: vec![/* merkle sibling hashes */],
};

let config = VerifierConfig {
    expected_tee_measurement: "a1b2c3d4...".to_string(),
    max_attestation_age_secs: 60,
};

// Run all 5 checks
let result = verify_inclusion_integrity(
    &attestation,
    &proof,
    0, // expected position
    &config,
);

if result.is_valid() {
    println!("✅ Inclusion verified at position {}", result.verified_position.unwrap());
} else {
    println!("❌ Verification failed: {}", result.summary);
}
```

### 11.2 Individual checks

```rust
// 1. Ed25519 signature
let sig_valid = verify_attestation_signature(&attestation)?;

// 2. TEE measurement
let tee_valid = verify_tee_measurement(&attestation, "expected-measurement");

// 3. Freshness
let fresh = verify_attestation_freshness(&attestation, now_unix, 60);

// 4. Merkle inclusion proof
let proof_valid = verify_inclusion_proof(&attestation, &proof)?;
```

### 11.3 Building proofs (for testing)

```rust
// Hash transactions
let hashes: Vec<[u8; 32]> = transactions
    .iter()
    .map(|tx| hash_transaction(tx))
    .collect();

// Compute merkle root
let root = compute_merkle_root(&hashes);

// Build proof for transaction at index 2
let proof_path = build_merkle_proof(&hashes, 2);
```

---

## 12. Building a Complete Trading Bot

Here's how to wire everything together into a production sniper bot.

### 12.1 The complete pattern

```rust
use jito_bam_client::bam_rpc::*;
use jito_bam_client::tip_market::*;
use jito_bam_client::raydium_clmm::*;
use jito_bam_client::engine::PrivateExecutionEngine;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // ═══════════════════════════════════════════════════
    // STEP 1: Set up BAM RPC endpoints
    // ═══════════════════════════════════════════════════
    let mut registry = BamEndpointRegistry::with_region(Region::UsEast);
    registry.add_jito_native(None);
    registry.add_from_env(BamProvider::Helius, "HELIUS_API_KEY");

    // Create two engines: one for the monitor, one for trading
    let monitor_engine = engine_from_registry(&registry);
    let trade_engine = engine_from_registry(&registry);

    // ═══════════════════════════════════════════════════
    // STEP 2: Start the tip market monitor
    // ═══════════════════════════════════════════════════
    let tip_state = spawn_tip_monitor(
        monitor_engine,
        TipMonitorConfig::default(),
    );
    seed_tip_market(&tip_state, &[30_000, 50_000, 70_000, 50_000, 60_000]);

    // ═══════════════════════════════════════════════════
    // STEP 3: Load wallet
    // ═══════════════════════════════════════════════════
    let payer = load_keypair();
    let pool: Pubkey = "YourTargetPool...".parse()?;

    // ═══════════════════════════════════════════════════
    // STEP 4: Trading loop
    // ═══════════════════════════════════════════════════
    let rpc = trade_engine.rpc_client();
    let builder = RaydiumClmmSwapBuilder::new(rpc);

    loop {
        // Get the current market-aware tip
        let rec = tip_state.recommend();

        // Fetch fresh pool state
        let pool_state = builder.fetch_pool_state(&pool).await?;

        // Check if the trade is profitable (your logic here)
        let amount_in = 1_000_000_000; // 1 SOL
        let expected_out = RaydiumClmmSwapBuilder::estimate_output(
            &pool_state, amount_in, true,
        )?;

        // Only trade if output meets your threshold
        if expected_out > your_minimum_output {
            let user_in  = derive_ata(&payer.pubkey(), &pool_state.token_mint_0, &TOKEN_PROGRAM_ID);
            let user_out = derive_ata(&payer.pubkey(), &pool_state.token_mint_1, &TOKEN_PROGRAM_ID);

            let swap_config = ClmmSwapConfig {
                pool_address: pool,
                amount_in,
                a_to_b: true,
                slippage_bps: 50,
                user_input_token: user_in,
                user_output_token: user_out,
                user_authority: payer.pubkey(),
                lookup_table_addresses: vec![],
                tip_lamports: rec.tip_lamports,  // ← market-aware tip!
                tip_account: None,
            };

            let blockhash = trade_engine.get_latest_blockhash().await?;
            let swap = builder
                .build_versioned_transaction(&swap_config, &payer, blockhash)
                .await?;

            let uuid = trade_engine.send_bundle(vec![swap.transaction]).await?;
            let status = trade_engine.poll_bundle_status(&uuid).await?;

            match status.status {
                InFlightStatus::Landed => println!("✅ Landed!"),
                InFlightStatus::Failed => println!("❌ Failed: {:?}", status.error),
                _ => println!("⏳ Pending..."),
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
```

### 12.2 Key production considerations

| Concern | Recommendation |
|---|---|
| **Keypair security** | Load from encrypted file or HSM; never hardcode |
| **RPC rate limits** | Use paid RPC (Helius/Triton/QuickNode); monitor 429s |
| **Error handling** | Wrap each trade in `match`; don't crash the loop |
| **Circuit breaker** | Stop after N consecutive failures |
| **Logging** | Use `tracing` with structured fields; export to monitoring |
| **Tip ceiling** | Always set a ceiling to prevent runaway tips |
| **Slippage** | Start conservative (50 bps); tighten as you learn pools |
| **Blockhash freshness** | Fetch a new blockhash before each trade |
| **Fund management** | Keep only trading capital in the hot wallet |

---

## 13. Configuration Reference

### 13.1 `EngineConfig`

| Field | Type | Default | Description |
|---|---|---|---|
| `jito_url` | `String` | Mainnet block-engine | Jito JSON-RPC URL |
| `uuid` | `Option<String>` | `None` | Auth UUID for Jito |
| `rpc_url` | `String` | `api.mainnet-beta.solana.com` | Solana RPC URL |
| `max_status_polls` | `u32` | 12 | Max bundle status polls |
| `poll_interval` | `Duration` | 2s | Delay between polls |
| `default_tip_lamports` | `u64` | 50,000 | Default tip if none specified |

### 13.2 `TipMonitorConfig`

| Field | Type | Default | Description |
|---|---|---|---|
| `poll_interval` | `Duration` | 5s | How often to query Jito |
| `sample_window` | `usize` | 100 | Rolling window size |
| `volatility_ratio_threshold` | `f64` | 1.50 | p75/p50 ratio for volatile |
| `acceleration_threshold` | `f64` | 5,000.0 | Tip slope (lamports/sample) |
| `normal_premium` | `f64` | 1.05 | Premium in normal mode |
| `volatile_premium` | `f64` | 1.10 | Premium in volatile mode |
| `floor` | `u64` | 1,000 | Minimum tip |
| `ceiling` | `u64` | 5,000,000,000 | Maximum tip (5 SOL) |
| `fallback_tip` | `u64` | 50,000 | Tip when no data |

### 13.3 `BamClientConfig` (gRPC path)

| Field | Type | Default | Description |
|---|---|---|---|
| `endpoint` | `String` | Mainnet | gRPC endpoint |
| `tls_ca_cert` | `Option<Vec<u8>>` | `None` | Custom TLS cert |
| `expected_tee_measurement` | `Option<String>` | `None` | TEE verification |
| `timeout` | `Duration` | 30s | gRPC timeout |
| `max_retries` | `u32` | 3 | Retry count |
| `retry_base_delay` | `Duration` | 500ms | Exponential backoff base |
| `auth_token` | `Option<String>` | `None` | Bearer token |

---

## 14. Examples

Run any example with:

```bash
cargo run --example <name>
```

| Example | File | Description |
|---|---|---|
| `live_pipeline` | `examples/live_pipeline.rs` | Full operational demo: BAM registry → tip monitor → trade loop |
| `raydium_clmm_sniper` | `examples/raydium_clmm_sniper.rs` | Raydium CLMM sniper with dynamic tips |
| `dlmm_sniper` | `examples/dlmm_sniper.rs` | Meteora DLMM sniper with circuit breaker |
| `ace_sol_momentum` | `examples/ace_sol_momentum.rs` | ACE conditional execution with simulated Oracle |
| `engine_bot_loop` | `examples/engine_bot_loop.rs` | Simple engine-based trading loop |
| `bot_loop` | `examples/bot_loop.rs` | SDK-level bot loop (gRPC path) |
| `inclusion_monitor` | `examples/inclusion_monitor.rs` | TEE attestation verification monitor |

### Recommended learning path

1. **Start here** → `engine_bot_loop` (simplest engine usage)
2. **Add tips** → `raydium_clmm_sniper` (dynamic tips + real swap building)
3. **Go production** → `live_pipeline` (multi-provider + tip monitor)
4. **Add triggers** → `ace_sol_momentum` (conditional execution)

---

## 15. Troubleshooting

### Common errors

| Error | Cause | Fix |
|---|---|---|
| `BundleNotLanded { attempts: 12 }` | Bundle didn't land within polling window | Increase tip or `max_status_polls` |
| `NoTipAccounts` | Couldn't fetch Jito tip accounts | Check RPC connectivity |
| `BlockhashFetch` | RPC is down or rate-limited | Switch RPC provider |
| `BundleRejected("...")` | Jito rejected the bundle | Check tx validity; ensure sufficient SOL |
| `SlippageExceeded` | Output was less than min | Increase `slippage_bps` or reduce `amount_in` |
| `TransactionTooLarge` | Tx exceeds 1232 bytes | Use Address Lookup Tables (ALTs) |
| `RpcError("429")` | Rate limited | Use a paid RPC; add backoff |

### Tips that are too low

If bundles aren't landing:

```rust
// Check what the market is paying
let dist = tip_state.distribution();
println!("Market tips: p50={} p75={} p90={}", dist.p50, dist.p75, dist.p90);

// Force volatile mode for higher tips
tip_state.force_volatile(true);
```

### Debugging bundle status

```rust
let confirmation = engine.poll_bundle_status(&uuid).await?;
println!("Status: {:?}", confirmation.status);
println!("Slot: {:?}", confirmation.slot);
println!("Transactions: {:?}", confirmation.transactions);
println!("Error: {:?}", confirmation.error);
```

### Enabling detailed logs

```bash
RUST_LOG=debug cargo run --example live_pipeline
# or for specific modules:
RUST_LOG=jito_bam_client::tip_market=debug,jito_bam_client::engine=info cargo run
```

---

## 16. API Quick Reference

### Engine (everyday use)

```rust
PrivateExecutionEngine::mainnet(rpc_url)
engine.execute(&[ixs], &payer, tip)
engine.execute_and_confirm(&[ixs], &payer, tip)
engine.execute_with_signers(&[ixs], &payer, &[&signer], tip)
engine.send_bundle(vec![tx])
engine.send_transaction(&tx, skip_preflight)
engine.poll_bundle_status(&uuid)
engine.get_tip_accounts()
engine.get_random_tip_account()
engine.get_latest_blockhash()
```

### BAM RPC (multi-provider)

```rust
BamEndpointRegistry::with_region(Region::UsEast)
registry.add_jito_native(None)
registry.add_from_env(BamProvider::Helius, "HELIUS_API_KEY")
registry.best_endpoint()
engine_from_registry(&registry)
```

### Tip Market (autonomous)

```rust
spawn_tip_monitor(engine, TipMonitorConfig::default())
tip_state.recommend()        // → TipRecommendation
tip_state.distribution()     // → TipDistribution
tip_state.market_state()     // → Normal | Volatile | Initializing
tip_state.force_volatile(true)
seed_tip_market(&state, &[tips])
```

### Dynamic Tips (manual)

```rust
DynamicTipCalculator::new(window_size)
calc.record_tip(lamports)
calc.track_bundle(&uuid, lamports)
calc.refresh_from_jito(&engine).await
calc.recommended_tip()
calc.snapshot()
```

### Raydium CLMM

```rust
RaydiumClmmSwapBuilder::new(rpc)
builder.fetch_pool_state(&pool).await
RaydiumClmmSwapBuilder::estimate_output(&state, amount, a_to_b)
builder.build_versioned_transaction(&config, &payer, blockhash).await
apply_slippage(amount, bps)
derive_ata(&owner, &mint, &token_program)
tick_to_price(tick)
```

### Meteora DLMM

```rust
DlmmSwapBuilder::new(rpc)
builder.fetch_pool_state(&lb_pair).await
builder.build(&config).await
bin_id_to_price(bin_id, bin_step)
apply_slippage(amount, bps)
```

### ACE Triggers

```rust
AcePlugin::builder().plugin_id("x").trigger(cond).placement(TopOfBlock).build()
TriggerEngine::new(capacity)
engine.record_price(feed, price, confidence, ts)
plugin.should_fire(&mut engine)
```

### Static Tips

```rust
build_tip_instruction(&payer, lamports, None)
random_tip_account()
```

### Transaction Builder

```rust
build_tipped_versioned_transaction(&ixs, &payer, &signers, blockhash, &config)
```

---

## License

MIT

---

*Built with jito-sdk-rust 0.3.2, solana-sdk 2.1, and a lot of caffeine.*
