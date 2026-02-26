/// Raydium Concentrated Liquidity Market Maker (CLMM / AmmV3) swap builder.
///
/// Constructs a production-grade `SwapBaseIn` instruction for the Raydium
/// CLMM program, integrated with the Jito BAM private execution engine
/// for top-of-block inclusion.
///
/// # Features
///
/// - **Zero-copy pool deserialization** — reads only the bytes needed from
///   the on-chain `PoolState` account (sqrt_price, tick, liquidity) without
///   allocating the full 1.6 KiB layout, minimising CU overhead.
/// - **Dynamic slippage protection** — `min_amount_out` is derived from the
///   pool's current `sqrt_price_x64` with a configurable tolerance (default
///   0.5% / 50 bps).
/// - **Versioned Transaction output** — returns a `VersionedTransaction` (V0)
///   with Jito tip as the final instruction, ready for `send_bundle()`.
/// - **Tick-array derivation** — deterministically computes the 3 tick-array
///   PDA addresses the CLMM program needs, based on the current tick index.
/// - **Address-lookup-table support** — optionally compresses the 18-account
///   instruction into a V0 message via ALTs.
///
/// # Architecture
///
/// ```text
///   ┌─────────────────┐     ┌───────────────────────┐
///   │  Your Trading    │────▶│  RaydiumClmmSwapBuilder│
///   │  Bot / Strategy  │     │                       │
///   └─────────────────┘     │ 1. fetch_pool_state() │
///                           │ 2. derive_tick_arrays()│
///                           │ 3. calc min_amount_out │
///                           │ 4. build_swap_ix()     │
///                           │ 5. build_versioned_tx()│
///                           └───────────┬───────────┘
///                                       │
///                          ┌────────────┼────────────┐
///                          ▼            ▼            ▼
///                     Solana RPC   CLMM Program   Jito TEE
/// ```
///
/// # Raydium CLMM SwapBaseIn account layout (v3)
///
/// | #  | Account                | Signer | Writable |
/// |----|------------------------|--------|----------|
/// | 0  | payer (user authority) | ✓      |          |
/// | 1  | amm_config             |        |          |
/// | 2  | pool_state             |        | ✓        |
/// | 3  | input_token_account    |        | ✓        |
/// | 4  | output_token_account   |        | ✓        |
/// | 5  | input_vault            |        | ✓        |
/// | 6  | output_vault           |        | ✓        |
/// | 7  | observation_state      |        | ✓        |
/// | 8  | token_program          |        |          |
/// | 9  | token_program_2022     |        |          |
/// | 10 | memo_program           |        |          |
/// | 11 | input_vault_mint       |        |          |
/// | 12 | output_vault_mint      |        |          |
/// | 13 | tick_array_0           |        | ✓        |
/// | 14 | tick_array_1           |        | ✓        |
/// | 15 | tick_array_2           |        | ✓        |
///
/// Instruction data (SwapBaseIn):
/// - 8 bytes: Anchor discriminator
/// - 8 bytes: `amount_in` (u64 LE)
/// - 8 bytes: `min_amount_out` (u64 LE)
/// - 16 bytes: `sqrt_price_limit_x64` (u128 LE)

use std::str::FromStr;

use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::VersionedTransaction,
};
use tracing::{debug, info, instrument, warn};

use crate::error::{JitoBamError, Result};
use crate::tip;

// ═══════════════════════════════════════════════════════════════════════════
//  Constants
// ═══════════════════════════════════════════════════════════════════════════

/// Raydium CLMM (AmmV3) program ID — mainnet-beta.
pub const RAYDIUM_CLMM_PROGRAM_ID: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";

/// SPL Token program.
pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// SPL Token-2022 program.
pub const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// SPL Memo program (v2). Required by Raydium CLMM.
pub const MEMO_PROGRAM_ID: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

/// Anchor discriminator for `swap` (sha256("global:swap")[..8]).
const SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

/// Default slippage tolerance (0.5% = 50 bps).
pub const DEFAULT_SLIPPAGE_BPS: u16 = 50;

/// Maximum slippage we allow (10% = 1000 bps) — safety guard.
const MAX_SLIPPAGE_BPS: u16 = 1_000;

/// Number of ticks per tick-array in the Raydium CLMM.
const TICK_ARRAY_SIZE: i32 = 60;

/// Minimum sqrt_price_x64 boundary (≈ price → 0). From Raydium SDK.
pub const MIN_SQRT_PRICE_X64: u128 = 4295048016;

/// Maximum sqrt_price_x64 boundary (≈ price → ∞). From Raydium SDK.
pub const MAX_SQRT_PRICE_X64: u128 = 79226673515401279992447579055;

// ═══════════════════════════════════════════════════════════════════════════
//  Zero-copy pool state
// ═══════════════════════════════════════════════════════════════════════════

/// Zero-copy deserialization of the on-chain Raydium CLMM PoolState.
///
/// The full PoolState is ~1600 bytes. We only read the fields we need
/// for swap construction to minimise allocations and compute.
///
/// # Layout offsets (Raydium CLMM v3 — anchor account discriminator = 8 bytes)
///
/// | Offset | Size | Field              | Type    |
/// |--------|------|--------------------|---------|
/// | 8      | 1    | bump[0]            | u8      |
/// | 9      | 32   | amm_config         | Pubkey  |
/// | 41     | 32   | owner              | Pubkey  |
/// | 73     | 32   | token_mint_0       | Pubkey  |
/// | 105    | 32   | token_mint_1       | Pubkey  |
/// | 137    | 32   | token_vault_0      | Pubkey  |
/// | 169    | 32   | token_vault_1      | Pubkey  |
/// | 201    | 32   | observation_key    | Pubkey  |
/// | 233    | 1    | mint_decimals_0    | u8      |
/// | 234    | 1    | mint_decimals_1    | u8      |
/// | 235    | 2    | tick_spacing       | u16     |
/// | 237    | 16   | liquidity          | u128    |
/// | 253    | 16   | sqrt_price_x64     | u128    |
/// | 269    | 4    | tick_current       | i32     |
/// | 345    | 8    | fee_growth_global0 | u64     |
/// | 353    | 8    | fee_growth_global1 | u64     |
/// | 361    | 8    | protocol_fees_0    | u64     |
/// | 369    | 8    | protocol_fees_1    | u64     |
/// | 377    | 8    | fund_fees_0        | u64     |
/// | 385    | 8    | fund_fees_1        | u64     |
/// | 809    | 32   | vault_0_mint       | Pubkey  |
/// | 841    | 32   | vault_1_mint       | Pubkey  |
///
/// Remaining fields (padding, reward_infos, etc.) are skipped.
#[derive(Clone, Debug)]
pub struct ClmmPoolState {
    /// AMM config account (fee tiers, protocol owner).
    pub amm_config: Pubkey,
    /// Token mint 0 (base token).
    pub token_mint_0: Pubkey,
    /// Token mint 1 (quote token).
    pub token_mint_1: Pubkey,
    /// Token vault 0 (pool's ATA for mint 0).
    pub token_vault_0: Pubkey,
    /// Token vault 1 (pool's ATA for mint 1).
    pub token_vault_1: Pubkey,
    /// Observation state account (TWAP oracle).
    pub observation_key: Pubkey,
    /// Decimals of token mint 0.
    pub mint_decimals_0: u8,
    /// Decimals of token mint 1.
    pub mint_decimals_1: u8,
    /// Tick spacing for this pool's fee tier.
    pub tick_spacing: u16,
    /// Current liquidity in range.
    pub liquidity: u128,
    /// Current sqrt(price) as a Q64.64 fixed-point.
    pub sqrt_price_x64: u128,
    /// Current tick index.
    pub tick_current: i32,
}

impl ClmmPoolState {
    /// Zero-copy deserialize a `ClmmPoolState` from raw account data.
    ///
    /// Reads only the specific byte ranges needed, avoiding a full
    /// `borsh::BorshDeserialize` of the 1.6 KiB layout.
    pub fn from_account_data(data: &[u8]) -> Result<Self> {
        // Minimum size check: we need at least up to tick_current (offset 269 + 4 = 273)
        // plus observation_key (201..233) and optionally vault mints
        const MIN_LEN: usize = 273;
        if data.len() < MIN_LEN {
            return Err(JitoBamError::RaydiumClmmError(format!(
                "PoolState account too small: {} bytes (need >= {MIN_LEN})",
                data.len()
            )));
        }

        let read_pubkey = |offset: usize| -> Result<Pubkey> {
            let bytes: [u8; 32] = data[offset..offset + 32]
                .try_into()
                .map_err(|_| {
                    JitoBamError::RaydiumClmmError(format!(
                        "failed to read pubkey at offset {offset}"
                    ))
                })?;
            Ok(Pubkey::new_from_array(bytes))
        };

        let read_u16 = |offset: usize| -> u16 {
            u16::from_le_bytes([data[offset], data[offset + 1]])
        };

        let read_i32 = |offset: usize| -> i32 {
            i32::from_le_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ])
        };

        let read_u128 = |offset: usize| -> u128 {
            let mut buf = [0u8; 16];
            buf.copy_from_slice(&data[offset..offset + 16]);
            u128::from_le_bytes(buf)
        };

        Ok(ClmmPoolState {
            amm_config: read_pubkey(9)?,
            token_mint_0: read_pubkey(73)?,
            token_mint_1: read_pubkey(105)?,
            token_vault_0: read_pubkey(137)?,
            token_vault_1: read_pubkey(169)?,
            observation_key: read_pubkey(201)?,
            mint_decimals_0: data[233],
            mint_decimals_1: data[234],
            tick_spacing: read_u16(235),
            liquidity: read_u128(237),
            sqrt_price_x64: read_u128(253),
            tick_current: read_i32(269),
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Swap configuration
// ═══════════════════════════════════════════════════════════════════════════

/// Configuration for building a Raydium CLMM swap.
#[derive(Clone, Debug)]
pub struct ClmmSwapConfig {
    /// The CLMM pool address.
    pub pool_address: Pubkey,
    /// Amount of input tokens to swap (in smallest unit / lamports).
    pub amount_in: u64,
    /// True = swap token_0 → token_1 (a_to_b).
    /// False = swap token_1 → token_0 (b_to_a).
    pub a_to_b: bool,
    /// Slippage tolerance in basis points (e.g. 50 = 0.5%).
    pub slippage_bps: u16,
    /// User's associated token account for the input token.
    pub user_input_token: Pubkey,
    /// User's associated token account for the output token.
    pub user_output_token: Pubkey,
    /// User authority (signer).
    pub user_authority: Pubkey,
    /// Optional additional address-lookup-table addresses for V0 compression.
    pub lookup_table_addresses: Vec<Pubkey>,
    /// Jito tip amount in lamports.
    pub tip_lamports: u64,
    /// Optional explicit tip account; random if None.
    pub tip_account: Option<Pubkey>,
}

/// Output from building a CLMM swap.
#[derive(Clone, Debug)]
pub struct ClmmSwapOutput {
    /// The swap instruction (without tip).
    pub swap_instruction: Instruction,
    /// The computed minimum output amount (after slippage).
    pub min_amount_out: u64,
    /// The expected output amount (before slippage).
    pub expected_amount_out: u64,
    /// The sqrt_price_limit used in the instruction.
    pub sqrt_price_limit_x64: u128,
    /// The pool state used for the computation.
    pub pool_state: ClmmPoolState,
    /// Derived tick array addresses [current, next_0, next_1].
    pub tick_arrays: [Pubkey; 3],
}

/// A fully-assembled swap result, ready for `engine.send_bundle()`.
#[derive(Debug)]
pub struct ClmmVersionedSwap {
    /// The signed V0 transaction with swap + Jito tip.
    pub transaction: VersionedTransaction,
    /// The computed minimum output amount (after slippage).
    pub min_amount_out: u64,
    /// Expected output before slippage.
    pub expected_amount_out: u64,
    /// Tick arrays used in the swap.
    pub tick_arrays: [Pubkey; 3],
}

// ═══════════════════════════════════════════════════════════════════════════
//  Swap builder
// ═══════════════════════════════════════════════════════════════════════════

/// Builder for Raydium CLMM swaps integrated with the Jito BAM execution engine.
///
/// # Usage
///
/// ```rust,no_run
/// use std::collections::VecDeque;
/// use solana_client::rpc_client::RpcClient;
/// use solana_sdk::{pubkey::Pubkey, signature::Keypair, signer::Signer};
/// use jito_bam_client::raydium_clmm::{RaydiumClmmSwapBuilder, ClmmSwapConfig};
///
/// # fn example() -> jito_bam_client::error::Result<()> {
/// let rpc = RpcClient::new("https://api.mainnet-beta.solana.com".to_string());
/// let builder = RaydiumClmmSwapBuilder::new(&rpc);
///
/// let config = ClmmSwapConfig {
///     pool_address: Pubkey::new_unique(),
///     amount_in: 1_000_000,
///     a_to_b: true,
///     slippage_bps: 50,
///     user_input_token: Pubkey::new_unique(),
///     user_output_token: Pubkey::new_unique(),
///     user_authority: Pubkey::new_unique(),
///     lookup_table_addresses: vec![],
///     tip_lamports: 50_000,
///     tip_account: None,
/// };
///
/// let swap_output = builder.build_swap_instruction(&config)?;
/// // swap_output.swap_instruction is ready to include in a transaction
/// # Ok(())
/// # }
/// ```
pub struct RaydiumClmmSwapBuilder<'a> {
    rpc: &'a RpcClient,
}

impl<'a> RaydiumClmmSwapBuilder<'a> {
    /// Create a new builder backed by the given Solana RPC client.
    pub fn new(rpc: &'a RpcClient) -> Self {
        Self { rpc }
    }

    // ── Pool state fetch ────────────────────────────────────────────────

    /// Fetch and zero-copy deserialize the CLMM pool state from on-chain.
    #[instrument(skip_all, fields(pool = %pool_address))]
    pub fn fetch_pool_state(&self, pool_address: &Pubkey) -> Result<ClmmPoolState> {
        let account = self
            .rpc
            .get_account(pool_address)
            .map_err(|e| JitoBamError::RpcError(format!("fetch pool {pool_address}: {e}")))?;

        debug!(
            data_len = account.data.len(),
            owner = %account.owner,
            "fetched CLMM pool account"
        );

        let program_id = Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).unwrap();
        if account.owner != program_id {
            return Err(JitoBamError::RaydiumClmmError(format!(
                "account {pool_address} is not owned by Raydium CLMM program \
                 (owner: {}, expected: {program_id})",
                account.owner
            )));
        }

        ClmmPoolState::from_account_data(&account.data)
    }

    // ── Tick array derivation ───────────────────────────────────────────

    /// Derive the 3 tick-array PDA addresses required by the swap instruction.
    ///
    /// The CLMM program requires 3 tick arrays: the one containing the
    /// current tick, and the next two in the swap direction.
    #[instrument(skip_all, fields(tick = tick_current, spacing = tick_spacing, a_to_b))]
    pub fn derive_tick_arrays(
        pool_address: &Pubkey,
        tick_current: i32,
        tick_spacing: u16,
        a_to_b: bool,
    ) -> [Pubkey; 3] {
        let program_id = Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).unwrap();
        let spacing = tick_spacing as i32;
        let ticks_in_array = TICK_ARRAY_SIZE * spacing;

        // Start index of the tick array containing the current tick
        let current_start = if tick_current >= 0 {
            (tick_current / ticks_in_array) * ticks_in_array
        } else {
            // For negative ticks, floor division
            ((tick_current - ticks_in_array + 1) / ticks_in_array) * ticks_in_array
        };

        let direction: i32 = if a_to_b { -1 } else { 1 };

        let starts = [
            current_start,
            current_start + direction * ticks_in_array,
            current_start + direction * 2 * ticks_in_array,
        ];

        let mut result = [Pubkey::default(); 3];
        for (i, &start_index) in starts.iter().enumerate() {
            let (pda, _bump) = Pubkey::find_program_address(
                &[
                    b"tick_array",
                    pool_address.as_ref(),
                    &start_index.to_le_bytes(),
                ],
                &program_id,
            );
            result[i] = pda;
            debug!(
                index = i,
                start_tick = start_index,
                pda = %pda,
                "derived tick array PDA"
            );
        }

        result
    }

    // ── Price → expected output ─────────────────────────────────────────

    /// Estimate the expected output amount from the pool's current
    /// `sqrt_price_x64` for a `SwapBaseIn` with `amount_in`.
    ///
    /// This is a **first-order approximation** — it uses the spot price
    /// at the current tick and does not walk through tick crossings.
    /// The actual on-chain swap may cross ticks and yield a slightly
    /// different amount; the slippage tolerance covers this gap.
    ///
    /// # Price derivation from sqrt_price_x64
    ///
    /// ```text
    /// sqrt_price_x64 = sqrt(price) * 2^64
    /// price = (sqrt_price_x64 / 2^64)^2 = sqrt_price_x64^2 / 2^128
    ///
    /// For a_to_b (selling token_0 for token_1):
    ///   output_1 = amount_in_0 * price
    ///            = amount_in_0 * sqrt_price_x64^2 / 2^128
    ///
    /// For b_to_a (selling token_1 for token_0):
    ///   output_0 = amount_in_1 / price
    ///            = amount_in_1 * 2^128 / sqrt_price_x64^2
    /// ```
    ///
    /// Decimal adjustment is applied for token decimal differences.
    pub fn estimate_output(
        pool: &ClmmPoolState,
        amount_in: u64,
        a_to_b: bool,
    ) -> Result<u64> {
        if pool.sqrt_price_x64 == 0 {
            return Err(JitoBamError::RaydiumClmmError(
                "pool sqrt_price_x64 is zero — pool may be uninitialised".into(),
            ));
        }

        let amount = amount_in as u128;
        let sqrt_price = pool.sqrt_price_x64;

        // Q64.64 fixed-point: price = sqrt_price^2 / 2^128
        // We use u128 intermediate math to avoid overflow where possible.
        let output = if a_to_b {
            // Selling token_0 for token_1.
            // output_1 = amount_0 * sqrt_price^2 / 2^128
            // Split: (amount * sqrt_price / 2^64) * sqrt_price / 2^64
            let step1 = amount
                .checked_mul(sqrt_price)
                .ok_or_else(|| {
                    JitoBamError::RaydiumClmmError(
                        "overflow in output estimate step1 (a_to_b)".into(),
                    )
                })?
                >> 64;
            let step2 = step1
                .checked_mul(sqrt_price)
                .ok_or_else(|| {
                    JitoBamError::RaydiumClmmError(
                        "overflow in output estimate step2 (a_to_b)".into(),
                    )
                })?
                >> 64;

            // Decimal adjustment: if decimals_0 > decimals_1, divide; else multiply
            let dec_diff =
                pool.mint_decimals_0 as i32 - pool.mint_decimals_1 as i32;
            adjust_decimals(step2, dec_diff)
        } else {
            // Selling token_1 for token_0.
            // output_0 = amount_1 * 2^128 / sqrt_price^2
            // Split: (amount * 2^64 / sqrt_price) * 2^64 / sqrt_price
            let step1 = (amount << 64)
                .checked_div(sqrt_price)
                .ok_or_else(|| {
                    JitoBamError::RaydiumClmmError(
                        "division by zero in output estimate step1 (b_to_a)".into(),
                    )
                })?;
            let step2 = (step1 << 64).checked_div(sqrt_price).ok_or_else(|| {
                JitoBamError::RaydiumClmmError(
                    "overflow in output estimate step2 (b_to_a)".into(),
                )
            })?;

            let dec_diff =
                pool.mint_decimals_1 as i32 - pool.mint_decimals_0 as i32;
            adjust_decimals(step2, dec_diff)
        };

        // Clamp to u64
        if output > u64::MAX as u128 {
            return Err(JitoBamError::RaydiumClmmError(
                "estimated output exceeds u64::MAX".into(),
            ));
        }

        Ok(output as u64)
    }

    // ── Build the swap instruction ──────────────────────────────────────

    /// Build a complete `ClmmSwapOutput` by:
    /// 1. Fetching the on-chain pool state (zero-copy)
    /// 2. Estimating expected output from `sqrt_price_x64`
    /// 3. Applying slippage to compute `min_amount_out`
    /// 4. Deriving the 3 tick-array PDAs
    /// 5. Assembling the 16-account instruction
    #[instrument(skip_all, fields(
        pool = %config.pool_address,
        amount_in = config.amount_in,
        slippage_bps = config.slippage_bps,
        a_to_b = config.a_to_b
    ))]
    pub fn build_swap_instruction(
        &self,
        config: &ClmmSwapConfig,
    ) -> Result<ClmmSwapOutput> {
        // Validate slippage
        if config.slippage_bps > MAX_SLIPPAGE_BPS {
            return Err(JitoBamError::RaydiumClmmError(format!(
                "slippage {}bps exceeds maximum {MAX_SLIPPAGE_BPS}bps",
                config.slippage_bps
            )));
        }

        // 1. Fetch pool state
        let pool = self.fetch_pool_state(&config.pool_address)?;
        info!(
            sqrt_price = pool.sqrt_price_x64,
            tick = pool.tick_current,
            liquidity = pool.liquidity,
            spacing = pool.tick_spacing,
            "pool state fetched"
        );

        // 2. Estimate expected output
        let expected_out = Self::estimate_output(&pool, config.amount_in, config.a_to_b)?;

        // 3. Apply slippage
        let min_out = apply_slippage(expected_out, config.slippage_bps);
        info!(
            expected = expected_out,
            min_out,
            slippage_bps = config.slippage_bps,
            "slippage-protected min_amount_out"
        );

        // 4. Derive tick arrays
        let tick_arrays = Self::derive_tick_arrays(
            &config.pool_address,
            pool.tick_current,
            pool.tick_spacing,
            config.a_to_b,
        );

        // 5. Compute sqrt_price_limit_x64
        let sqrt_price_limit_x64 = if config.a_to_b {
            MIN_SQRT_PRICE_X64
        } else {
            MAX_SQRT_PRICE_X64
        };

        // 6. Determine input/output vaults and mints
        let (input_vault, output_vault, input_mint, output_mint) = if config.a_to_b {
            (
                pool.token_vault_0,
                pool.token_vault_1,
                pool.token_mint_0,
                pool.token_mint_1,
            )
        } else {
            (
                pool.token_vault_1,
                pool.token_vault_0,
                pool.token_mint_1,
                pool.token_mint_0,
            )
        };

        // 7. Build instruction
        let program_id = Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).unwrap();
        let token_program = Pubkey::from_str(TOKEN_PROGRAM_ID).unwrap();
        let token_2022 = Pubkey::from_str(TOKEN_2022_PROGRAM_ID).unwrap();
        let memo_program = Pubkey::from_str(MEMO_PROGRAM_ID).unwrap();

        let accounts = vec![
            // 0: payer / user authority (signer)
            AccountMeta::new_readonly(config.user_authority, true),
            // 1: amm_config
            AccountMeta::new_readonly(pool.amm_config, false),
            // 2: pool_state (writable)
            AccountMeta::new(config.pool_address, false),
            // 3: input_token_account (user's, writable)
            AccountMeta::new(config.user_input_token, false),
            // 4: output_token_account (user's, writable)
            AccountMeta::new(config.user_output_token, false),
            // 5: input_vault (pool's, writable)
            AccountMeta::new(input_vault, false),
            // 6: output_vault (pool's, writable)
            AccountMeta::new(output_vault, false),
            // 7: observation_state (writable)
            AccountMeta::new(pool.observation_key, false),
            // 8: token_program (SPL Token)
            AccountMeta::new_readonly(token_program, false),
            // 9: token_program_2022
            AccountMeta::new_readonly(token_2022, false),
            // 10: memo_program
            AccountMeta::new_readonly(memo_program, false),
            // 11: input_vault_mint (read-only)
            AccountMeta::new_readonly(input_mint, false),
            // 12: output_vault_mint (read-only)
            AccountMeta::new_readonly(output_mint, false),
            // 13–15: tick arrays (writable)
            AccountMeta::new(tick_arrays[0], false),
            AccountMeta::new(tick_arrays[1], false),
            AccountMeta::new(tick_arrays[2], false),
        ];

        let data = encode_swap_data(
            config.amount_in,
            min_out,
            sqrt_price_limit_x64,
        );

        let instruction = Instruction {
            program_id,
            accounts,
            data,
        };

        debug!(
            account_count = 16,
            data_len = instruction.data.len(),
            "swap instruction assembled"
        );

        Ok(ClmmSwapOutput {
            swap_instruction: instruction,
            min_amount_out: min_out,
            expected_amount_out: expected_out,
            sqrt_price_limit_x64,
            pool_state: pool,
            tick_arrays,
        })
    }

    // ── Full versioned transaction builder ──────────────────────────────

    /// Build a signed `VersionedTransaction` (V0) containing:
    /// 1. The Raydium CLMM swap instruction
    /// 2. A Jito tip instruction (appended last for BAM priority)
    ///
    /// Optionally resolves address-lookup-tables for V0 compression.
    ///
    /// The returned transaction is ready for `engine.send_bundle(vec![tx])`.
    #[instrument(skip_all, fields(
        pool = %config.pool_address,
        amount_in = config.amount_in,
        tip = config.tip_lamports
    ))]
    pub fn build_versioned_transaction(
        &self,
        config: &ClmmSwapConfig,
        payer: &Keypair,
        recent_blockhash: Hash,
    ) -> Result<ClmmVersionedSwap> {
        // 1. Build swap instruction
        let swap_output = self.build_swap_instruction(config)?;

        // 2. Build Jito tip instruction (final instruction for BAM priority)
        let tip_ix = tip::build_tip_instruction(
            &payer.pubkey(),
            config.tip_lamports,
            config.tip_account,
        )?;

        let instructions = vec![swap_output.swap_instruction.clone(), tip_ix];

        // 3. Resolve address lookup tables (if any)
        let alts = if config.lookup_table_addresses.is_empty() {
            vec![]
        } else {
            resolve_lookup_tables(self.rpc, &config.lookup_table_addresses)?
        };

        // 4. Compile into V0 message
        let message = v0::Message::try_compile(
            &payer.pubkey(),
            &instructions,
            &alts,
            recent_blockhash,
        )
        .map_err(|e| JitoBamError::CompileError(e.to_string()))?;

        let versioned_message = VersionedMessage::V0(message);

        // 5. Sign
        let tx = VersionedTransaction::try_new(versioned_message, &[payer])
            .map_err(JitoBamError::Signing)?;

        // 6. Size guard
        let serialized = bincode::serialize(&tx)
            .map_err(|e| JitoBamError::Serialization(e.to_string()))?;

        if serialized.len() > 1_232 {
            return Err(JitoBamError::TransactionTooLarge {
                size: serialized.len(),
                max: 1_232,
            });
        }

        info!(
            tx_size = serialized.len(),
            min_out = swap_output.min_amount_out,
            expected_out = swap_output.expected_amount_out,
            "versioned transaction built"
        );

        Ok(ClmmVersionedSwap {
            transaction: tx,
            min_amount_out: swap_output.min_amount_out,
            expected_amount_out: swap_output.expected_amount_out,
            tick_arrays: swap_output.tick_arrays,
        })
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  Helper functions
// ═══════════════════════════════════════════════════════════════════════════

/// Encode the `SwapBaseIn` instruction data.
///
/// Layout:
/// - `[0..8]`   Anchor discriminator
/// - `[8..16]`  amount_in (u64 LE)
/// - `[16..24]` min_amount_out (u64 LE)
/// - `[24..40]` sqrt_price_limit_x64 (u128 LE)
pub fn encode_swap_data(
    amount_in: u64,
    min_amount_out: u64,
    sqrt_price_limit_x64: u128,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(40);
    data.extend_from_slice(&SWAP_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit_x64.to_le_bytes());
    data
}

/// Apply slippage tolerance to an expected output.
///
/// Returns `expected * (10_000 - slippage_bps) / 10_000`, using u128
/// intermediate arithmetic to avoid overflow.
pub fn apply_slippage(expected: u64, slippage_bps: u16) -> u64 {
    if slippage_bps >= 10_000 {
        return 0;
    }
    let factor = 10_000u128 - slippage_bps as u128;
    ((expected as u128) * factor / 10_000u128) as u64
}

/// Adjust a raw amount for decimal difference between input and output tokens.
///
/// If `dec_diff > 0`, the input token has more decimals → divide.
/// If `dec_diff < 0`, the input token has fewer decimals → multiply.
fn adjust_decimals(amount: u128, dec_diff: i32) -> u128 {
    if dec_diff > 0 {
        amount / 10u128.pow(dec_diff as u32)
    } else if dec_diff < 0 {
        amount * 10u128.pow((-dec_diff) as u32)
    } else {
        amount
    }
}

/// Derive an associated token account address.
///
/// `ata = PDA(["", wallet, token_program, mint], ATA_PROGRAM)`
pub fn derive_ata(wallet: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let ata_program =
        Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap();
    let (ata, _bump) = Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_program,
    );
    ata
}

/// Convert a tick index to its approximate price.
///
/// `price = 1.0001^tick`
pub fn tick_to_price(tick: i32) -> f64 {
    1.0001_f64.powi(tick)
}

/// Convert `sqrt_price_x64` to a human-readable price.
///
/// `price = (sqrt_price_x64 / 2^64)^2`
pub fn sqrt_price_x64_to_price(sqrt_price_x64: u128, decimals_0: u8, decimals_1: u8) -> f64 {
    let sqrt_price = sqrt_price_x64 as f64 / (1u128 << 64) as f64;
    let raw_price = sqrt_price * sqrt_price;
    let decimal_adj = 10f64.powi(decimals_0 as i32 - decimals_1 as i32);
    raw_price * decimal_adj
}

/// Resolve on-chain address lookup table accounts for V0 transaction compression.
fn resolve_lookup_tables(
    rpc: &RpcClient,
    addresses: &[Pubkey],
) -> Result<Vec<AddressLookupTableAccount>> {
    let mut tables = Vec::with_capacity(addresses.len());

    for addr in addresses {
        let account = rpc
            .get_account(addr)
            .map_err(|e| JitoBamError::RpcError(format!("fetch ALT {addr}: {e}")))?;

        // Deserialize the ALT. The on-chain format has a 56-byte header
        // followed by packed 32-byte pubkey entries.
        let alt = deserialize_alt_account(addr, &account.data)?;
        tables.push(alt);
    }

    debug!(count = tables.len(), "resolved address lookup tables");
    Ok(tables)
}

/// Deserialize an address lookup table from raw account data.
///
/// Layout (simplified):
/// - `[0..4]`   type_index (u32 LE, must be 1 = LookupTable)
/// - `[4..12]`  deactivation slot (u64 LE)
/// - `[12..20]` last extended slot (u64 LE)
/// - `[20..21]` last extended slot start index (u8)
/// - `[21..22]` has authority flag (u8)
/// - `[22..56]` authority or padding (32 bytes + 2 padding)
/// - `[56..]`   packed addresses (n × 32 bytes)
fn deserialize_alt_account(
    key: &Pubkey,
    data: &[u8],
) -> Result<AddressLookupTableAccount> {
    const HEADER_SIZE: usize = 56;
    if data.len() < HEADER_SIZE {
        return Err(JitoBamError::RaydiumClmmError(format!(
            "ALT account {key} too small: {} bytes (need >= {HEADER_SIZE})",
            data.len()
        )));
    }

    let addr_data = &data[HEADER_SIZE..];
    if addr_data.len() % 32 != 0 {
        return Err(JitoBamError::RaydiumClmmError(format!(
            "ALT {key} address section not aligned: {} bytes",
            addr_data.len()
        )));
    }

    let addresses: Vec<Pubkey> = addr_data
        .chunks_exact(32)
        .map(|chunk| {
            let bytes: [u8; 32] = chunk.try_into().unwrap();
            Pubkey::new_from_array(bytes)
        })
        .collect();

    Ok(AddressLookupTableAccount {
        key: *key,
        addresses,
    })
}

// ═══════════════════════════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── Instruction encoding ────────────────────────────────────────────

    #[test]
    fn encode_swap_data_correct_length() {
        let data = encode_swap_data(1_000_000, 500_000, MIN_SQRT_PRICE_X64);
        assert_eq!(data.len(), 40, "8 discriminator + 8 amount + 8 min + 16 sqrt");
    }

    #[test]
    fn encode_swap_data_discriminator() {
        let data = encode_swap_data(0, 0, 0);
        assert_eq!(&data[..8], &SWAP_DISCRIMINATOR);
    }

    #[test]
    fn encode_swap_data_roundtrip() {
        let amount_in = 123_456_789u64;
        let min_out = 987_654u64;
        let sqrt_limit = 12345678901234567890u128;

        let data = encode_swap_data(amount_in, min_out, sqrt_limit);

        let decoded_amount = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let decoded_min = u64::from_le_bytes(data[16..24].try_into().unwrap());
        let decoded_sqrt = u128::from_le_bytes(data[24..40].try_into().unwrap());

        assert_eq!(decoded_amount, amount_in);
        assert_eq!(decoded_min, min_out);
        assert_eq!(decoded_sqrt, sqrt_limit);
    }

    // ── Slippage ────────────────────────────────────────────────────────

    #[test]
    fn slippage_50bps() {
        // 0.5% slippage on 1_000_000 → 995_000
        assert_eq!(apply_slippage(1_000_000, 50), 995_000);
    }

    #[test]
    fn slippage_100bps() {
        // 1% slippage on 1_000_000 → 990_000
        assert_eq!(apply_slippage(1_000_000, 100), 990_000);
    }

    #[test]
    fn slippage_zero() {
        assert_eq!(apply_slippage(1_000_000, 0), 1_000_000);
    }

    #[test]
    fn slippage_10000bps_returns_zero() {
        // 100% slippage → 0
        assert_eq!(apply_slippage(1_000_000, 10_000), 0);
    }

    #[test]
    fn slippage_no_overflow_large_amount() {
        let large = u64::MAX;
        let result = apply_slippage(large, 50);
        // (u64::MAX * 9950) / 10000
        let expected = ((large as u128) * 9950 / 10_000) as u64;
        assert_eq!(result, expected);
    }

    // ── Tick-to-price ───────────────────────────────────────────────────

    #[test]
    fn tick_zero_is_price_one() {
        let price = tick_to_price(0);
        assert!((price - 1.0).abs() < 1e-12);
    }

    #[test]
    fn tick_positive_price_greater_than_one() {
        let price = tick_to_price(100);
        assert!(price > 1.0);
    }

    #[test]
    fn tick_negative_price_less_than_one() {
        let price = tick_to_price(-100);
        assert!(price < 1.0);
    }

    // ── sqrt_price_x64_to_price ─────────────────────────────────────────

    #[test]
    fn sqrt_price_x64_identity() {
        // sqrt_price_x64 = 2^64 means sqrt(price) = 1, so price = 1
        let sqrt_1 = 1u128 << 64;
        let price = sqrt_price_x64_to_price(sqrt_1, 9, 9);
        assert!((price - 1.0).abs() < 1e-6);
    }

    #[test]
    fn sqrt_price_x64_with_decimal_adjustment() {
        let sqrt_1 = 1u128 << 64;
        // decimals_0 = 9 (SOL), decimals_1 = 6 (USDC) → multiply by 10^3
        let price = sqrt_price_x64_to_price(sqrt_1, 9, 6);
        assert!((price - 1000.0).abs() < 1.0);
    }

    // ── Zero-copy pool state ────────────────────────────────────────────

    #[test]
    fn pool_state_rejects_short_data() {
        let data = vec![0u8; 100]; // too short
        let result = ClmmPoolState::from_account_data(&data);
        assert!(result.is_err());
    }

    #[test]
    fn pool_state_from_synthetic_data() {
        // Build a synthetic 1600-byte buffer with known values
        let mut data = vec![0u8; 1600];

        // Write amm_config at offset 9 (32 bytes)
        let amm_config = Pubkey::new_unique();
        data[9..41].copy_from_slice(amm_config.as_ref());

        // Write token_mint_0 at offset 73
        let mint0 = Pubkey::new_unique();
        data[73..105].copy_from_slice(mint0.as_ref());

        // Write token_mint_1 at offset 105
        let mint1 = Pubkey::new_unique();
        data[105..137].copy_from_slice(mint1.as_ref());

        // Write token_vault_0 at offset 137
        let vault0 = Pubkey::new_unique();
        data[137..169].copy_from_slice(vault0.as_ref());

        // Write token_vault_1 at offset 169
        let vault1 = Pubkey::new_unique();
        data[169..201].copy_from_slice(vault1.as_ref());

        // Write observation_key at offset 201
        let obs = Pubkey::new_unique();
        data[201..233].copy_from_slice(obs.as_ref());

        // Decimals
        data[233] = 9;  // mint_decimals_0
        data[234] = 6;  // mint_decimals_1

        // tick_spacing at offset 235 (u16 LE)
        data[235..237].copy_from_slice(&10u16.to_le_bytes());

        // liquidity at offset 237 (u128 LE)
        let liq = 500_000_000u128;
        data[237..253].copy_from_slice(&liq.to_le_bytes());

        // sqrt_price_x64 at offset 253 (u128 LE)
        let sqrt_p = (1u128 << 64) * 12; // sqrt(price) ≈ 12
        data[253..269].copy_from_slice(&sqrt_p.to_le_bytes());

        // tick_current at offset 269 (i32 LE)
        let tick = -1234i32;
        data[269..273].copy_from_slice(&tick.to_le_bytes());

        let pool = ClmmPoolState::from_account_data(&data).unwrap();
        assert_eq!(pool.amm_config, amm_config);
        assert_eq!(pool.token_mint_0, mint0);
        assert_eq!(pool.token_mint_1, mint1);
        assert_eq!(pool.token_vault_0, vault0);
        assert_eq!(pool.token_vault_1, vault1);
        assert_eq!(pool.observation_key, obs);
        assert_eq!(pool.mint_decimals_0, 9);
        assert_eq!(pool.mint_decimals_1, 6);
        assert_eq!(pool.tick_spacing, 10);
        assert_eq!(pool.liquidity, liq);
        assert_eq!(pool.sqrt_price_x64, sqrt_p);
        assert_eq!(pool.tick_current, tick);
    }

    // ── Tick array derivation ───────────────────────────────────────────

    #[test]
    fn tick_arrays_are_deterministic() {
        let pool = Pubkey::new_unique();
        let a1 = RaydiumClmmSwapBuilder::derive_tick_arrays(&pool, 100, 10, true);
        let a2 = RaydiumClmmSwapBuilder::derive_tick_arrays(&pool, 100, 10, true);
        assert_eq!(a1, a2);
    }

    #[test]
    fn tick_arrays_differ_by_direction() {
        let pool = Pubkey::new_unique();
        let forward = RaydiumClmmSwapBuilder::derive_tick_arrays(&pool, 100, 10, true);
        let backward = RaydiumClmmSwapBuilder::derive_tick_arrays(&pool, 100, 10, false);
        // Arrays 1 and 2 should differ (different direction)
        assert_ne!(forward[1], backward[1]);
    }

    #[test]
    fn tick_arrays_three_distinct() {
        let pool = Pubkey::new_unique();
        let arrays = RaydiumClmmSwapBuilder::derive_tick_arrays(&pool, 0, 1, true);
        assert_ne!(arrays[0], arrays[1]);
        assert_ne!(arrays[1], arrays[2]);
        assert_ne!(arrays[0], arrays[2]);
    }

    #[test]
    fn tick_arrays_negative_tick() {
        let pool = Pubkey::new_unique();
        // Should not panic with negative ticks
        let arrays = RaydiumClmmSwapBuilder::derive_tick_arrays(&pool, -5000, 10, true);
        assert_ne!(arrays[0], Pubkey::default());
    }

    // ── Estimate output ─────────────────────────────────────────────────

    #[test]
    fn estimate_output_equal_decimals() {
        // sqrt_price_x64 = 2^64 → price = 1.0
        // Swapping 1_000_000 token_0 should yield ~1_000_000 token_1
        let pool = ClmmPoolState {
            amm_config: Pubkey::new_unique(),
            token_mint_0: Pubkey::new_unique(),
            token_mint_1: Pubkey::new_unique(),
            token_vault_0: Pubkey::new_unique(),
            token_vault_1: Pubkey::new_unique(),
            observation_key: Pubkey::new_unique(),
            mint_decimals_0: 9,
            mint_decimals_1: 9,
            tick_spacing: 10,
            liquidity: 1_000_000_000,
            sqrt_price_x64: 1u128 << 64,
            tick_current: 0,
        };

        let out = RaydiumClmmSwapBuilder::estimate_output(&pool, 1_000_000, true).unwrap();
        assert_eq!(out, 1_000_000, "price = 1 → output should equal input");
    }

    #[test]
    fn estimate_output_different_decimals() {
        // price = 1, but decimals differ: 9 vs 6
        // 1_000_000_000 (9 dec) → should map to ~1_000_000 (6 dec)
        let pool = ClmmPoolState {
            amm_config: Pubkey::new_unique(),
            token_mint_0: Pubkey::new_unique(),
            token_mint_1: Pubkey::new_unique(),
            token_vault_0: Pubkey::new_unique(),
            token_vault_1: Pubkey::new_unique(),
            observation_key: Pubkey::new_unique(),
            mint_decimals_0: 9,
            mint_decimals_1: 6,
            tick_spacing: 10,
            liquidity: 1_000_000_000,
            sqrt_price_x64: 1u128 << 64,
            tick_current: 0,
        };

        let out = RaydiumClmmSwapBuilder::estimate_output(&pool, 1_000_000_000, true).unwrap();
        // With decimal adjustment: output = 1_000_000_000 * 1 / 10^3 = 1_000_000
        assert_eq!(out, 1_000_000);
    }

    #[test]
    fn estimate_output_b_to_a() {
        let pool = ClmmPoolState {
            amm_config: Pubkey::new_unique(),
            token_mint_0: Pubkey::new_unique(),
            token_mint_1: Pubkey::new_unique(),
            token_vault_0: Pubkey::new_unique(),
            token_vault_1: Pubkey::new_unique(),
            observation_key: Pubkey::new_unique(),
            mint_decimals_0: 9,
            mint_decimals_1: 9,
            tick_spacing: 10,
            liquidity: 1_000_000_000,
            sqrt_price_x64: 1u128 << 64,
            tick_current: 0,
        };

        let out = RaydiumClmmSwapBuilder::estimate_output(&pool, 1_000_000, false).unwrap();
        assert_eq!(out, 1_000_000, "price = 1 → output = input (b_to_a)");
    }

    #[test]
    fn estimate_output_zero_sqrt_price_errors() {
        let pool = ClmmPoolState {
            amm_config: Pubkey::new_unique(),
            token_mint_0: Pubkey::new_unique(),
            token_mint_1: Pubkey::new_unique(),
            token_vault_0: Pubkey::new_unique(),
            token_vault_1: Pubkey::new_unique(),
            observation_key: Pubkey::new_unique(),
            mint_decimals_0: 9,
            mint_decimals_1: 6,
            tick_spacing: 10,
            liquidity: 0,
            sqrt_price_x64: 0,
            tick_current: 0,
        };

        let result = RaydiumClmmSwapBuilder::estimate_output(&pool, 1_000_000, true);
        assert!(result.is_err());
    }

    // ── ATA derivation ──────────────────────────────────────────────────

    #[test]
    fn ata_is_deterministic() {
        let wallet = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let token_prog = Pubkey::from_str(TOKEN_PROGRAM_ID).unwrap();

        let ata1 = derive_ata(&wallet, &mint, &token_prog);
        let ata2 = derive_ata(&wallet, &mint, &token_prog);
        assert_eq!(ata1, ata2);
    }

    #[test]
    fn ata_differs_by_mint() {
        let wallet = Pubkey::new_unique();
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        let token_prog = Pubkey::from_str(TOKEN_PROGRAM_ID).unwrap();

        let ata_a = derive_ata(&wallet, &mint_a, &token_prog);
        let ata_b = derive_ata(&wallet, &mint_b, &token_prog);
        assert_ne!(ata_a, ata_b);
    }

    // ── Decimal adjustment ──────────────────────────────────────────────

    #[test]
    fn adjust_decimals_positive() {
        assert_eq!(adjust_decimals(1_000_000_000, 3), 1_000_000);
    }

    #[test]
    fn adjust_decimals_negative() {
        assert_eq!(adjust_decimals(1_000_000, -3), 1_000_000_000);
    }

    #[test]
    fn adjust_decimals_zero() {
        assert_eq!(adjust_decimals(42, 0), 42);
    }

    // ── ALT deserialization ─────────────────────────────────────────────

    #[test]
    fn alt_rejects_short_data() {
        let key = Pubkey::new_unique();
        let data = vec![0u8; 20]; // too short
        assert!(deserialize_alt_account(&key, &data).is_err());
    }

    #[test]
    fn alt_empty_addresses() {
        let key = Pubkey::new_unique();
        let data = vec![0u8; 56]; // header only, no addresses
        let alt = deserialize_alt_account(&key, &data).unwrap();
        assert_eq!(alt.key, key);
        assert!(alt.addresses.is_empty());
    }

    #[test]
    fn alt_with_addresses() {
        let key = Pubkey::new_unique();
        let addr1 = Pubkey::new_unique();
        let addr2 = Pubkey::new_unique();

        let mut data = vec![0u8; 56]; // header
        data.extend_from_slice(addr1.as_ref());
        data.extend_from_slice(addr2.as_ref());

        let alt = deserialize_alt_account(&key, &data).unwrap();
        assert_eq!(alt.addresses.len(), 2);
        assert_eq!(alt.addresses[0], addr1);
        assert_eq!(alt.addresses[1], addr2);
    }

    #[test]
    fn alt_rejects_unaligned() {
        let key = Pubkey::new_unique();
        let mut data = vec![0u8; 56];
        data.extend_from_slice(&[1, 2, 3]); // 3 bytes → not aligned to 32
        assert!(deserialize_alt_account(&key, &data).is_err());
    }

    // ── Constants ───────────────────────────────────────────────────────

    #[test]
    fn sqrt_price_boundaries() {
        assert!(MIN_SQRT_PRICE_X64 < MAX_SQRT_PRICE_X64);
        assert!(MIN_SQRT_PRICE_X64 > 0);
    }

    #[test]
    fn program_ids_parse() {
        assert!(Pubkey::from_str(RAYDIUM_CLMM_PROGRAM_ID).is_ok());
        assert!(Pubkey::from_str(TOKEN_PROGRAM_ID).is_ok());
        assert!(Pubkey::from_str(TOKEN_2022_PROGRAM_ID).is_ok());
        assert!(Pubkey::from_str(MEMO_PROGRAM_ID).is_ok());
    }
}
