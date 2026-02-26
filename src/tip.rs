/// Jito tip instruction construction and tip-account selection.
///
/// The Jito tip program is a simple SOL transfer to one of the 8 rotating
/// tip accounts.  The block-engine selects the transaction bundle with the
/// highest cumulative tip, so always append the tip as the **last**
/// instruction to make it trivially verifiable by the leader.
use rand::seq::SliceRandom;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
};
use std::str::FromStr;

use crate::error::{JitoBamError, Result};

/// The eight well-known Jito tip accounts (mainnet-beta).
/// Source: <https://jito-labs.gitbook.io/mev/searcher-resources/tip-accounts>
const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4bVqkfRtQ7NmXwkiY8qHb2G",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSLjT5SKoLUfEpAQdgt",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// Minimum sensible tip – below this the transaction is unlikely to receive
/// priority from the leader/block-engine.
pub const MIN_TIP_LAMPORTS: u64 = 1_000; // 0.000001 SOL

/// Parse and cache the tip pubkeys at first use.
pub fn tip_accounts() -> Vec<Pubkey> {
    JITO_TIP_ACCOUNTS
        .iter()
        .map(|s| Pubkey::from_str(s).expect("hardcoded tip account pubkey must be valid"))
        .collect()
}

/// Pick a random tip account to spread load across the set.
pub fn random_tip_account() -> Result<Pubkey> {
    let accounts = tip_accounts();
    accounts
        .choose(&mut rand::thread_rng())
        .copied()
        .ok_or(JitoBamError::NoTipAccounts)
}

/// Build a Jito tip instruction (a plain SOL transfer via the System Program).
///
/// # Arguments
/// * `payer`       – the fee-payer / tipper pubkey
/// * `tip_lamports` – amount to tip (must be >= [`MIN_TIP_LAMPORTS`])
/// * `tip_account`  – optional explicit tip account; random if `None`
///
/// # Errors
/// Returns [`JitoBamError::InvalidTipAmount`] if the tip is below the minimum.
pub fn build_tip_instruction(
    payer: &Pubkey,
    tip_lamports: u64,
    tip_account: Option<Pubkey>,
) -> Result<Instruction> {
    if tip_lamports < MIN_TIP_LAMPORTS {
        return Err(JitoBamError::InvalidTipAmount(tip_lamports, MIN_TIP_LAMPORTS));
    }

    let recipient = match tip_account {
        Some(acct) => acct,
        None => random_tip_account()?,
    };

    // SOL transfer via the System Program (TransferInstruction = index 2).
    Ok(Instruction {
        program_id: system_program::id(),
        accounts: vec![
            AccountMeta::new(*payer, true),      // from – signer, writable
            AccountMeta::new(recipient, false),   // to   – writable
        ],
        data: {
            // System program Transfer instruction: u32 discriminant (2) + u64 lamports
            let mut data = Vec::with_capacity(12);
            data.extend_from_slice(&2u32.to_le_bytes());
            data.extend_from_slice(&tip_lamports.to_le_bytes());
            data
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tip_instruction_rejects_zero() {
        let payer = Pubkey::new_unique();
        let result = build_tip_instruction(&payer, 0, None);
        assert!(result.is_err());
    }

    #[test]
    fn tip_instruction_has_correct_accounts() {
        let payer = Pubkey::new_unique();
        let ix = build_tip_instruction(&payer, 10_000, None).unwrap();
        assert_eq!(ix.accounts.len(), 2);
        assert!(ix.accounts[0].is_signer);
        assert!(ix.accounts[0].is_writable);
        assert!(!ix.accounts[1].is_signer);
        assert!(ix.accounts[1].is_writable);
    }

    #[test]
    fn random_tip_account_is_in_set() {
        let accounts = tip_accounts();
        let picked = random_tip_account().unwrap();
        assert!(accounts.contains(&picked));
    }
}
