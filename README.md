# jito-bam-client

A Rust SDK for building and submitting Solana transactions through [Jito's Block Awareness Module (BAM)](https://www.jito.wtf/) — bypassing the public mempool via TEE-encrypted private submission.

## Features

- **Private transaction submission** — route transactions through BAM Nodes instead of the public mempool
- **Bundle support** — submit up to 5 transactions as an atomic bundle via `send_bundle()`
- **Jito tip injection** — automatically append a tip instruction as the final instruction (required for BAM prioritisation)
- **V0 versioned transactions** — build modern Solana transactions with Address Lookup Table support
- **TEE attestation** — verify BAM Node Trusted Execution Environment reports
- **Exponential backoff retry** — automatic retries on transient gRPC/RPC errors
- **Bundle status polling** — poll for `Landed` / `Failed` confirmation after submission
- **High-level SDK facade** — `JitoBamSdk` provides a one-call `execute()` that builds, tips, submits, and confirms

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
jito-bam-client = { git = "https://github.com/Germinsky/jito-bam-client.git" }
```

### Simplest usage (SDK facade)

```rust
use jito_bam_client::sdk::JitoBamSdk;
use solana_sdk::{
    signature::Keypair, signer::Signer, system_instruction,
    pubkey::Pubkey, hash::Hash,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let sdk = JitoBamSdk::mainnet().await?;
    let payer = Keypair::new();

    let swap_ix = system_instruction::transfer(
        &payer.pubkey(),
        &Pubkey::new_unique(),
        1_000_000,
    );

    // Build tx + append Jito tip + submit bundle + poll confirmation
    let status = sdk.execute_and_confirm(
        &[swap_ix],
        &payer,
        50_000,                    // tip: 0.00005 SOL
        Hash::new_unique(),        // use a fresh blockhash
    ).await?;

    println!("Bundle status: {status}");
    Ok(())
}
```

### Low-level `BamClient` usage

```rust
use jito_bam_client::bam::BamClient;
use jito_bam_client::tip;
use jito_bam_client::tx_builder::{build_tipped_versioned_transaction, TxBuildConfig};
use solana_sdk::{signature::Keypair, signer::Signer, hash::Hash};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let payer = Keypair::new();

    // 1. Build a versioned transaction with a Jito tip as the final instruction
    let config = TxBuildConfig {
        payer: payer.pubkey(),
        instructions: vec![/* your swap / arb instructions */],
        tip_lamports: 50_000,
        recent_blockhash: Hash::new_unique(),
        address_lookup_tables: vec![],
    };
    let tx = build_tipped_versioned_transaction(&config, &[&payer])?;

    // 2. Connect to BAM and submit as a private bundle
    let bam = BamClient::new("https://mainnet.bam.jito.wtf").await?;
    let result = bam.send_bundle(vec![tx]).await?;

    println!("Bundle {} submitted", result.bundle_id);

    // 3. Poll for landing confirmation
    let status = bam.poll_bundle_status(&result.bundle_id).await?;
    println!("Final status: {status}");

    Ok(())
}
```

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                      Your Bot                           │
├─────────────────────────────────────────────────────────┤
│  sdk.rs    JitoBamSdk       High-level one-call facade  │
│  bam.rs    BamClient        gRPC client + retry logic   │
│  bundle.rs BundleResult     Bundle result / status types │
│  tip.rs    build_tip_ix     Jito tip instruction builder │
│  tx_builder.rs              V0 VersionedTx construction  │
│  error.rs  JitoBamError     Typed error hierarchy        │
└────────────────────┬────────────────────────────────────┘
                     │ gRPC over TLS
                     ▼
            ┌─────────────────┐
            │  Jito BAM Node  │
            │  (TEE enclave)  │
            └────────┬────────┘
                     │ private submission
                     ▼
            ┌─────────────────┐
            │  Solana Leader  │
            └─────────────────┘
```

## Modules

| Module | Description |
|--------|-------------|
| `bam` | `BamClient` — connects to a BAM Node over gRPC/TLS, submits bundles and single transactions with retry, polls bundle status |
| `sdk` | `JitoBamSdk` — high-level facade: `execute()`, `execute_and_confirm()`, `send_bundle()`, `tip_instruction()` |
| `bundle` | `BundleResult` and `BundleStatus` types (`Pending`, `Processing`, `Landed`, `Failed`) |
| `tip` | 8 hardcoded Jito tip accounts, `build_tip_instruction()`, `random_tip_account()` |
| `tx_builder` | `build_tipped_versioned_transaction()` — compiles V0 message with ALT support, appends tip, enforces 1232-byte limit |
| `error` | `JitoBamError` enum with variants for every failure mode |

## Examples

Run the bot execution loop example:

```bash
cargo run --example bot_loop
```

This demonstrates a complete MEV / trading bot loop: SDK setup, opportunity detection (placeholder), swap instruction building, private BAM submission, and bundle confirmation polling.

## Configuration

`BamClientConfig` fields:

| Field | Default | Description |
|-------|---------|-------------|
| `endpoint` | `https://mainnet.bam.jito.wtf` | BAM Node URL |
| `timeout` | 10s | gRPC call timeout |
| `max_retries` | 3 | Retry attempts on transient errors |
| `retry_base_delay` | 500ms | Initial backoff delay (doubles each retry) |
| `tee_verify` | `true` | Verify TEE attestation on connect |

## Well-known BAM endpoints

```rust
use jito_bam_client::bam::endpoints;

endpoints::MAINNET    // "https://mainnet.bam.jito.wtf"
endpoints::AMSTERDAM  // "https://amsterdam.bam.jito.wtf"
endpoints::FRANKFURT  // "https://frankfurt.bam.jito.wtf"
endpoints::NEW_YORK   // "https://ny.bam.jito.wtf"
endpoints::TOKYO      // "https://tokyo.bam.jito.wtf"
```

## Testing

```bash
cargo test
```

12 tests (8 unit + 4 doc-tests) covering tip construction, transaction building, bundle types, and API surface compilation.

## Dependencies

- **solana-sdk / solana-client** 2.1 — transaction building & signing
- **tonic** 0.12 — gRPC client with TLS
- **tokio** 1 — async runtime
- **tracing** — structured logging
- **thiserror** — error derive macros
- **serde / serde_json** — JSON serialisation for RPC payloads

## License

MIT
