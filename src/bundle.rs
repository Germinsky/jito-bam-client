/// Bundle types and helpers for Jito BAM atomic transaction bundles.
///
/// A bundle is an ordered list of 1–5 signed transactions that execute
/// atomically in a single slot.  The Jito tip must be the **last
/// instruction of the last transaction** in the bundle.
use serde::{Deserialize, Serialize};

// ─── Bundle result ──────────────────────────────────────────────────────────

/// Result returned after successful bundle submission to a BAM node.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleResult {
    /// Unique bundle identifier (used for status polling).
    pub bundle_id: String,
    /// Whether the BAM node accepted the bundle into its auction.
    pub accepted: bool,
    /// Base-58 signatures of each transaction in the bundle.
    pub signatures: Vec<String>,
    /// Optional message from the BAM node.
    pub message: Option<String>,
}

// ─── Bundle status (from polling) ───────────────────────────────────────────

/// Status of a previously submitted bundle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum BundleStatus {
    /// Bundle is queued but not yet picked up by the block-engine.
    Pending,
    /// Bundle is being processed / forwarded to the leader.
    Processing,
    /// Bundle landed on-chain in the given slot.
    Landed { slot: u64 },
    /// Bundle was dropped or simulation failed.
    Failed { slot: u64, reason: String },
    /// Status unknown (the BAM node has no record — may have expired).
    Unknown,
}

impl BundleStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Landed { .. } | Self::Failed { .. })
    }

    pub fn is_landed(&self) -> bool {
        matches!(self, Self::Landed { .. })
    }
}

impl std::fmt::Display for BundleStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "Pending"),
            Self::Processing => write!(f, "Processing"),
            Self::Landed { slot } => write!(f, "Landed (slot {slot})"),
            Self::Failed { slot, reason } => write!(f, "Failed (slot {slot}): {reason}"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_status_terminal() {
        assert!(!BundleStatus::Pending.is_terminal());
        assert!(!BundleStatus::Processing.is_terminal());
        assert!(BundleStatus::Landed { slot: 42 }.is_terminal());
        assert!(BundleStatus::Failed { slot: 42, reason: "sim fail".into() }.is_terminal());
        assert!(!BundleStatus::Unknown.is_terminal());
    }

    #[test]
    fn bundle_status_display() {
        let s = format!("{}", BundleStatus::Landed { slot: 123 });
        assert!(s.contains("123"));
    }
}
