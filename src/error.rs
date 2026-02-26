use thiserror::Error;

#[derive(Debug, Error)]
pub enum JitoBamError {
    #[error("Solana client error: {0}")]
    SolanaClient(#[from] solana_sdk::transport::TransportError),

    #[error("Signing error: {0}")]
    Signing(#[from] solana_sdk::signer::SignerError),

    #[error("Transaction compilation error: {0}")]
    CompileError(String),

    #[error("gRPC transport error: {0}")]
    GrpcTransport(#[from] tonic::transport::Error),

    #[error("gRPC status error: {0}")]
    GrpcStatus(#[from] tonic::Status),

    #[error("TEE attestation verification failed: {0}")]
    TeeAttestation(String),

    #[error("No tip accounts available")]
    NoTipAccounts,

    #[error("Invalid tip amount: {0} lamports (minimum: {1})")]
    InvalidTipAmount(u64, u64),

    #[error("Transaction too large: {size} bytes (maximum: {max})")]
    TransactionTooLarge { size: usize, max: usize },

    #[error("Missing required signers")]
    MissingSigners,

    #[error("Blockhash fetch failed: {0}")]
    BlockhashFetch(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Bundle rejected: {0}")]
    BundleRejected(String),

    #[error("Bundle too large: {count} transactions (maximum: {max})")]
    BundleTooLarge { count: usize, max: usize },

    #[error("Bundle not landed after {attempts} status polls")]
    BundleNotLanded { attempts: u32 },

    #[error("RPC error: {0}")]
    RpcError(String),

    #[error("Max retries ({0}) exhausted")]
    RetriesExhausted(u32),

    #[error("Timeout after {0} ms")]
    Timeout(u64),

    #[error("Attestation signature invalid: {0}")]
    AttestationSignatureInvalid(String),

    #[error("Attestation slot mismatch: expected {expected}, got {actual}")]
    AttestationSlotMismatch { expected: u64, actual: u64 },

    #[error("Transaction not included in attested Merkle root")]
    TransactionNotInMerkleRoot,

    #[error("Attestation expired: age {age_secs}s exceeds max {max_secs}s")]
    AttestationExpired { age_secs: u64, max_secs: u64 },

    #[error("Merkle proof length invalid: {0}")]
    InvalidMerkleProofLength(usize),

    #[error("Position mismatch: expected index {expected}, proved index {actual}")]
    PositionMismatch { expected: u32, actual: u32 },

    #[error("ACE plugin error: {0}")]
    AcePluginError(String),

    #[error("ACE trigger registration rejected: {0}")]
    AceTriggerRejected(String),

    #[error("Price feed unavailable: {0}")]
    PriceFeedUnavailable(String),

    #[error("Meteora DLMM error: {0}")]
    MeteoraDlmmError(String),

    #[error("Slippage exceeded: expected min {min_out} but got {actual_out}")]
    SlippageExceeded { min_out: u64, actual_out: u64 },

    #[error("Dynamic tip error: {0}")]
    DynamicTipError(String),

    #[error("Raydium CLMM error: {0}")]
    RaydiumClmmError(String),
}

pub type Result<T> = std::result::Result<T, JitoBamError>;
