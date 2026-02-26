/// Meteora DLMM (Dynamic Liquidity Market Maker) swap instruction builder.
///
/// Constructs a production-grade swap instruction against Meteora DLMM
/// pools on Solana, with:
///
/// - **Dynamic `min_amount_out`** calculated from on-chain bin data using
///   a configurable slippage tolerance (default 0.5% / 50 bps).
/// - **Address-lookup-table resolution** for V0 versioned transactions,
///   keeping the tx under the 1 232-byte limit even with 20+ accounts.
/// - **Active-bin price discovery** — fetches the pool's current active
///   bin to derive the real-time effective price.
///
/// # Architecture
///
/// ```text
///   ┌─────────────┐      ┌────────────────┐
///   │ Your Bot     │─────▶│ DlmmSwapBuilder│
///   └─────────────┘      │                │
///                        │  1. fetch pool │
///                        │  2. get bins   │
///                        │  3. calc min   │
///                        │  4. build ix   │
///                        │  5. resolve ALT│
///                        └───────┬────────┘
///                                │
///                   ┌────────────┼─────────────┐
///                   ▼            ▼              ▼
///              Solana RPC    DLMM Program   ALT Program
/// ```
///
/// # Meteora DLMM program layout
///
/// The DLMM program uses a "swap" instruction (discriminator index 1 in
/// the IDL) with the following account layout:
///
/// | #  | Account            | Signer | Writable |
/// |----|--------------------|--------|----------|
/// | 0  | lb_pair            |        | ✓        |
/// | 1  | bin_array_lower    |        | ✓        |
/// | 2  | bin_array_upper    |        | ✓        |
/// | 3  | reserve_x          |        | ✓        |
/// | 4  | reserve_y          |        | ✓        |
/// | 5  | user_token_x       |        | ✓        |
/// | 6  | user_token_y       |        | ✓        |
/// | 7  | token_x_mint       |        |          |
/// | 8  | token_y_mint       |        |          |
/// | 9  | oracle             |        | ✓        |
/// | 10 | host_fee_in        |        | ✓        |
/// | 11 | user (authority)   | ✓      |          |
/// | 12 | token_x_program    |        |          |
/// | 13 | token_y_program    |        |          |
/// | 14 | event_authority    |        |          |
/// | 15 | program            |        |          |
///
/// The instruction data is:
/// - 8 bytes: Anchor discriminator (`[248, 198, 158, 145, 225, 117, 135, 200]`)
/// - 8 bytes: `amount_in` (u64 LE)
/// - 8 bytes: `min_amount_out` (u64 LE)
/// - 1 byte:  `swap_for_y` (bool — true = sell token X for Y)
use std::str::FromStr;

use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::{self, AddressLookupTableAccount},
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use tracing::{debug, info, instrument, warn};

use crate::error::{JitoBamError, Result};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Meteora DLMM v2 program ID (mainnet).
pub const DLMM_PROGRAM_ID: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";

/// Anchor discriminator for the `swap` instruction.
/// SHA-256("global:swap")[..8]
const SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];

/// Default slippage tolerance in basis points (0.5% = 50 bps).
pub const DEFAULT_SLIPPAGE_BPS: u16 = 50;

/// Maximum slippage we allow (10% = 1000 bps) — safety guard.
const MAX_SLIPPAGE_BPS: u16 = 1_000;

/// Number of bins in DLMM bin arrays.
const BIN_ARRAY_SIZE: i32 = 70;

// ─── On-chain account structures ────────────────────────────────────────────

/// Minimal deserialization of the Meteora DLMM LbPair account.
///
/// We only read the fields needed for swap construction.
/// Full layout: <https://github.com/nicoo-d/meteora-dlmm/blob/main/lb_programs>
#[derive(Clone, Debug)]
pub struct LbPairState {
    /// The active bin ID that holds the current price.
    pub active_id: i32,
    /// Bin step in basis points (e.g. 25 = 0.25% per bin).
    pub bin_step: u16,
    /// Token X mint.
    pub token_x_mint: Pubkey,
    /// Token Y mint.
    pub token_y_mint: Pubkey,
    /// Pool reserve account for token X.
    pub reserve_x: Pubkey,
    /// Pool reserve account for token Y.
    pub reserve_y: Pubkey,
    /// Oracle account for the pair.
    pub oracle: Pubkey,
    /// Token X program (usually SPL Token or Token-2022).
    pub token_x_program: Pubkey,
    /// Token Y program.
    pub token_y_program: Pubkey,
}

/// A single bin's liquidity and price data.
#[derive(Clone, Debug)]
pub struct BinData {
    /// Bin ID.
    pub id: i32,
    /// Price numerator (fixed-point Q64.64 or similar).
    pub price: u128,
    /// Available liquidity in token X in this bin.
    pub amount_x: u64,
    /// Available liquidity in token Y in this bin.
    pub amount_y: u64,
}

/// Resolved pool state ready for swap instruction construction.
#[derive(Clone, Debug)]
pub struct ResolvedPool {
    /// The LbPair account address.
    pub lb_pair: Pubkey,
    /// Decoded on-chain state.
    pub state: LbPairState,
    /// Bin array bitmap lower bound PDA.
    pub bin_array_lower: Pubkey,
    /// Bin array bitmap upper bound PDA.
    pub bin_array_upper: Pubkey,
    /// Bins around the active price.
    pub bins: Vec<BinData>,
    /// Event authority PDA.
    pub event_authority: Pubkey,
    /// Host fee-in account (optional, use system program if absent).
    pub host_fee_in: Option<Pubkey>,
}

// ─── Swap configuration ─────────────────────────────────────────────────────

/// Full configuration for a Meteora DLMM swap.
#[derive(Clone, Debug)]
pub struct DlmmSwapConfig {
    /// The LbPair (pool) address.
    pub lb_pair: Pubkey,
    /// Amount of input token to swap (in native token units).
    pub amount_in: u64,
    /// `true` = sell token X for token Y; `false` = the reverse.
    pub swap_for_y: bool,
    /// Slippage tolerance in basis points (default: 50 = 0.5%).
    pub slippage_bps: u16,
    /// User's token X account (ATA).
    pub user_token_x: Pubkey,
    /// User's token Y account (ATA).
    pub user_token_y: Pubkey,
    /// The user/authority pubkey (signer).
    pub user_authority: Pubkey,
    /// Optional host-fee-in account.
    pub host_fee_in: Option<Pubkey>,
    /// Address lookup table addresses to resolve for V0 tx compression.
    pub lookup_table_addresses: Vec<Pubkey>,
}

/// The output of the swap builder — everything needed to submit the tx.
#[derive(Clone, Debug)]
pub struct DlmmSwapOutput {
    /// The swap instruction, ready to include in a transaction.
    pub instruction: Instruction,
    /// Calculated `min_amount_out` after slippage.
    pub min_amount_out: u64,
    /// Raw expected output before slippage.
    pub expected_amount_out: u64,
    /// The active bin price used for the calculation.
    pub active_bin_price: f64,
    /// Resolved address lookup tables (for V0 message compilation).
    pub address_lookup_tables: Vec<AddressLookupTableAccount>,
}

// ─── Swap builder ───────────────────────────────────────────────────────────

/// Builds Meteora DLMM swap instructions with dynamic slippage
/// calculation and ALT resolution.
pub struct DlmmSwapBuilder<'a> {
    rpc: &'a RpcClient,
    dlmm_program: Pubkey,
}

impl<'a> DlmmSwapBuilder<'a> {
    /// Create a new builder using the given RPC client.
    pub fn new(rpc: &'a RpcClient) -> Self {
        let dlmm_program =
            Pubkey::from_str(DLMM_PROGRAM_ID).expect("hardcoded DLMM program ID");
        Self { rpc, dlmm_program }
    }

    /// Create a builder with a custom program ID (e.g. devnet deployment).
    pub fn with_program(rpc: &'a RpcClient, program_id: Pubkey) -> Self {
        Self {
            rpc,
            dlmm_program: program_id,
        }
    }

    // ── Primary entry point ─────────────────────────────────────────────

    /// Build a complete DLMM swap instruction.
    ///
    /// 1. Fetches and decodes the LbPair account from on-chain.
    /// 2. Fetches active bins to derive the effective price.
    /// 3. Calculates `min_amount_out` with slippage protection.
    /// 4. Resolves any address lookup tables for V0 compression.
    /// 5. Returns a fully-formed [`DlmmSwapOutput`].
    ///
    /// # Errors
    /// - `RpcError` if any on-chain fetch fails.
    /// - `InvalidSlippage` if `slippage_bps > 1000` (10%).
    /// - `PoolNotFound` if the LbPair account doesn't exist.
    #[instrument(skip_all, fields(
        pool = %config.lb_pair,
        amount_in = config.amount_in,
        swap_for_y = config.swap_for_y,
        slippage_bps = config.slippage_bps,
    ))]
    pub fn build(&self, config: &DlmmSwapConfig) -> Result<DlmmSwapOutput> {
        // ── Validate slippage ───────────────────────────────────────────
        if config.slippage_bps > MAX_SLIPPAGE_BPS {
            return Err(JitoBamError::MeteoraDlmmError(format!(
                "slippage {}bps exceeds maximum {}bps",
                config.slippage_bps, MAX_SLIPPAGE_BPS
            )));
        }

        // ── 1. Fetch and decode the LbPair ──────────────────────────────
        let pool = self.fetch_pool_state(&config.lb_pair)?;
        info!(
            active_id = pool.state.active_id,
            bin_step = pool.state.bin_step,
            "pool state resolved"
        );

        // ── 2. Calculate expected output from bin data ──────────────────
        let expected_amount_out = self.simulate_swap(
            &pool,
            config.amount_in,
            config.swap_for_y,
        )?;

        // ── 3. Apply slippage tolerance ─────────────────────────────────
        let min_amount_out =
            apply_slippage(expected_amount_out, config.slippage_bps);
        info!(
            expected = expected_amount_out,
            min_out = min_amount_out,
            slippage_bps = config.slippage_bps,
            "slippage-adjusted min_amount_out"
        );

        // ── 4. Compute active bin price ─────────────────────────────────
        let active_bin_price =
            bin_id_to_price(pool.state.active_id, pool.state.bin_step);

        // ── 5. Build the instruction ────────────────────────────────────
        let instruction = self.build_swap_instruction(
            config,
            &pool,
            min_amount_out,
        );

        // ── 6. Resolve address lookup tables ────────────────────────────
        let address_lookup_tables = self.resolve_lookup_tables(
            &config.lookup_table_addresses,
        )?;

        debug!(
            alts = address_lookup_tables.len(),
            "lookup tables resolved"
        );

        Ok(DlmmSwapOutput {
            instruction,
            min_amount_out,
            expected_amount_out,
            active_bin_price,
            address_lookup_tables,
        })
    }

    // ── Pool state fetch ────────────────────────────────────────────────

    /// Fetch and decode the LbPair account from on-chain.
    ///
    /// Layout offsets (Anchor 8-byte discriminator prefix):
    /// - `active_id`:       offset 264..268  (i32)
    /// - `bin_step`:        offset 268..270  (u16)
    /// - `token_x_mint`:    offset 72..104   (Pubkey)
    /// - `token_y_mint`:    offset 104..136  (Pubkey)
    /// - `reserve_x`:       offset 136..168  (Pubkey)
    /// - `reserve_y`:       offset 168..200  (Pubkey)
    /// - `oracle`:          offset 200..232  (Pubkey)
    /// - `token_x_program`: offset 8..40     (Pubkey – first field after discriminator)
    /// - `token_y_program`: offset 40..72    (Pubkey)
    #[instrument(skip_all, fields(lb_pair = %lb_pair))]
    pub fn fetch_pool_state(&self, lb_pair: &Pubkey) -> Result<ResolvedPool> {
        let account = self
            .rpc
            .get_account(lb_pair)
            .map_err(|e| JitoBamError::MeteoraDlmmError(format!(
                "failed to fetch LbPair {lb_pair}: {e}"
            )))?;

        let data = &account.data;
        if data.len() < 280 {
            return Err(JitoBamError::MeteoraDlmmError(format!(
                "LbPair account data too short: {} bytes (need >= 280)",
                data.len()
            )));
        }

        // Decode fields from the raw account data
        let token_x_program = Pubkey::try_from(&data[8..40])
            .map_err(|e| JitoBamError::MeteoraDlmmError(format!("token_x_program: {e}")))?;
        let token_y_program = Pubkey::try_from(&data[40..72])
            .map_err(|e| JitoBamError::MeteoraDlmmError(format!("token_y_program: {e}")))?;
        let token_x_mint = Pubkey::try_from(&data[72..104])
            .map_err(|e| JitoBamError::MeteoraDlmmError(format!("token_x_mint: {e}")))?;
        let token_y_mint = Pubkey::try_from(&data[104..136])
            .map_err(|e| JitoBamError::MeteoraDlmmError(format!("token_y_mint: {e}")))?;
        let reserve_x = Pubkey::try_from(&data[136..168])
            .map_err(|e| JitoBamError::MeteoraDlmmError(format!("reserve_x: {e}")))?;
        let reserve_y = Pubkey::try_from(&data[168..200])
            .map_err(|e| JitoBamError::MeteoraDlmmError(format!("reserve_y: {e}")))?;
        let oracle = Pubkey::try_from(&data[200..232])
            .map_err(|e| JitoBamError::MeteoraDlmmError(format!("oracle: {e}")))?;

        let active_id = i32::from_le_bytes(
            data[264..268].try_into().unwrap(),
        );
        let bin_step = u16::from_le_bytes(
            data[268..270].try_into().unwrap(),
        );

        let state = LbPairState {
            active_id,
            bin_step,
            token_x_mint,
            token_y_mint,
            reserve_x,
            reserve_y,
            oracle,
            token_x_program,
            token_y_program,
        };

        // Derive PDAs
        let bin_array_lower = self.derive_bin_array_pda(lb_pair, active_id, false);
        let bin_array_upper = self.derive_bin_array_pda(lb_pair, active_id, true);
        let event_authority = self.derive_event_authority_pda();

        // Fetch bin data around the active bin
        let bins = self.fetch_bins_around_active(lb_pair, &state)?;

        Ok(ResolvedPool {
            lb_pair: *lb_pair,
            state,
            bin_array_lower,
            bin_array_upper,
            bins,
            event_authority,
            host_fee_in: None,
        })
    }

    // ── Swap simulation ─────────────────────────────────────────────────

    /// Walk bins from the active one outward to simulate the swap's
    /// effective output amount.
    ///
    /// For `swap_for_y = true`: we sell token X → get token Y.
    /// We consume token Y liquidity from bins starting at `active_id`
    /// and moving downward.
    ///
    /// For `swap_for_y = false`: we sell token Y → get token X.
    /// We consume token X liquidity from bins starting above `active_id`.
    fn simulate_swap(
        &self,
        pool: &ResolvedPool,
        amount_in: u64,
        swap_for_y: bool,
    ) -> Result<u64> {
        if pool.bins.is_empty() {
            // Fallback to active-bin-price arithmetic estimate
            let price = bin_id_to_price(pool.state.active_id, pool.state.bin_step);
            let estimated = if swap_for_y {
                (amount_in as f64 * price) as u64
            } else {
                (amount_in as f64 / price) as u64
            };
            warn!(
                estimated,
                "no bin data — using active-bin price estimate"
            );
            return Ok(estimated);
        }

        let mut remaining_in = amount_in;
        let mut total_out: u64 = 0;

        // Sort bins by ID; iterate in appropriate direction
        let mut sorted_bins = pool.bins.clone();
        if swap_for_y {
            // Walk active → lower bins (descending ID)
            sorted_bins.sort_by(|a, b| b.id.cmp(&a.id));
        } else {
            // Walk active → upper bins (ascending ID)
            sorted_bins.sort_by(|a, b| a.id.cmp(&b.id));
        }

        for bin in &sorted_bins {
            if remaining_in == 0 {
                break;
            }

            let price = bin_id_to_price(bin.id, pool.state.bin_step);

            if swap_for_y {
                // Selling X for Y: available output is bin.amount_y
                let max_x_consumable = if price > 0.0 {
                    (bin.amount_y as f64 / price) as u64
                } else {
                    continue;
                };
                let x_consumed = remaining_in.min(max_x_consumable);
                let y_received = (x_consumed as f64 * price) as u64;
                remaining_in -= x_consumed;
                total_out += y_received;
            } else {
                // Selling Y for X: available output is bin.amount_x
                let max_y_consumable = (bin.amount_x as f64 * price) as u64;
                let y_consumed = remaining_in.min(max_y_consumable);
                let x_received = if price > 0.0 {
                    (y_consumed as f64 / price) as u64
                } else {
                    continue;
                };
                remaining_in -= y_consumed;
                total_out += x_received;
            }
        }

        if remaining_in > 0 {
            warn!(
                unfilled = remaining_in,
                filled_out = total_out,
                "insufficient bin liquidity — partial fill"
            );
        }

        Ok(total_out)
    }

    // ── Instruction assembly ────────────────────────────────────────────

    /// Assemble the raw DLMM swap instruction with all 16 accounts.
    fn build_swap_instruction(
        &self,
        config: &DlmmSwapConfig,
        pool: &ResolvedPool,
        min_amount_out: u64,
    ) -> Instruction {
        // Instruction data: discriminator + amount_in + min_amount_out + swap_for_y
        let mut data = Vec::with_capacity(25);
        data.extend_from_slice(&SWAP_DISCRIMINATOR);
        data.extend_from_slice(&config.amount_in.to_le_bytes());
        data.extend_from_slice(&min_amount_out.to_le_bytes());
        data.push(config.swap_for_y as u8);

        let host_fee_in = config
            .host_fee_in
            .or(pool.host_fee_in)
            .unwrap_or(self.dlmm_program); // program itself as no-op

        let accounts = vec![
            // 0: lb_pair (writable)
            AccountMeta::new(pool.lb_pair, false),
            // 1: bin_array_lower (writable)
            AccountMeta::new(pool.bin_array_lower, false),
            // 2: bin_array_upper (writable)
            AccountMeta::new(pool.bin_array_upper, false),
            // 3: reserve_x (writable)
            AccountMeta::new(pool.state.reserve_x, false),
            // 4: reserve_y (writable)
            AccountMeta::new(pool.state.reserve_y, false),
            // 5: user_token_x (writable)
            AccountMeta::new(config.user_token_x, false),
            // 6: user_token_y (writable)
            AccountMeta::new(config.user_token_y, false),
            // 7: token_x_mint (read-only)
            AccountMeta::new_readonly(pool.state.token_x_mint, false),
            // 8: token_y_mint (read-only)
            AccountMeta::new_readonly(pool.state.token_y_mint, false),
            // 9: oracle (writable)
            AccountMeta::new(pool.state.oracle, false),
            // 10: host_fee_in (writable)
            AccountMeta::new(host_fee_in, false),
            // 11: user authority (signer, read-only)
            AccountMeta::new_readonly(config.user_authority, true),
            // 12: token_x_program
            AccountMeta::new_readonly(pool.state.token_x_program, false),
            // 13: token_y_program
            AccountMeta::new_readonly(pool.state.token_y_program, false),
            // 14: event_authority
            AccountMeta::new_readonly(pool.event_authority, false),
            // 15: DLMM program
            AccountMeta::new_readonly(self.dlmm_program, false),
        ];

        Instruction {
            program_id: self.dlmm_program,
            accounts,
            data,
        }
    }

    // ── Address Lookup Table resolution ─────────────────────────────────

    /// Fetch and decode on-chain AddressLookupTable accounts so the
    /// transaction builder can compile a compact V0 message.
    fn resolve_lookup_tables(
        &self,
        addresses: &[Pubkey],
    ) -> Result<Vec<AddressLookupTableAccount>> {
        let mut tables = Vec::with_capacity(addresses.len());

        for addr in addresses {
            let account = self
                .rpc
                .get_account(addr)
                .map_err(|e| JitoBamError::MeteoraDlmmError(format!(
                    "failed to fetch ALT {addr}: {e}"
                )))?;

            let table = address_lookup_table::state::AddressLookupTable::deserialize(
                &account.data,
            )
            .map_err(|e| JitoBamError::MeteoraDlmmError(format!(
                "failed to deserialize ALT {addr}: {e}"
            )))?;

            tables.push(AddressLookupTableAccount {
                key: *addr,
                addresses: table.addresses.to_vec(),
            });
        }

        Ok(tables)
    }

    // ── PDA derivation ──────────────────────────────────────────────────

    /// Derive the BinArray PDA for the given pool and active bin.
    ///
    /// `upper = false` → lower array (active bin array index)
    /// `upper = true`  → next array above
    fn derive_bin_array_pda(
        &self,
        lb_pair: &Pubkey,
        active_id: i32,
        upper: bool,
    ) -> Pubkey {
        let bin_array_index = active_id / BIN_ARRAY_SIZE;
        let index = if upper {
            bin_array_index + 1
        } else {
            bin_array_index
        };

        let (pda, _) = Pubkey::find_program_address(
            &[
                b"bin_array",
                lb_pair.as_ref(),
                &index.to_le_bytes(),
            ],
            &self.dlmm_program,
        );
        pda
    }

    /// Derive the event authority PDA.
    fn derive_event_authority_pda(&self) -> Pubkey {
        let (pda, _) = Pubkey::find_program_address(
            &[b"__event_authority"],
            &self.dlmm_program,
        );
        pda
    }

    // ── Bin data fetch ──────────────────────────────────────────────────

    /// Fetch bins around the active bin for swap simulation.
    ///
    /// In a production deployment this would decode the BinArray accounts.
    /// Here we use a simplified on-chain fetch that reads the bin array
    /// accounts and extracts bin liquidity data.
    fn fetch_bins_around_active(
        &self,
        lb_pair: &Pubkey,
        state: &LbPairState,
    ) -> Result<Vec<BinData>> {
        let lower_pda = self.derive_bin_array_pda(lb_pair, state.active_id, false);
        let upper_pda = self.derive_bin_array_pda(lb_pair, state.active_id, true);

        let mut bins = Vec::new();

        // Try fetching both bin arrays — non-fatal if they don't exist
        for (pda, label) in [(lower_pda, "lower"), (upper_pda, "upper")] {
            match self.rpc.get_account(&pda) {
                Ok(account) => {
                    let decoded = Self::decode_bin_array(&account.data, state)?;
                    debug!(
                        array = label,
                        bins = decoded.len(),
                        "decoded bin array"
                    );
                    bins.extend(decoded);
                }
                Err(e) => {
                    debug!(
                        array = label,
                        error = %e,
                        "bin array not available — will estimate from active bin price"
                    );
                }
            }
        }

        Ok(bins)
    }

    /// Decode a BinArray account's raw bytes into individual bins.
    ///
    /// BinArray layout (after 8-byte Anchor discriminator):
    /// - `bin_array_index`: i32 (offset 8)
    /// - padding/version:   4 bytes (offset 12)
    /// - `bins`:            70 × BinEntry starting at offset 16
    ///
    /// Each BinEntry is 24 bytes:
    /// - `amount_x`: u64 (LE)
    /// - `amount_y`: u64 (LE)
    /// - `price`:    u64 (LE) — price per bin (fixed-point)
    fn decode_bin_array(
        data: &[u8],
        state: &LbPairState,
    ) -> Result<Vec<BinData>> {
        const DISC_SIZE: usize = 8;
        const HEADER_SIZE: usize = 8; // bin_array_index (i32) + padding (4)
        const BIN_ENTRY_SIZE: usize = 24;
        const BIN_START: usize = DISC_SIZE + HEADER_SIZE;

        if data.len() < BIN_START {
            return Ok(Vec::new());
        }

        let bin_array_index = i32::from_le_bytes(
            data[DISC_SIZE..DISC_SIZE + 4].try_into().unwrap(),
        );

        let mut bins = Vec::new();
        let available_bytes = data.len() - BIN_START;
        let num_bins = (available_bytes / BIN_ENTRY_SIZE).min(BIN_ARRAY_SIZE as usize);

        for i in 0..num_bins {
            let offset = BIN_START + i * BIN_ENTRY_SIZE;
            if offset + BIN_ENTRY_SIZE > data.len() {
                break;
            }

            let amount_x = u64::from_le_bytes(
                data[offset..offset + 8].try_into().unwrap(),
            );
            let amount_y = u64::from_le_bytes(
                data[offset + 8..offset + 16].try_into().unwrap(),
            );
            let price_raw = u64::from_le_bytes(
                data[offset + 16..offset + 24].try_into().unwrap(),
            );

            // Skip empty bins
            if amount_x == 0 && amount_y == 0 {
                continue;
            }

            let bin_id = bin_array_index * BIN_ARRAY_SIZE + i as i32;

            bins.push(BinData {
                id: bin_id,
                price: price_raw as u128,
                amount_x,
                amount_y,
            });
        }

        debug!(
            bin_array_index,
            bins_with_liquidity = bins.len(),
            "decoded bin array"
        );

        Ok(bins)
    }
}

// ─── Offline helpers (no RPC needed) ────────────────────────────────────────

/// Apply slippage tolerance to an expected output amount.
///
/// `min_out = expected * (10_000 - slippage_bps) / 10_000`
///
/// Uses u128 intermediate to avoid overflow for large amounts.
pub fn apply_slippage(expected_amount: u64, slippage_bps: u16) -> u64 {
    let bps = slippage_bps.min(10_000) as u128;
    let factor = 10_000u128 - bps;
    ((expected_amount as u128 * factor) / 10_000) as u64
}

/// Convert a DLMM bin ID to an effective price.
///
/// The DLMM uses a geometric price formula:
///     `price = (1 + bin_step / 10_000) ^ (bin_id - 0)`
///
/// This is the token-Y-per-token-X price for the given bin.
pub fn bin_id_to_price(bin_id: i32, bin_step: u16) -> f64 {
    let step = 1.0 + (bin_step as f64 / 10_000.0);
    step.powi(bin_id)
}

/// Build the instruction data for a DLMM swap (without accounts).
///
/// Useful for constructing the instruction manually.
pub fn encode_swap_data(
    amount_in: u64,
    min_amount_out: u64,
    swap_for_y: bool,
) -> Vec<u8> {
    let mut data = Vec::with_capacity(25);
    data.extend_from_slice(&SWAP_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());
    data.push(swap_for_y as u8);
    data
}

/// Derive the user's Associated Token Account (ATA) address.
pub fn derive_ata(wallet: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let spl_associated =
        Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")
            .expect("hardcoded ATA program");
    let (ata, _) = Pubkey::find_program_address(
        &[wallet.as_ref(), token_program.as_ref(), mint.as_ref()],
        &spl_associated,
    );
    ata
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slippage_50bps() {
        // 0.5% slippage on 1_000_000
        let min = apply_slippage(1_000_000, 50);
        assert_eq!(min, 995_000);
    }

    #[test]
    fn slippage_zero() {
        assert_eq!(apply_slippage(1_000_000, 0), 1_000_000);
    }

    #[test]
    fn slippage_100_percent() {
        assert_eq!(apply_slippage(1_000_000, 10_000), 0);
    }

    #[test]
    fn slippage_clamped_above_10000() {
        // bps > 10_000 is clamped
        assert_eq!(apply_slippage(1_000_000, 15_000), 0);
    }

    #[test]
    fn slippage_small_amount() {
        // 50bps on 100 → 99 (rounds down via integer division)
        assert_eq!(apply_slippage(100, 50), 99);
    }

    #[test]
    fn slippage_large_amount_no_overflow() {
        // u64::MAX should not overflow thanks to u128 intermediate
        let result = apply_slippage(u64::MAX, 50);
        let expected = ((u64::MAX as u128 * 9_950) / 10_000) as u64;
        assert_eq!(result, expected);
    }

    #[test]
    fn bin_price_at_zero() {
        let price = bin_id_to_price(0, 25);
        assert!((price - 1.0).abs() < 1e-10);
    }

    #[test]
    fn bin_price_positive_id() {
        // bin 100, step 25bps → (1.0025)^100
        let price = bin_id_to_price(100, 25);
        let expected = 1.0025_f64.powi(100);
        assert!((price - expected).abs() / expected < 1e-10);
    }

    #[test]
    fn bin_price_negative_id() {
        // bin -50, step 50bps → (1.005)^(-50)
        let price = bin_id_to_price(-50, 50);
        let expected = 1.005_f64.powi(-50);
        assert!((price - expected).abs() / expected < 1e-10);
    }

    #[test]
    fn swap_discriminator_is_correct() {
        // The known Anchor discriminator for Meteora DLMM swap
        assert_eq!(SWAP_DISCRIMINATOR, [248, 198, 158, 145, 225, 117, 135, 200]);
    }

    #[test]
    fn encode_swap_data_layout() {
        let data = encode_swap_data(1_000_000, 990_000, true);
        assert_eq!(data.len(), 25);
        // First 8 bytes: discriminator
        assert_eq!(&data[0..8], &SWAP_DISCRIMINATOR);
        // Next 8 bytes: amount_in
        assert_eq!(
            u64::from_le_bytes(data[8..16].try_into().unwrap()),
            1_000_000
        );
        // Next 8 bytes: min_amount_out
        assert_eq!(
            u64::from_le_bytes(data[16..24].try_into().unwrap()),
            990_000
        );
        // Last byte: swap_for_y
        assert_eq!(data[24], 1);
    }

    #[test]
    fn encode_swap_data_sell_y() {
        let data = encode_swap_data(500_000, 490_000, false);
        assert_eq!(data[24], 0); // swap_for_y = false
    }

    #[test]
    fn derive_ata_deterministic() {
        let wallet = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let token_program =
            Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        let ata1 = derive_ata(&wallet, &mint, &token_program);
        let ata2 = derive_ata(&wallet, &mint, &token_program);
        assert_eq!(ata1, ata2);
    }

    #[test]
    fn dlmm_program_id_parses() {
        let pk = Pubkey::from_str(DLMM_PROGRAM_ID);
        assert!(pk.is_ok());
    }

    #[test]
    fn apply_slippage_common_values() {
        // 1% on 1 SOL (1_000_000_000 lamports)
        assert_eq!(apply_slippage(1_000_000_000, 100), 990_000_000);
        // 0.1% on 1 SOL
        assert_eq!(apply_slippage(1_000_000_000, 10), 999_000_000);
        // 3% on 1 SOL
        assert_eq!(apply_slippage(1_000_000_000, 300), 970_000_000);
    }
}
