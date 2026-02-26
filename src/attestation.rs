/// Inclusion-integrity attestation verification for Jito BAM.
///
/// BAM nodes running inside a TEE (Intel TDX / SGX) produce
/// cryptographic attestations that prove:
///
/// 1. **The transaction was included** in the block at the claimed slot.
/// 2. **The position** of the transaction within the block is correct
///    (no front-running / re-ordering by the validator).
/// 3. **The transaction was not inspected** — the TEE measurement
///    (MRENCLAVE / MRTD) proves the enclave code is unmodified.
///
/// # Verification flow
///
/// ```text
/// ┌─────────────────────────────────────────────────────┐
/// │  Attestation from BAM node                          │
/// │                                                     │
/// │  enclave_pubkey   — Ed25519 key bound to TEE        │
/// │  slot             — Solana slot                     │
/// │  merkle_root      — root of tx-Merkle tree          │
/// │  signature        — Ed25519 sig over the payload    │
/// │  tee_measurement  — MRENCLAVE / MRTD hex            │
/// │  timestamp        — UTC epoch seconds               │
/// └────────────────────┬────────────────────────────────┘
///                      │
///         ┌────────────▼──────────────┐
///         │  verify_attestation()     │
///         │  1. check sig             │
///         │  2. check TEE measurement │
///         │  3. check freshness       │
///         └────────────┬──────────────┘
///                      │
///         ┌────────────▼──────────────┐
///         │  verify_inclusion_proof() │
///         │  1. reconstruct root      │
///         │  2. compare against       │
///         │     attestation root      │
///         │  3. verify position index │
///         └───────────────────────────┘
/// ```
///
/// # Security considerations
///
/// - The enclave public key MUST be cross-checked against the TEE
///   attestation report.  A compromised node could fabricate keys outside
///   the enclave.
/// - Attestation freshness (`timestamp`) guards against replay.
/// - The Merkle proof covers the **serialised transaction bytes**, not
///   only the signature.  This prevents substitution attacks.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info, instrument, warn};

use crate::error::{JitoBamError, Result};

// ═══════════════════════════════════════════════════════════════════════════
//  Data types
// ═══════════════════════════════════════════════════════════════════════════

/// Attestation produced by a BAM node's TEE after a block is finalised.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InclusionAttestation {
    /// Ed25519 public key of the TEE enclave (32 bytes).
    pub enclave_pubkey: Vec<u8>,
    /// Solana slot in which the block was produced.
    pub slot: u64,
    /// Root of the Merkle tree over **all** transactions in the block.
    pub merkle_root: [u8; 32],
    /// Ed25519 signature over the canonical attestation payload.
    pub signature: Vec<u8>,
    /// MRENCLAVE (SGX) or MRTD (TDX) measurement hex of the enclave
    /// that produced this attestation.
    pub tee_measurement: String,
    /// UTC epoch seconds when the attestation was created.
    pub timestamp: u64,
}

/// Merkle inclusion proof that a specific transaction appears at a
/// specific position within the attested Merkle tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InclusionProof {
    /// SHA-256 hash of the **serialised transaction bytes**.
    pub tx_hash: [u8; 32],
    /// Zero-based index of the transaction within the block.
    pub position: u32,
    /// Total number of transactions in the block.
    pub total_txs: u32,
    /// Merkle proof siblings — one hash per tree level, bottom-up.
    pub proof: Vec<[u8; 32]>,
}

/// Result of a full inclusion-integrity verification.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerificationResult {
    /// Did the attestation signature check pass?
    pub signature_valid: bool,
    /// Does the TEE measurement match the expected value?
    pub tee_measurement_valid: bool,
    /// Is the attestation fresh (not expired)?
    pub attestation_fresh: bool,
    /// Does the Merkle proof re-derive the attested root?
    pub merkle_proof_valid: bool,
    /// Is the transaction position correct?
    pub position_verified: bool,
    /// The slot covered by this attestation.
    pub slot: u64,
    /// The verified transaction position (if valid).
    pub verified_position: Option<u32>,
    /// Human-readable summary.
    pub summary: String,
}

impl VerificationResult {
    /// All checks passed — the inclusion integrity is cryptographically
    /// confirmed.
    pub fn is_valid(&self) -> bool {
        self.signature_valid
            && self.tee_measurement_valid
            && self.attestation_fresh
            && self.merkle_proof_valid
            && self.position_verified
    }
}

impl std::fmt::Display for VerificationResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let icon = if self.is_valid() { "✓" } else { "✗" };
        write!(
            f,
            "[{icon}] slot={} pos={} sig={} tee={} fresh={} merkle={} pos_ok={}",
            self.slot,
            self.verified_position
                .map(|p| p.to_string())
                .unwrap_or_else(|| "?".into()),
            self.signature_valid,
            self.tee_measurement_valid,
            self.attestation_fresh,
            self.merkle_proof_valid,
            self.position_verified,
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Configuration
// ═══════════════════════════════════════════════════════════════════════════

/// Configuration for the attestation verifier.
#[derive(Clone, Debug)]
pub struct VerifierConfig {
    /// Expected MRENCLAVE / MRTD hex — if `None`, skip measurement check.
    pub expected_tee_measurement: Option<String>,
    /// Maximum allowed age of an attestation in seconds.
    pub max_attestation_age_secs: u64,
}

impl Default for VerifierConfig {
    fn default() -> Self {
        Self {
            expected_tee_measurement: None,
            max_attestation_age_secs: 120, // 2 minutes
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Core verification functions
// ═══════════════════════════════════════════════════════════════════════════

/// Verify the Ed25519 signature over the attestation payload.
///
/// The canonical payload is:
///   `SHA-256( slot_le_bytes || merkle_root || timestamp_le_bytes )`
///
/// The enclave signs this with its TEE-bound Ed25519 key.
#[instrument(skip_all, fields(slot = attestation.slot))]
pub fn verify_attestation_signature(
    attestation: &InclusionAttestation,
) -> Result<bool> {
    use ed25519_dalek::{Signature, VerifyingKey};

    // Parse enclave public key
    let pubkey_bytes: [u8; 32] = attestation
        .enclave_pubkey
        .as_slice()
        .try_into()
        .map_err(|_| {
            JitoBamError::AttestationSignatureInvalid(format!(
                "enclave pubkey must be 32 bytes, got {}",
                attestation.enclave_pubkey.len()
            ))
        })?;

    let verifying_key = VerifyingKey::from_bytes(&pubkey_bytes).map_err(|e| {
        JitoBamError::AttestationSignatureInvalid(format!("invalid enclave pubkey: {e}"))
    })?;

    // Parse signature
    let sig_bytes: [u8; 64] = attestation
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| {
            JitoBamError::AttestationSignatureInvalid(format!(
                "signature must be 64 bytes, got {}",
                attestation.signature.len()
            ))
        })?;

    let signature = Signature::from_bytes(&sig_bytes);

    // Build canonical payload
    let payload = build_attestation_payload(
        attestation.slot,
        &attestation.merkle_root,
        attestation.timestamp,
    );

    // Verify
    match verifying_key.verify_strict(&payload, &signature) {
        Ok(()) => {
            debug!(slot = attestation.slot, "attestation signature valid");
            Ok(true)
        }
        Err(e) => {
            warn!(slot = attestation.slot, error = %e, "attestation signature INVALID");
            Ok(false)
        }
    }
}

/// Verify the TEE measurement (MRENCLAVE / MRTD) against a known-good value.
pub fn verify_tee_measurement(
    attestation: &InclusionAttestation,
    expected: &str,
) -> bool {
    let normalised_actual = attestation.tee_measurement.to_lowercase();
    let normalised_expected = expected.to_lowercase();
    let matches = normalised_actual == normalised_expected;

    if !matches {
        warn!(
            expected = normalised_expected,
            actual = normalised_actual,
            "TEE measurement mismatch — enclave code may have changed"
        );
    }

    matches
}

/// Check that the attestation is not stale.
pub fn verify_attestation_freshness(
    attestation: &InclusionAttestation,
    max_age_secs: u64,
    now_epoch_secs: u64,
) -> bool {
    if attestation.timestamp > now_epoch_secs {
        warn!(
            attestation_ts = attestation.timestamp,
            now = now_epoch_secs,
            "attestation timestamp is in the future"
        );
        return false;
    }

    let age = now_epoch_secs - attestation.timestamp;

    if age > max_age_secs {
        warn!(age, max_age_secs, "attestation is stale");
        return false;
    }

    debug!(age, "attestation is fresh");
    true
}

// ═══════════════════════════════════════════════════════════════════════════
//  Merkle tree verification
// ═══════════════════════════════════════════════════════════════════════════

/// Verify a Merkle inclusion proof.
///
/// Given the transaction hash, position index, and sibling hashes,
/// recompute the Merkle root and compare it to the attested root.
///
/// This proves that the transaction at `proof.position` in the block
/// is exactly the one you submitted — no reordering, no substitution.
#[instrument(skip_all, fields(position = proof.position, total = proof.total_txs))]
pub fn verify_inclusion_proof(
    attestation: &InclusionAttestation,
    proof: &InclusionProof,
) -> Result<bool> {
    // Validate proof depth
    let expected_depth = if proof.total_txs <= 1 {
        0
    } else {
        (proof.total_txs as f64).log2().ceil() as usize
    };

    if proof.proof.len() != expected_depth {
        warn!(
            proof_len = proof.proof.len(),
            expected_depth,
            "merkle proof length mismatch"
        );
        return Err(JitoBamError::InvalidMerkleProofLength(proof.proof.len()));
    }

    // Walk up from the leaf
    let mut current_hash = proof.tx_hash;
    let mut index = proof.position;

    for (level, sibling) in proof.proof.iter().enumerate() {
        current_hash = if index % 2 == 0 {
            // Current node is a left child
            hash_pair(&current_hash, sibling)
        } else {
            // Current node is a right child
            hash_pair(sibling, &current_hash)
        };
        index /= 2;
        debug!(level, root_so_far = hex::encode(current_hash), "merkle step");
    }

    let root_matches = current_hash == attestation.merkle_root;

    if root_matches {
        debug!(
            position = proof.position,
            "merkle proof VALID — transaction is in the attested tree"
        );
    } else {
        warn!(
            expected_root = hex::encode(attestation.merkle_root),
            computed_root = hex::encode(current_hash),
            "merkle proof INVALID — root mismatch"
        );
    }

    Ok(root_matches)
}

/// Compute the SHA-256 hash of serialised transaction bytes.
///
/// This is the "leaf" value used in the Merkle tree.
pub fn hash_transaction(tx_bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(tx_bytes);
    hasher.finalize().into()
}

/// Build a Merkle tree from an ordered list of transaction hashes and
/// return the root.  Useful for constructing reference trees in tests.
pub fn compute_merkle_root(tx_hashes: &[[u8; 32]]) -> [u8; 32] {
    if tx_hashes.is_empty() {
        return [0u8; 32];
    }
    if tx_hashes.len() == 1 {
        return tx_hashes[0];
    }

    // Pad to next power of 2
    let n = tx_hashes.len().next_power_of_two();
    let mut layer: Vec<[u8; 32]> = tx_hashes.to_vec();
    while layer.len() < n {
        layer.push(*layer.last().unwrap());
    }

    while layer.len() > 1 {
        let mut next_layer = Vec::with_capacity(layer.len() / 2);
        for pair in layer.chunks(2) {
            next_layer.push(hash_pair(&pair[0], &pair[1]));
        }
        layer = next_layer;
    }

    layer[0]
}

/// Build a Merkle proof for the transaction at `index`.
///
/// Returns the sibling hashes bottom-up.  The caller combines this with
/// `verify_inclusion_proof`.
pub fn build_merkle_proof(
    tx_hashes: &[[u8; 32]],
    index: usize,
) -> Vec<[u8; 32]> {
    if tx_hashes.len() <= 1 {
        return vec![];
    }

    let n = tx_hashes.len().next_power_of_two();
    let mut padded: Vec<[u8; 32]> = tx_hashes.to_vec();
    while padded.len() < n {
        padded.push(*padded.last().unwrap());
    }

    let mut proof = Vec::new();
    let mut layer = padded;
    let mut idx = index;

    while layer.len() > 1 {
        let sibling_idx = if idx % 2 == 0 { idx + 1 } else { idx - 1 };
        proof.push(layer[sibling_idx]);

        let mut next_layer = Vec::with_capacity(layer.len() / 2);
        for pair in layer.chunks(2) {
            next_layer.push(hash_pair(&pair[0], &pair[1]));
        }
        layer = next_layer;
        idx /= 2;
    }

    proof
}

// ═══════════════════════════════════════════════════════════════════════════
//  High-level verify-all
// ═══════════════════════════════════════════════════════════════════════════

/// Run **all** inclusion-integrity checks in one call.
///
/// This is the primary function a monitoring script should call.
///
/// # Arguments
/// * `attestation`    – the attestation from the BAM node
/// * `proof`          – Merkle inclusion proof for your transaction
/// * `expected_pos`   – the position you expect your tx to occupy
/// * `config`         – verifier settings (TEE measurement, max age)
///
/// # Returns
/// A `VerificationResult` summarising every check.
#[instrument(skip_all, fields(slot = attestation.slot, expected_pos))]
pub fn verify_inclusion_integrity(
    attestation: &InclusionAttestation,
    proof: &InclusionProof,
    expected_pos: u32,
    config: &VerifierConfig,
) -> VerificationResult {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // 1. Signature
    let signature_valid = verify_attestation_signature(attestation).unwrap_or(false);

    // 2. TEE measurement
    let tee_measurement_valid = config
        .expected_tee_measurement
        .as_deref()
        .map(|exp| verify_tee_measurement(attestation, exp))
        .unwrap_or(true); // skip if no expected measurement configured

    // 3. Freshness
    let attestation_fresh =
        verify_attestation_freshness(attestation, config.max_attestation_age_secs, now);

    // 4. Merkle inclusion
    let merkle_proof_valid = verify_inclusion_proof(attestation, proof).unwrap_or(false);

    // 5. Position
    let position_verified = proof.position == expected_pos;
    if !position_verified {
        warn!(
            expected = expected_pos,
            actual = proof.position,
            "POSITION MISMATCH — possible front-run or reorder!"
        );
    }

    let all_valid = signature_valid
        && tee_measurement_valid
        && attestation_fresh
        && merkle_proof_valid
        && position_verified;

    let summary = if all_valid {
        format!(
            "Inclusion integrity VERIFIED for slot {} position {}",
            attestation.slot, proof.position
        )
    } else {
        let mut failures = Vec::new();
        if !signature_valid {
            failures.push("BAD_SIGNATURE");
        }
        if !tee_measurement_valid {
            failures.push("TEE_MISMATCH");
        }
        if !attestation_fresh {
            failures.push("STALE");
        }
        if !merkle_proof_valid {
            failures.push("MERKLE_INVALID");
        }
        if !position_verified {
            failures.push("POSITION_MISMATCH");
        }
        format!(
            "Inclusion integrity FAILED for slot {}: {}",
            attestation.slot,
            failures.join(", ")
        )
    };

    info!("{summary}");

    VerificationResult {
        signature_valid,
        tee_measurement_valid,
        attestation_fresh,
        merkle_proof_valid,
        position_verified,
        slot: attestation.slot,
        verified_position: if merkle_proof_valid {
            Some(proof.position)
        } else {
            None
        },
        summary,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// Hash two 32-byte nodes into their parent (left || right).
fn hash_pair(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

/// Build the canonical attestation payload that the enclave signs.
///
/// `SHA-256( slot_le || merkle_root || timestamp_le )`
pub fn build_attestation_payload(
    slot: u64,
    merkle_root: &[u8; 32],
    timestamp: u64,
) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(slot.to_le_bytes());
    hasher.update(merkle_root);
    hasher.update(timestamp.to_le_bytes());
    hasher.finalize().to_vec()
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    // ── Helper: build a valid attestation + proof from scratch ───────────

    fn make_test_attestation(
        n_txs: usize,
        target_idx: usize,
    ) -> (InclusionAttestation, InclusionProof, Vec<[u8; 32]>) {
        // Create deterministic "transaction" hashes
        let tx_hashes: Vec<[u8; 32]> = (0..n_txs)
            .map(|i| {
                let mut h = Sha256::new();
                h.update(format!("tx_{i}").as_bytes());
                h.finalize().into()
            })
            .collect();

        let merkle_root = compute_merkle_root(&tx_hashes);
        let proof_siblings = build_merkle_proof(&tx_hashes, target_idx);

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Ed25519 key pair
        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();

        // Sign canonical payload
        let payload = build_attestation_payload(123_456_789, &merkle_root, now_secs);
        let signature = signing_key.sign(&payload);

        let attestation = InclusionAttestation {
            enclave_pubkey: verifying_key.to_bytes().to_vec(),
            slot: 123_456_789,
            merkle_root,
            signature: signature.to_bytes().to_vec(),
            tee_measurement: "aabbccdd".into(),
            timestamp: now_secs,
        };

        let inclusion_proof = InclusionProof {
            tx_hash: tx_hashes[target_idx],
            position: target_idx as u32,
            total_txs: n_txs as u32,
            proof: proof_siblings,
        };

        (attestation, inclusion_proof, tx_hashes)
    }

    // ── Merkle tree tests ────────────────────────────────────────────────

    #[test]
    fn merkle_root_single_tx() {
        let h = hash_transaction(b"hello");
        let root = compute_merkle_root(&[h]);
        assert_eq!(root, h);
    }

    #[test]
    fn merkle_root_two_txs() {
        let h0 = hash_transaction(b"tx0");
        let h1 = hash_transaction(b"tx1");
        let root = compute_merkle_root(&[h0, h1]);
        let expected = hash_pair(&h0, &h1);
        assert_eq!(root, expected);
    }

    #[test]
    fn merkle_proof_roundtrip_4_txs() {
        let hashes: Vec<[u8; 32]> = (0..4)
            .map(|i| hash_transaction(format!("tx{i}").as_bytes()))
            .collect();

        let root = compute_merkle_root(&hashes);

        for idx in 0..4 {
            let siblings = build_merkle_proof(&hashes, idx);

            // Walk proof manually
            let mut current = hashes[idx];
            let mut pos = idx;
            for sibling in &siblings {
                current = if pos % 2 == 0 {
                    hash_pair(&current, sibling)
                } else {
                    hash_pair(sibling, &current)
                };
                pos /= 2;
            }
            assert_eq!(current, root, "proof failed for index {idx}");
        }
    }

    #[test]
    fn merkle_proof_roundtrip_7_txs() {
        let hashes: Vec<[u8; 32]> = (0..7)
            .map(|i| hash_transaction(format!("block_tx_{i}").as_bytes()))
            .collect();

        let root = compute_merkle_root(&hashes);

        for idx in 0..7 {
            let siblings = build_merkle_proof(&hashes, idx);
            let mut current = hashes[idx];
            let mut pos = idx;
            for sibling in &siblings {
                current = if pos % 2 == 0 {
                    hash_pair(&current, sibling)
                } else {
                    hash_pair(sibling, &current)
                };
                pos /= 2;
            }
            assert_eq!(current, root, "proof failed for index {idx}");
        }
    }

    // ── Attestation signature tests ─────────────────────────────────────

    #[test]
    fn valid_attestation_signature() {
        let (att, _, _) = make_test_attestation(4, 0);
        let valid = verify_attestation_signature(&att).unwrap();
        assert!(valid);
    }

    #[test]
    fn tampered_attestation_fails_signature() {
        let (mut att, _, _) = make_test_attestation(4, 0);
        att.slot += 1; // tamper with slot
        let valid = verify_attestation_signature(&att).unwrap();
        assert!(!valid);
    }

    #[test]
    fn wrong_key_fails_signature() {
        let (mut att, _, _) = make_test_attestation(4, 0);
        let other_key = SigningKey::from_bytes(&[99u8; 32]);
        att.enclave_pubkey = other_key.verifying_key().to_bytes().to_vec();
        let valid = verify_attestation_signature(&att).unwrap();
        assert!(!valid);
    }

    // ── TEE measurement tests ───────────────────────────────────────────

    #[test]
    fn tee_measurement_match() {
        let (att, _, _) = make_test_attestation(4, 0);
        assert!(verify_tee_measurement(&att, "aabbccdd"));
        assert!(verify_tee_measurement(&att, "AABBCCDD")); // case insensitive
    }

    #[test]
    fn tee_measurement_mismatch() {
        let (att, _, _) = make_test_attestation(4, 0);
        assert!(!verify_tee_measurement(&att, "deadbeef"));
    }

    // ── Freshness tests ─────────────────────────────────────────────────

    #[test]
    fn fresh_attestation() {
        let now = 1_000_000u64;
        let (mut att, _, _) = make_test_attestation(4, 0);
        att.timestamp = now - 30; // 30 seconds old
        assert!(verify_attestation_freshness(&att, 120, now));
    }

    #[test]
    fn stale_attestation() {
        let now = 1_000_000u64;
        let (mut att, _, _) = make_test_attestation(4, 0);
        att.timestamp = now - 300; // 5 minutes old
        assert!(!verify_attestation_freshness(&att, 120, now));
    }

    #[test]
    fn future_attestation_rejected() {
        let now = 1_000_000u64;
        let (mut att, _, _) = make_test_attestation(4, 0);
        att.timestamp = now + 60; // 60 seconds in the future
        assert!(!verify_attestation_freshness(&att, 120, now));
    }

    // ── Inclusion proof tests ───────────────────────────────────────────

    #[test]
    fn valid_inclusion_proof() {
        let (att, proof, _) = make_test_attestation(4, 2);
        let valid = verify_inclusion_proof(&att, &proof).unwrap();
        assert!(valid);
    }

    #[test]
    fn tampered_tx_hash_fails_proof() {
        let (att, mut proof, _) = make_test_attestation(4, 2);
        proof.tx_hash = [0xFFu8; 32]; // not the original tx
        let valid = verify_inclusion_proof(&att, &proof).unwrap();
        assert!(!valid);
    }

    #[test]
    fn wrong_position_fails_proof() {
        let (att, mut proof, _) = make_test_attestation(4, 2);
        // Swap position but keep same proof siblings → root mismatch
        proof.position = 0;
        let valid = verify_inclusion_proof(&att, &proof).unwrap();
        assert!(!valid);
    }

    // ── Full verify_inclusion_integrity tests ───────────────────────────

    #[test]
    fn full_verification_passes() {
        let (att, proof, _) = make_test_attestation(4, 1);
        let config = VerifierConfig {
            expected_tee_measurement: Some("aabbccdd".into()),
            max_attestation_age_secs: 600,
        };
        let result = verify_inclusion_integrity(&att, &proof, 1, &config);
        assert!(result.is_valid(), "full verification should pass: {result}");
        assert_eq!(result.verified_position, Some(1));
    }

    #[test]
    fn full_verification_fails_wrong_position() {
        let (att, proof, _) = make_test_attestation(4, 1);
        let config = VerifierConfig::default();
        // Expect position 0, but tx is at position 1
        let result = verify_inclusion_integrity(&att, &proof, 0, &config);
        assert!(!result.position_verified);
        assert!(!result.is_valid());
        assert!(result.summary.contains("POSITION_MISMATCH"));
    }

    #[test]
    fn full_verification_fails_bad_measurement() {
        let (att, proof, _) = make_test_attestation(4, 1);
        let config = VerifierConfig {
            expected_tee_measurement: Some("badc0ffee".into()),
            max_attestation_age_secs: 600,
        };
        let result = verify_inclusion_integrity(&att, &proof, 1, &config);
        assert!(!result.tee_measurement_valid);
        assert!(!result.is_valid());
    }
}
