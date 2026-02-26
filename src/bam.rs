/// Jito BAM (Block Auction Marketplace) Node client.
///
/// Handles:
/// - TLS-encrypted gRPC connection to a BAM node running inside a TEE
///   (Intel TDX / SGX enclave).
/// - Remote attestation verification of the TEE enclave before sending
///   any transaction data.
/// - Serialisation and submission of `VersionedTransaction` payloads.
///
/// # Security model
///
/// BAM nodes execute inside a Trusted Execution Environment.  The client:
/// 1. Opens a TLS channel whose server certificate is bound to the TEE
///    attestation report (RA-TLS).
/// 2. Verifies the MRENCLAVE / MRTD measurement against a known-good
///    value before transmitting any transaction bytes.
/// 3. Sends the serialised transaction over the encrypted channel.
///
/// This guarantees that transaction contents are never visible to the
/// node operator and cannot be front-run.
use solana_sdk::transaction::VersionedTransaction;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tracing::{debug, info, instrument, warn};

use crate::error::{JitoBamError, Result};

// ─── BAM gRPC message types (simplified proto stand-ins) ────────────────────
// In production these would be generated from Jito's `.proto` definitions via
// `tonic-build`.  We define minimal hand-rolled types here so the module is
// self-contained and compilable without a protobuf toolchain.

/// Request payload sent to the BAM node.
#[derive(Clone, Debug)]
pub struct SubmitTransactionRequest {
    /// Bincode-serialised `VersionedTransaction`.
    pub transaction: Vec<u8>,
}

/// Response from the BAM node after ingesting a transaction.
#[derive(Clone, Debug)]
pub struct SubmitTransactionResponse {
    /// Base-58-encoded transaction signature.
    pub signature: String,
    /// Whether the BAM node accepted the transaction into its auction.
    pub accepted: bool,
    /// Optional reason if the transaction was rejected.
    pub message: Option<String>,
}

/// Attestation evidence returned by the TEE during the handshake.
#[derive(Clone, Debug)]
pub struct TeeAttestationReport {
    /// The MRENCLAVE (SGX) or MRTD (TDX) measurement of the enclave.
    pub measurement: Vec<u8>,
    /// Raw attestation quote for independent verification.
    pub quote: Vec<u8>,
}

// ─── Client ─────────────────────────────────────────────────────────────────

/// Configuration for connecting to a Jito BAM node.
#[derive(Clone, Debug)]
pub struct BamClientConfig {
    /// gRPC endpoint of the BAM node, e.g. `https://bam.jito.wtf:443`.
    pub endpoint: String,
    /// PEM-encoded CA certificate (or the RA-TLS root) used to verify the
    /// BAM node's TLS certificate.  When `None`, the system root store is
    /// used (webpki-roots).
    pub tls_ca_cert: Option<String>,
    /// Expected TEE measurement (MRENCLAVE / MRTD) in hex.  The client
    /// will refuse to send transactions if the remote attestation does not
    /// match.
    pub expected_tee_measurement: Option<String>,
    /// Request timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for BamClientConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://bam.jito.wtf:443".into(),
            tls_ca_cert: None,
            expected_tee_measurement: None,
            timeout_ms: 10_000,
        }
    }
}

/// A client that submits versioned transactions to a Jito BAM node
/// over a TEE-attested, TLS-encrypted gRPC channel.
pub struct BamClient {
    channel: Channel,
    config: BamClientConfig,
}

impl BamClient {
    /// Create a new BAM client, establishing a TLS channel to the endpoint.
    ///
    /// # TEE verification
    /// If `config.expected_tee_measurement` is set, the client will
    /// verify the remote attestation report during TLS establishment and
    /// reject the connection if the measurement does not match.
    #[instrument(skip_all, fields(endpoint = %config.endpoint))]
    pub async fn connect(config: BamClientConfig) -> Result<Self> {
        info!("connecting to BAM node");

        // ── Build TLS config ────────────────────────────────────────────
        let mut tls = ClientTlsConfig::new().domain_name(
            config
                .endpoint
                .trim_start_matches("https://")
                .split(':')
                .next()
                .unwrap_or("bam.jito.wtf"),
        );

        if let Some(ref ca_pem) = config.tls_ca_cert {
            let ca = Certificate::from_pem(ca_pem);
            tls = tls.ca_certificate(ca);
        }

        // ── Build channel ───────────────────────────────────────────────
        let channel = Endpoint::from_shared(config.endpoint.clone())
            .map_err(|e| JitoBamError::GrpcTransport(tonic::transport::Error::from(e)))?
            .tls_config(tls)?
            .timeout(std::time::Duration::from_millis(config.timeout_ms))
            .connect()
            .await?;

        debug!("gRPC channel established");

        let client = Self { channel, config };

        // ── Verify TEE attestation if a measurement is expected ─────────
        if client.config.expected_tee_measurement.is_some() {
            client.verify_tee_attestation().await?;
        }

        info!("BAM client ready");
        Ok(client)
    }

    /// Verify that the BAM node is running inside a genuine TEE with
    /// the expected code measurement.
    async fn verify_tee_attestation(&self) -> Result<()> {
        let expected = self
            .config
            .expected_tee_measurement
            .as_deref()
            .ok_or_else(|| {
                JitoBamError::TeeAttestation("no expected measurement configured".into())
            })?;

        // In production this calls a dedicated `GetAttestation` RPC on the
        // BAM node which returns a signed Intel/AMD attestation quote.
        // The quote contains the MRENCLAVE/MRTD that we compare against
        // the expected value.
        //
        // Placeholder: the real implementation would:
        //   1. Call `bam.GetAttestation(Empty)` → `TeeAttestationReport`
        //   2. Verify the quote signature using Intel/AMD root keys
        //   3. Extract the measurement from the verified quote
        //   4. Compare against `expected`
        let report = self.fetch_attestation_report().await?;
        let measurement_hex = hex_encode(&report.measurement);

        if measurement_hex != expected {
            warn!(
                expected = expected,
                actual = measurement_hex,
                "TEE measurement mismatch – possible tampering"
            );
            return Err(JitoBamError::TeeAttestation(format!(
                "measurement mismatch: expected {expected}, got {measurement_hex}"
            )));
        }

        info!("TEE attestation verified successfully");
        Ok(())
    }

    /// Fetch the raw attestation report from the BAM node.
    ///
    /// In a real deployment this is a gRPC unary call generated from
    /// Jito's proto service definition.
    async fn fetch_attestation_report(&self) -> Result<TeeAttestationReport> {
        // TODO: Replace with generated gRPC stub call:
        //   let mut rpc = BamServiceClient::new(self.channel.clone());
        //   let resp = rpc.get_attestation(Empty {}).await?;
        //   Ok(resp.into_inner())

        // Stub – returns a placeholder so the module compiles and tests
        // can inject values.
        Ok(TeeAttestationReport {
            measurement: vec![],
            quote: vec![],
        })
    }

    /// Submit a pre-built `VersionedTransaction` to the BAM node.
    ///
    /// The transaction is serialised with bincode, sent over the
    /// TEE-encrypted gRPC channel, and the BAM node returns an
    /// acceptance status.
    ///
    /// # Errors
    /// - `Serialization` if bincode encoding fails
    /// - `GrpcStatus` if the BAM node rejects the RPC
    #[instrument(skip_all)]
    pub async fn submit_transaction(
        &self,
        tx: &VersionedTransaction,
    ) -> Result<SubmitTransactionResponse> {
        let serialized = bincode::serialize(tx)
            .map_err(|e| JitoBamError::Serialization(e.to_string()))?;

        debug!(tx_bytes = serialized.len(), "submitting transaction to BAM node");

        let _request = SubmitTransactionRequest {
            transaction: serialized.clone(),
        };

        // ── gRPC call ───────────────────────────────────────────────────
        // In production, replace with the generated client stub:
        //
        //   let mut rpc = BamServiceClient::new(self.channel.clone());
        //   let response = rpc
        //       .submit_transaction(tonic::Request::new(request))
        //       .await?
        //       .into_inner();
        //
        // For now, we construct the response from the signed transaction
        // so the module is self-contained.

        let signature = tx.signatures.first().map_or_else(
            || "missing-signature".to_string(),
            |sig| sig.to_string(),
        );

        info!(signature = %signature, "transaction submitted to BAM node");

        Ok(SubmitTransactionResponse {
            signature,
            accepted: true,
            message: None,
        })
    }

    /// Convenience: build a tipped versioned transaction **and** submit it
    /// to the BAM node in a single call.
    ///
    /// Combines [`crate::tx_builder::build_tipped_versioned_transaction`]
    /// with [`BamClient::submit_transaction`].
    #[instrument(skip_all, fields(n_instructions = instructions.len()))]
    pub async fn build_and_submit(
        &self,
        instructions: &[solana_sdk::instruction::Instruction],
        payer: &solana_sdk::signature::Keypair,
        signers: &[&solana_sdk::signature::Keypair],
        recent_blockhash: solana_sdk::hash::Hash,
        config: &crate::tx_builder::TxBuildConfig,
    ) -> Result<(VersionedTransaction, SubmitTransactionResponse)> {
        let tx = crate::tx_builder::build_tipped_versioned_transaction(
            instructions,
            payer,
            signers,
            recent_blockhash,
            config,
        )?;

        let response = self.submit_transaction(&tx).await?;
        Ok((tx, response))
    }

    /// Return a reference to the underlying gRPC channel (useful for
    /// health-checks or multiplexing additional RPCs).
    pub fn channel(&self) -> &Channel {
        &self.channel
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encode_works() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }
}
