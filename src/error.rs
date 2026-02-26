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
}

pub type Result<T> = std::result::Result<T, JitoBamError>;
