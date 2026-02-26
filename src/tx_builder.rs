/// Versioned-transaction builder with automatic Jito tip injection.
///
/// Constructs a `VersionedTransaction` (V0 with address-lookup-table support)
/// and always appends a Jito tip as the **final** instruction so the
/// block-engine/leader can trivially verify the bid.
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    hash::Hash,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::VersionedTransaction,
};
use tracing::{debug, instrument};

use crate::error::{JitoBamError, Result};
use crate::tip;

/// Maximum serialised transaction size on Solana (1232 bytes as of v1.18+).
const MAX_TX_SIZE: usize = 1_232;

/// Configuration knobs for building a Jito-tipped transaction.
#[derive(Debug, Clone)]
pub struct TxBuildConfig {
    /// Tip amount in lamports.
    pub tip_lamports: u64,
    /// Optionally pin the tip to a specific tip account.
    pub tip_account: Option<Pubkey>,
    /// Address-lookup-table accounts to compress the message.
    pub address_lookup_tables: Vec<AddressLookupTableAccount>,
}

impl Default for TxBuildConfig {
    fn default() -> Self {
        Self {
            tip_lamports: 10_000, // default 0.00001 SOL tip
            tip_account: None,
            address_lookup_tables: Vec::new(),
        }
    }
}

/// Build and sign a versioned transaction from a list of instructions,
/// automatically appending a Jito tip instruction as the last instruction.
///
/// # Arguments
/// * `instructions`  – application instructions (the tip is appended automatically)
/// * `payer`         – the fee-payer keypair (also signs the tip transfer)
/// * `signers`       – any additional signers required by the instructions
/// * `recent_blockhash` – a recent blockhash for the transaction
/// * `config`        – builder configuration (tip amount, ALTs, …)
///
/// # Returns
/// A fully-signed `VersionedTransaction` ready for submission.
#[instrument(skip_all, fields(payer = %payer.pubkey(), n_instructions = instructions.len(), tip = config.tip_lamports))]
pub fn build_tipped_versioned_transaction(
    instructions: &[Instruction],
    payer: &Keypair,
    signers: &[&Keypair],
    recent_blockhash: Hash,
    config: &TxBuildConfig,
) -> Result<VersionedTransaction> {
    // ── 1. Append tip as the final instruction ──────────────────────────
    let tip_ix = tip::build_tip_instruction(
        &payer.pubkey(),
        config.tip_lamports,
        config.tip_account,
    )?;

    let mut all_instructions: Vec<Instruction> = instructions.to_vec();
    all_instructions.push(tip_ix);

    debug!(
        total_instructions = all_instructions.len(),
        "assembled instruction list with tip"
    );

    // ── 2. Compile into a V0 message ────────────────────────────────────
    let message = v0::Message::try_compile(
        &payer.pubkey(),
        &all_instructions,
        &config.address_lookup_tables,
        recent_blockhash,
    )
    .map_err(|e| JitoBamError::CompileError(e.to_string()))?;

    let versioned_message = VersionedMessage::V0(message);

    // ── 3. Collect all signers (payer first) ────────────────────────────
    let mut all_signers: Vec<&Keypair> = vec![payer];
    for s in signers {
        if s.pubkey() != payer.pubkey() {
            all_signers.push(s);
        }
    }

    // ── 4. Sign ─────────────────────────────────────────────────────────
    let tx = VersionedTransaction::try_new(versioned_message, &all_signers)
        .map_err(JitoBamError::Signing)?;

    // ── 5. Size guard ───────────────────────────────────────────────────
    let serialized = bincode::serialize(&tx)
        .map_err(|e| JitoBamError::Serialization(e.to_string()))?;

    if serialized.len() > MAX_TX_SIZE {
        return Err(JitoBamError::TransactionTooLarge {
            size: serialized.len(),
            max: MAX_TX_SIZE,
        });
    }

    debug!(tx_size = serialized.len(), "transaction signed and validated");
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{
        system_instruction,
    };

    #[test]
    fn build_simple_transfer_with_tip() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();

        let ixs = vec![system_instruction::transfer(
            &payer.pubkey(),
            &recipient,
            1_000_000,
        )];

        let blockhash = Hash::new_unique();
        let config = TxBuildConfig::default();

        let tx = build_tipped_versioned_transaction(&ixs, &payer, &[], blockhash, &config)
            .expect("should build transaction");

        // The V0 message should have our transfer + the tip = 2 instructions.
        match tx.message {
            VersionedMessage::V0(ref msg) => {
                assert_eq!(msg.instructions.len(), 2, "expected app ix + tip ix");
            }
            _ => panic!("expected V0 message"),
        }
    }

    #[test]
    fn rejects_below_minimum_tip() {
        let payer = Keypair::new();
        let config = TxBuildConfig {
            tip_lamports: 0,
            ..Default::default()
        };
        let result = build_tipped_versioned_transaction(
            &[],
            &payer,
            &[],
            Hash::new_unique(),
            &config,
        );
        assert!(result.is_err());
    }
}
