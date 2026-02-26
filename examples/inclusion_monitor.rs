/// Inclusion-integrity monitoring script for Jito BAM.
///
/// This example demonstrates a continuous monitoring loop that:
///
///   1. Submits a transaction bundle via BAM (private, TEE-encrypted)
///   2. Polls for landing confirmation
///   3. Fetches the BAM attestation for the slot
///   4. Verifies the cryptographic proof that the validator didn't cheat:
///      a) Ed25519 signature over slot + Merkle root — proves TEE produced it
///      b) TEE measurement (MRENCLAVE / MRTD) — proves enclave is unmodified
///      c) Freshness — rejects stale / replayed attestations
///      d) Merkle inclusion proof — proves your tx is in the block
///      e) Position check — proves your tx wasn't reordered (front-run)
///
/// # Running
/// ```bash
/// RUST_LOG=info cargo run --example inclusion_monitor
/// ```
///
/// In production, integrate this into your bot's post-trade pipeline.
/// Any verification failure should trigger an alert — it means the
/// validator may have inspected or reordered your transaction despite
/// the TEE guarantee.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};

use tracing::{error, info, warn};

use jito_bam_client::attestation::{
    build_attestation_payload, build_merkle_proof, compute_merkle_root,
    hash_transaction, verify_inclusion_integrity, InclusionAttestation,
    InclusionProof, VerificationResult, VerifierConfig,
};

// ═══════════════════════════════════════════════════════════════════════════
//  Configuration
// ═══════════════════════════════════════════════════════════════════════════

/// Expected TEE measurement — in production, obtain this from Jito's
/// published enclave builds.  Set to `None` to skip the check during
/// development.
const EXPECTED_TEE_MEASUREMENT: Option<&str> = Some("aabbccdd");

/// Maximum attestation age before we consider it stale.
const MAX_ATTESTATION_AGE_SECS: u64 = 120;

/// How many recent trades to keep in the audit log.
const AUDIT_LOG_CAPACITY: usize = 1_000;

// ═══════════════════════════════════════════════════════════════════════════
//  Monitoring state
// ═══════════════════════════════════════════════════════════════════════════

/// Audit record for one verified trade.
#[derive(Clone, Debug)]
struct AuditRecord {
    slot: u64,
    position: u32,
    result: VerificationResult,
    checked_at: u64,
}

/// Running statistics across all monitored trades.
#[derive(Debug, Default)]
struct MonitorStats {
    total_checked: u64,
    total_passed: u64,
    total_failed: u64,
    sig_failures: u64,
    tee_failures: u64,
    freshness_failures: u64,
    merkle_failures: u64,
    position_failures: u64,
}

impl MonitorStats {
    fn record(&mut self, result: &VerificationResult) {
        self.total_checked += 1;
        if result.is_valid() {
            self.total_passed += 1;
        } else {
            self.total_failed += 1;
            if !result.signature_valid {
                self.sig_failures += 1;
            }
            if !result.tee_measurement_valid {
                self.tee_failures += 1;
            }
            if !result.attestation_fresh {
                self.freshness_failures += 1;
            }
            if !result.merkle_proof_valid {
                self.merkle_failures += 1;
            }
            if !result.position_verified {
                self.position_failures += 1;
            }
        }
    }

    fn pass_rate(&self) -> f64 {
        if self.total_checked == 0 {
            return 100.0;
        }
        (self.total_passed as f64 / self.total_checked as f64) * 100.0
    }
}

impl std::fmt::Display for MonitorStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "checked={} passed={} failed={} rate={:.1}% \
             [sig_fail={} tee_fail={} stale={} merkle_fail={} pos_fail={}]",
            self.total_checked,
            self.total_passed,
            self.total_failed,
            self.pass_rate(),
            self.sig_failures,
            self.tee_failures,
            self.freshness_failures,
            self.merkle_failures,
            self.position_failures,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Simulated BAM attestation fetcher
// ═══════════════════════════════════════════════════════════════════════════

/// In production, replace this with an actual gRPC call to the BAM node's
/// `GetAttestation` endpoint.  The node returns the attestation for a
/// specific slot + bundle ID.
///
/// This simulation creates a valid attestation with a proper Ed25519
/// signature and Merkle tree so the verification pipeline can be exercised
/// end-to-end.
fn simulate_bam_attestation(
    slot: u64,
    tx_bytes_list: &[Vec<u8>],
    your_tx_index: usize,
) -> (InclusionAttestation, InclusionProof) {
    // Hash all "transactions" in the block
    let tx_hashes: Vec<[u8; 32]> = tx_bytes_list
        .iter()
        .map(|b| hash_transaction(b))
        .collect();

    let merkle_root = compute_merkle_root(&tx_hashes);
    let proof_siblings = build_merkle_proof(&tx_hashes, your_tx_index);

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // TEE's Ed25519 key (deterministic for demo — in production this is
    // generated inside the enclave and the public key is in the
    // attestation report).
    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let verifying_key = signing_key.verifying_key();

    let payload = build_attestation_payload(slot, &merkle_root, now_secs);
    let signature = signing_key.sign(&payload);

    let attestation = InclusionAttestation {
        enclave_pubkey: verifying_key.to_bytes().to_vec(),
        slot,
        merkle_root,
        signature: signature.to_bytes().to_vec(),
        tee_measurement: "aabbccdd".into(),
        timestamp: now_secs,
    };

    let proof = InclusionProof {
        tx_hash: tx_hashes[your_tx_index],
        position: your_tx_index as u32,
        total_txs: tx_hashes.len() as u32,
        proof: proof_siblings,
    };

    (attestation, proof)
}

/// Simulate a tampered attestation to demonstrate detection.
fn simulate_tampered_attestation(
    slot: u64,
    tx_bytes_list: &[Vec<u8>],
    your_tx_index: usize,
) -> (InclusionAttestation, InclusionProof) {
    let (att, mut proof) =
        simulate_bam_attestation(slot, tx_bytes_list, your_tx_index);

    // Simulate a front-running attack: the validator reordered your tx
    // from position 2 to position 0, but the Merkle proof still claims
    // position 2.  Our verifier will catch this.
    proof.position = if your_tx_index > 0 {
        your_tx_index as u32 - 1
    } else {
        your_tx_index as u32 + 1
    };

    (att, proof)
}

// ═══════════════════════════════════════════════════════════════════════════
//  Main monitoring loop
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,jito_bam_client=debug")
        .init();

    info!("╔═══════════════════════════════════════════════════════════╗");
    info!("║   Jito BAM Inclusion-Integrity Monitor                   ║");
    info!("╚═══════════════════════════════════════════════════════════╝");

    let config = VerifierConfig {
        expected_tee_measurement: EXPECTED_TEE_MEASUREMENT.map(String::from),
        max_attestation_age_secs: MAX_ATTESTATION_AGE_SECS,
    };

    let mut stats = MonitorStats::default();
    let mut audit_log: Vec<AuditRecord> = Vec::with_capacity(AUDIT_LOG_CAPACITY);

    // ── Simulate a sequence of trades to monitor ────────────────────────
    //
    // In production, this loop would:
    //   1. Listen for your landed bundles (from the bot execution loop)
    //   2. Fetch the attestation from the BAM node
    //   3. Verify the cryptographic proof
    //   4. Alert on any failure

    let scenarios: Vec<(&str, u64, usize, bool)> = vec![
        ("Normal trade #1",       300_000_001, 2, false),
        ("Normal trade #2",       300_000_002, 0, false),
        ("Normal trade #3",       300_000_003, 3, false),
        ("Tampered trade (reorder)", 300_000_004, 2, true), // ← attack!
        ("Normal trade #4",       300_000_005, 1, false),
    ];

    for (label, slot, your_tx_index, tampered) in &scenarios {
        info!("─── Checking: {label} (slot {slot}) ───");

        // Simulate a block with 4 transactions, your tx at the given index
        let block_txs: Vec<Vec<u8>> = (0..4)
            .map(|i| format!("tx_slot{slot}_{i}").into_bytes())
            .collect();

        let (attestation, proof) = if *tampered {
            simulate_tampered_attestation(*slot, &block_txs, *your_tx_index)
        } else {
            simulate_bam_attestation(*slot, &block_txs, *your_tx_index)
        };

        // ── Run the full verification ───────────────────────────────────
        let result = verify_inclusion_integrity(
            &attestation,
            &proof,
            *your_tx_index as u32,
            &config,
        );

        // ── Record and report ───────────────────────────────────────────
        stats.record(&result);

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        audit_log.push(AuditRecord {
            slot: *slot,
            position: *your_tx_index as u32,
            result: result.clone(),
            checked_at: now_secs,
        });

        if result.is_valid() {
            info!(
                slot,
                position = your_tx_index,
                "  ✓ VERIFIED — validator did NOT cheat"
            );
        } else {
            error!(
                slot,
                position = your_tx_index,
                summary = %result.summary,
                "  ✗ ALERT — inclusion integrity violation detected!"
            );
            // In production: send webhook / PagerDuty / Telegram alert
            alert_on_violation(&result);
        }

        info!("  {result}");
        info!("  Stats: {stats}");
        info!("");

        // Small delay between checks (simulates real-time monitoring)
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ── Final summary ───────────────────────────────────────────────────

    info!("╔═══════════════════════════════════════════════════════════╗");
    info!("║   Monitoring Session Complete                            ║");
    info!("╚═══════════════════════════════════════════════════════════╝");
    info!("Final stats: {stats}");

    if stats.total_failed > 0 {
        warn!(
            "{} out of {} attestations FAILED verification!",
            stats.total_failed, stats.total_checked
        );
    } else {
        info!("All {} attestations passed — no cheating detected.", stats.total_checked);
    }

    // ── Print audit log ─────────────────────────────────────────────────

    info!("");
    info!("Audit log:");
    for (i, record) in audit_log.iter().enumerate() {
        let icon = if record.result.is_valid() { "✓" } else { "✗" };
        info!(
            "  [{icon}] #{i} slot={} pos={} at={}",
            record.slot, record.position, record.checked_at
        );
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
//  Alerting (stub — wire up to your monitoring infra)
// ═══════════════════════════════════════════════════════════════════════════

fn alert_on_violation(result: &VerificationResult) {
    // In production, replace with:
    //   - Webhook (Slack, Discord, Telegram)
    //   - PagerDuty incident
    //   - Log to SIEM / Datadog / Grafana
    //   - Automatic fund withdrawal as a safety measure
    error!(
        "🚨 INCLUSION INTEGRITY VIOLATION — slot={} summary={}",
        result.slot, result.summary
    );
    error!("   ➜ Possible validator front-run or transaction reordering");
    error!("   ➜ Review the block on an explorer and consider pausing the bot");
}
