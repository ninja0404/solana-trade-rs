//! PumpAMM (外盘) constant-product AMM trading module.
//!
//! After a pump.fun token "graduates" from the bonding curve, liquidity moves
//! to PumpAMM (`pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`).
//!
//! This module provides `quick_amm_buy` / `quick_amm_sell` that construct and
//! send swap transactions. All accounts are derived locally; the only RPC call
//! inside `quick_*` is `sendTransaction`. Use `read_amm_swap_context` to
//! pre-fetch all needed pool data in one shot before calling `build_*`.

use crate::pump::derive_ata;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
#[allow(deprecated)]
use solana_sdk::system_program;
use solana_sdk::{
    commitment_config::CommitmentLevel,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Signature,
    signer::{keypair::Keypair, Signer},
    transaction::Transaction,
};
use std::str::FromStr;
use std::sync::LazyLock;

// =====================================================================
// Program IDs
// =====================================================================

pub static AMM_PROGRAM_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA").unwrap());

pub static AMM_FEE_PROGRAM_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ").unwrap());

pub static WSOL_MINT: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap());

static ATA_PROGRAM_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap());

static COMPUTE_BUDGET_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("ComputeBudget111111111111111111111111111111").unwrap());

static TOKEN_PROGRAM_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap());

pub static TOKEN_2022_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap());

// =====================================================================
// Pre-derived PDAs
// =====================================================================

pub static AMM_GLOBAL_CONFIG: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::find_program_address(&[b"global_config"], &AMM_PROGRAM_ID).0);

pub static AMM_GLOBAL_VOLUME_ACCUM: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(&[b"global_volume_accumulator"], &AMM_PROGRAM_ID).0
});

pub static AMM_EVENT_AUTHORITY: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::find_program_address(&[b"__event_authority"], &AMM_PROGRAM_ID).0);

pub static AMM_FEE_CONFIG: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(
        &[b"fee_config", AMM_PROGRAM_ID.as_ref()],
        &AMM_FEE_PROGRAM_ID,
    )
    .0
});

// =====================================================================
// Instruction Discriminators
// =====================================================================

const AMM_BUY_DISC: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
const AMM_BUY_EXACT_QUOTE_IN_DISC: [u8; 8] = [198, 46, 21, 82, 180, 217, 232, 112];
const AMM_SELL_DISC: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

// =====================================================================
// Error
// =====================================================================

#[derive(Debug, thiserror::Error)]
pub enum AmmError {
    #[error("RPC error: {0}")]
    Rpc(#[from] solana_client::client_error::ClientError),

    #[error("invalid data: {0}")]
    InvalidData(String),
}

// =====================================================================
// PDA helpers
// =====================================================================

pub fn amm_user_volume_accumulator(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &AMM_PROGRAM_ID).0
}

/// Derive pool-v2 PDA (required trailing account after PumpSwap upgrade).
pub fn amm_pool_v2(base_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"pool-v2", base_mint.as_ref()], &AMM_PROGRAM_ID).0
}

pub fn amm_coin_creator_vault_authority(coin_creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"creator_vault", coin_creator.as_ref()],
        &AMM_PROGRAM_ID,
    )
    .0
}

// =====================================================================
// SwapContext — everything needed for a swap, pre-fetched once
// =====================================================================

/// All data required for a PumpAMM swap. Pre-fetch with [`read_amm_swap_context`].
pub struct AmmSwapContext {
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_token_program: Pubkey,
    pub quote_token_program: Pubkey,
    pub pool_base_token_account: Pubkey,
    pub pool_quote_token_account: Pubkey,
    pub coin_creator: Pubkey,
    pub protocol_fee_recipient: Pubkey,
    pub base_reserve: u64,
    pub quote_reserve: u64,
    pub is_reversed: bool,
}

/// Pool account data layout offsets (after 8-byte Anchor discriminator).
const POOL_BASE_MINT_OFFSET: usize = 43; // disc(8)+bump(1)+index(2)+creator(32)
const POOL_QUOTE_MINT_OFFSET: usize = 75;
const POOL_BASE_TOKEN_ACCT_OFFSET: usize = 139;
const POOL_QUOTE_TOKEN_ACCT_OFFSET: usize = 171;
const POOL_COIN_CREATOR_OFFSET: usize = 211; // after lp_supply(8)

fn pubkey_at(data: &[u8], offset: usize) -> Pubkey {
    Pubkey::new_from_array(data[offset..offset + 32].try_into().unwrap())
}

fn token_account_balance(data: &[u8]) -> u64 {
    if data.len() < 72 {
        return 0;
    }
    u64::from_le_bytes(data[64..72].try_into().unwrap())
}

/// One-shot RPC helper: reads pool account, mints, token vaults, and global
/// config to build an [`AmmSwapContext`]. Call once and reuse for multiple swaps.
pub fn read_amm_swap_context(
    rpc: &RpcClient,
    pool_address: &Pubkey,
) -> Result<AmmSwapContext, AmmError> {
    let pool_acct = rpc.get_account(pool_address)?;
    let d = &pool_acct.data;
    if d.len() < 250 {
        return Err(AmmError::InvalidData("pool account too short".into()));
    }

    let base_mint = pubkey_at(d, POOL_BASE_MINT_OFFSET);
    let quote_mint = pubkey_at(d, POOL_QUOTE_MINT_OFFSET);
    let pool_base_token_account = pubkey_at(d, POOL_BASE_TOKEN_ACCT_OFFSET);
    let pool_quote_token_account = pubkey_at(d, POOL_QUOTE_TOKEN_ACCT_OFFSET);
    let coin_creator = pubkey_at(d, POOL_COIN_CREATOR_OFFSET);

    let base_mint_acct = rpc.get_account(&base_mint)?;
    let quote_mint_acct = rpc.get_account(&quote_mint)?;
    let base_token_program = base_mint_acct.owner;
    let quote_token_program = quote_mint_acct.owner;

    let base_vault = rpc.get_account(&pool_base_token_account)?;
    let quote_vault = rpc.get_account(&pool_quote_token_account)?;
    let base_reserve = token_account_balance(&base_vault.data);
    let quote_reserve = token_account_balance(&quote_vault.data);

    let global_cfg = rpc.get_account(&AMM_GLOBAL_CONFIG)?;
    // GlobalConfigAccount layout (Borsh, after 8-byte discriminator):
    //   Admin(32) + LpFeeBasisPoints(8) + ProtocolFeeBasisPoints(8) + DisableFlags(1)
    //   = 8+32+8+8+1 = 57  →  ProtocolFeeRecipients[8] starts at offset 57
    // Pick a random non-zero recipient from the array (matching SDK behavior)
    let protocol_fee_recipient = {
        let mut candidates = Vec::new();
        for i in 0..8 {
            let off = 57 + i * 32;
            if off + 32 <= global_cfg.data.len() {
                let pk = pubkey_at(&global_cfg.data, off);
                if pk != Pubkey::default() {
                    candidates.push(pk);
                }
            }
        }
        if candidates.is_empty() {
            Pubkey::default()
        } else {
            use std::time::SystemTime;
            let idx = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as usize
                % candidates.len();
            candidates[idx]
        }
    };

    let is_reversed = base_mint == *WSOL_MINT;

    Ok(AmmSwapContext {
        pool: *pool_address,
        base_mint,
        quote_mint,
        base_token_program,
        quote_token_program,
        pool_base_token_account,
        pool_quote_token_account,
        coin_creator,
        protocol_fee_recipient,
        base_reserve,
        quote_reserve,
        is_reversed,
    })
}

// =====================================================================
// SPL helper instructions (WSOL wrap / unwrap)
// =====================================================================

fn system_transfer_ix(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = vec![2u8, 0, 0, 0]; // transfer discriminator (u32 LE)
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction::new_with_bytes(
        system_program::id(),
        &data,
        vec![
            AccountMeta::new(*from, true),
            AccountMeta::new(*to, false),
        ],
    )
}

fn sync_native_ix(account: &Pubkey, token_program: &Pubkey) -> Instruction {
    Instruction::new_with_bytes(
        *token_program,
        &[17], // SyncNative discriminator
        vec![AccountMeta::new(*account, false)],
    )
}

fn close_account_ix(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction::new_with_bytes(
        *token_program,
        &[9], // CloseAccount discriminator
        vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
    )
}

fn create_ata_idempotent_ix(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let ata = derive_ata(owner, mint, token_program);
    Instruction::new_with_bytes(
        *ATA_PROGRAM_ID,
        &[1],
        vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
    )
}

fn compute_unit_limit_ix(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(2u8);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(*COMPUTE_BUDGET_ID, &data, vec![])
}

fn compute_unit_price_ix(micro_lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3u8);
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction::new_with_bytes(*COMPUTE_BUDGET_ID, &data, vec![])
}

// =====================================================================
// PumpAMM swap instructions
// =====================================================================

/// Buy — buy `base_amount_out` tokens, paying at most `max_quote_in` SOL.
/// Uses the `Buy` instruction (not `BuyExactQuoteIn`), matching all major SDKs.
/// 23 accounts (Anchor resolves coinCreatorVault*, volume accumulators, feeConfig, feeProgram from IDL).
fn amm_buy_ix(
    base_amount_out: u64,
    max_quote_in: u64,
    ctx: &AmmSwapContext,
    user: &Pubkey,
) -> Instruction {
    let user_base_ata = derive_ata(user, &ctx.base_mint, &ctx.base_token_program);
    let user_quote_ata = derive_ata(user, &ctx.quote_mint, &ctx.quote_token_program);
    let protocol_fee_ata = derive_ata(
        &ctx.protocol_fee_recipient,
        &ctx.quote_mint,
        &ctx.quote_token_program,
    );
    let creator_vault_auth = amm_coin_creator_vault_authority(&ctx.coin_creator);
    let creator_vault_ata = derive_ata(
        &creator_vault_auth,
        &ctx.quote_mint,
        &ctx.quote_token_program,
    );
    let user_vol = amm_user_volume_accumulator(user);

    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&AMM_BUY_DISC);
    data.extend_from_slice(&base_amount_out.to_le_bytes());
    data.extend_from_slice(&max_quote_in.to_le_bytes());

    Instruction::new_with_bytes(
        *AMM_PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new(ctx.pool, false),                                  //  0
            AccountMeta::new(*user, true),                                      //  1
            AccountMeta::new_readonly(*AMM_GLOBAL_CONFIG, false),               //  2
            AccountMeta::new_readonly(ctx.base_mint, false),                    //  3
            AccountMeta::new_readonly(ctx.quote_mint, false),                   //  4
            AccountMeta::new(user_base_ata, false),                            //  5
            AccountMeta::new(user_quote_ata, false),                           //  6
            AccountMeta::new(ctx.pool_base_token_account, false),              //  7
            AccountMeta::new(ctx.pool_quote_token_account, false),             //  8
            AccountMeta::new_readonly(ctx.protocol_fee_recipient, false),       //  9
            AccountMeta::new(protocol_fee_ata, false),                         // 10
            AccountMeta::new_readonly(ctx.base_token_program, false),          // 11
            AccountMeta::new_readonly(ctx.quote_token_program, false),         // 12
            AccountMeta::new_readonly(system_program::id(), false),            // 13
            AccountMeta::new_readonly(*ATA_PROGRAM_ID, false),                 // 14
            AccountMeta::new_readonly(*AMM_EVENT_AUTHORITY, false),            // 15
            AccountMeta::new_readonly(*AMM_PROGRAM_ID, false),                 // 16
            AccountMeta::new(creator_vault_ata, false),                        // 17
            AccountMeta::new_readonly(creator_vault_auth, false),              // 18
            AccountMeta::new_readonly(*AMM_GLOBAL_VOLUME_ACCUM, false),        // 19
            AccountMeta::new(user_vol, false),                                 // 20
            AccountMeta::new_readonly(*AMM_FEE_CONFIG, false),                 // 21
            AccountMeta::new_readonly(*AMM_FEE_PROGRAM_ID, false),             // 22
            AccountMeta::new_readonly(amm_pool_v2(&ctx.base_mint), false),     // 23 pool-v2
        ],
    )
}

/// BuyExactQuoteIn — spend exact SOL (quote), receive >= min tokens (base).
/// Same 23+1 account layout as Buy, different discriminator and params.
fn amm_buy_exact_quote_in_ix(
    spendable_quote_in: u64,
    min_base_amount_out: u64,
    ctx: &AmmSwapContext,
    user: &Pubkey,
) -> Instruction {
    let user_base_ata = derive_ata(user, &ctx.base_mint, &ctx.base_token_program);
    let user_quote_ata = derive_ata(user, &ctx.quote_mint, &ctx.quote_token_program);
    let protocol_fee_ata = derive_ata(
        &ctx.protocol_fee_recipient,
        &ctx.quote_mint,
        &ctx.quote_token_program,
    );
    let creator_vault_auth = amm_coin_creator_vault_authority(&ctx.coin_creator);
    let creator_vault_ata = derive_ata(
        &creator_vault_auth,
        &ctx.quote_mint,
        &ctx.quote_token_program,
    );
    let user_vol = amm_user_volume_accumulator(user);
    let user_vol_wsol_ata = derive_ata(&user_vol, &ctx.quote_mint, &ctx.quote_token_program);

    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&AMM_BUY_EXACT_QUOTE_IN_DISC);
    data.extend_from_slice(&spendable_quote_in.to_le_bytes());
    data.extend_from_slice(&min_base_amount_out.to_le_bytes());
    // NO trackVolume byte — omit it, matching successful on-chain transactions

    Instruction::new_with_bytes(
        *AMM_PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new(ctx.pool, false),                                  //  0
            AccountMeta::new(*user, true),                                      //  1
            AccountMeta::new_readonly(*AMM_GLOBAL_CONFIG, false),               //  2
            AccountMeta::new_readonly(ctx.base_mint, false),                    //  3
            AccountMeta::new_readonly(ctx.quote_mint, false),                   //  4
            AccountMeta::new(user_base_ata, false),                            //  5
            AccountMeta::new(user_quote_ata, false),                           //  6
            AccountMeta::new(ctx.pool_base_token_account, false),              //  7
            AccountMeta::new(ctx.pool_quote_token_account, false),             //  8
            AccountMeta::new_readonly(ctx.protocol_fee_recipient, false),       //  9
            AccountMeta::new(protocol_fee_ata, false),                         // 10
            AccountMeta::new_readonly(ctx.base_token_program, false),          // 11
            AccountMeta::new_readonly(ctx.quote_token_program, false),         // 12
            AccountMeta::new_readonly(system_program::id(), false),            // 13
            AccountMeta::new_readonly(*ATA_PROGRAM_ID, false),                 // 14
            AccountMeta::new_readonly(*AMM_EVENT_AUTHORITY, false),            // 15
            AccountMeta::new_readonly(*AMM_PROGRAM_ID, false),                 // 16
            AccountMeta::new(creator_vault_ata, false),                        // 17
            AccountMeta::new_readonly(creator_vault_auth, false),              // 18
            AccountMeta::new_readonly(*AMM_GLOBAL_VOLUME_ACCUM, false),        // 19
            AccountMeta::new(user_vol, false),                                 // 20
            AccountMeta::new_readonly(*AMM_FEE_CONFIG, false),                 // 21
            AccountMeta::new_readonly(*AMM_FEE_PROGRAM_ID, false),             // 22
            AccountMeta::new(user_vol_wsol_ata, false),                        // 23 cashback
            AccountMeta::new_readonly(amm_pool_v2(&ctx.base_mint), false),     // 24 pool-v2
        ],
    )
}

/// Sell — sell exact tokens (base), receive >= min SOL (quote).
/// 21 accounts.
fn amm_sell_ix(
    base_amount_in: u64,
    min_quote_amount_out: u64,
    ctx: &AmmSwapContext,
    user: &Pubkey,
) -> Instruction {
    let user_base_ata = derive_ata(user, &ctx.base_mint, &ctx.base_token_program);
    let user_quote_ata = derive_ata(user, &ctx.quote_mint, &ctx.quote_token_program);
    let protocol_fee_ata = derive_ata(
        &ctx.protocol_fee_recipient,
        &ctx.quote_mint,
        &ctx.quote_token_program,
    );
    let creator_vault_auth = amm_coin_creator_vault_authority(&ctx.coin_creator);
    let creator_vault_ata = derive_ata(
        &creator_vault_auth,
        &ctx.quote_mint,
        &ctx.quote_token_program,
    );

    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&AMM_SELL_DISC);
    data.extend_from_slice(&base_amount_in.to_le_bytes());
    data.extend_from_slice(&min_quote_amount_out.to_le_bytes());

    Instruction::new_with_bytes(
        *AMM_PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new(ctx.pool, false),                                  //  0
            AccountMeta::new(*user, true),                                      //  1
            AccountMeta::new_readonly(*AMM_GLOBAL_CONFIG, false),               //  2
            AccountMeta::new_readonly(ctx.base_mint, false),                    //  3
            AccountMeta::new_readonly(ctx.quote_mint, false),                   //  4
            AccountMeta::new(user_base_ata, false),                            //  5
            AccountMeta::new(user_quote_ata, false),                           //  6
            AccountMeta::new(ctx.pool_base_token_account, false),              //  7
            AccountMeta::new(ctx.pool_quote_token_account, false),             //  8
            AccountMeta::new_readonly(ctx.protocol_fee_recipient, false),       //  9
            AccountMeta::new(protocol_fee_ata, false),                         // 10
            AccountMeta::new_readonly(ctx.base_token_program, false),          // 11
            AccountMeta::new_readonly(ctx.quote_token_program, false),         // 12
            AccountMeta::new_readonly(system_program::id(), false),            // 13
            AccountMeta::new_readonly(*ATA_PROGRAM_ID, false),                 // 14
            AccountMeta::new_readonly(*AMM_EVENT_AUTHORITY, false),            // 15
            AccountMeta::new_readonly(*AMM_PROGRAM_ID, false),                 // 16
            AccountMeta::new(creator_vault_ata, false),                        // 17
            AccountMeta::new_readonly(creator_vault_auth, false),              // 18
            AccountMeta::new_readonly(*AMM_FEE_CONFIG, false),                 // 19
            AccountMeta::new_readonly(*AMM_FEE_PROGRAM_ID, false),             // 20
            AccountMeta::new_readonly(amm_pool_v2(&ctx.base_mint), false),     // 21 pool-v2
        ],
    )
}

// =====================================================================
// Trade Parameters
// =====================================================================

pub struct AmmBuyParams {
    pub sol_amount_lamports: u64,
    /// Slippage in basis points (e.g. 500 = 5%). Applied to max SOL cost.
    pub slippage_bps: u64,
    pub recent_blockhash: Hash,
    pub compute_unit_limit: Option<u32>,
    pub compute_unit_price_micro_lamports: Option<u64>,
}

pub struct AmmSellParams {
    pub token_amount: u64,
    pub min_sol_out: u64,
    pub recent_blockhash: Hash,
    pub compute_unit_limit: Option<u32>,
    pub compute_unit_price_micro_lamports: Option<u64>,
}

// =====================================================================
// Transaction builders (pure, no RPC)
// =====================================================================

/// Build a buy transaction for a **normal** pool (BaseMint=Token, QuoteMint=WSOL).
/// PumpAMM handles SOL→WSOL wrapping internally — no manual wrap/sync needed.
fn build_normal_buy(
    signer: &Keypair,
    ctx: &AmmSwapContext,
    params: &AmmBuyParams,
) -> Transaction {
    let user = signer.pubkey();
    let cu = params.compute_unit_limit.unwrap_or(300_000);

    let mut ixs = Vec::with_capacity(5);
    ixs.push(compute_unit_limit_ix(cu));
    if let Some(p) = params.compute_unit_price_micro_lamports {
        ixs.push(compute_unit_price_ix(p));
    }
    let user_quote_ata = derive_ata(&user, &ctx.quote_mint, &ctx.quote_token_program);
    ixs.push(create_ata_idempotent_ix(&user, &user, &ctx.base_mint, &ctx.base_token_program));
    ixs.push(create_ata_idempotent_ix(&user, &user, &ctx.quote_mint, &ctx.quote_token_program));
    ixs.push(system_transfer_ix(&user, &user_quote_ata, params.sol_amount_lamports));
    ixs.push(sync_native_ix(&user_quote_ata, &ctx.quote_token_program));
    ixs.push(amm_buy_exact_quote_in_ix(params.sol_amount_lamports, 1, ctx, &user));
    ixs.push(close_account_ix(&user_quote_ata, &user, &user, &ctx.quote_token_program));

    Transaction::new_signed_with_payer(&ixs, Some(&user), &[signer], params.recent_blockhash)
}

/// Build a buy transaction for a **reversed** pool (on-chain BaseMint=WSOL).
/// Internally uses Sell instruction (sell WSOL → get Token).
fn build_reversed_buy(
    signer: &Keypair,
    ctx: &AmmSwapContext,
    params: &AmmBuyParams,
) -> Transaction {
    let user = signer.pubkey();
    // On-chain: base=WSOL, quote=Token → swap back for Sell instruction
    let on_chain_ctx = AmmSwapContext {
        pool: ctx.pool,
        base_mint: ctx.quote_mint,   // WSOL
        quote_mint: ctx.base_mint,   // Token
        base_token_program: ctx.quote_token_program,
        quote_token_program: ctx.base_token_program,
        pool_base_token_account: ctx.pool_quote_token_account,
        pool_quote_token_account: ctx.pool_base_token_account,
        coin_creator: ctx.coin_creator,
        protocol_fee_recipient: ctx.protocol_fee_recipient,
        base_reserve: ctx.quote_reserve,
        quote_reserve: ctx.base_reserve,
        is_reversed: false,
    };

    let user_wsol_ata = derive_ata(&user, &on_chain_ctx.base_mint, &on_chain_ctx.base_token_program);
    let cu = params.compute_unit_limit.unwrap_or(300_000);

    let mut ixs = Vec::with_capacity(8);
    ixs.push(compute_unit_limit_ix(cu));
    if let Some(p) = params.compute_unit_price_micro_lamports {
        ixs.push(compute_unit_price_ix(p));
    }
    ixs.push(create_ata_idempotent_ix(&user, &user, &on_chain_ctx.quote_mint, &on_chain_ctx.quote_token_program));
    ixs.push(create_ata_idempotent_ix(&user, &user, &on_chain_ctx.base_mint, &on_chain_ctx.base_token_program));
    let max_quote_in = params.sol_amount_lamports
        .checked_mul(10_000 + params.slippage_bps)
        .unwrap_or(params.sol_amount_lamports) / 10_000;
    ixs.push(system_transfer_ix(&user, &user_wsol_ata, max_quote_in));
    ixs.push(sync_native_ix(&user_wsol_ata, &on_chain_ctx.base_token_program));
    // Sell WSOL to get Token (min_tokens_out = 1 for simplicity in reversed)
    ixs.push(amm_sell_ix(
        params.sol_amount_lamports,
        1,
        &on_chain_ctx,
        &user,
    ));
    ixs.push(close_account_ix(&user_wsol_ata, &user, &user, &on_chain_ctx.base_token_program));

    Transaction::new_signed_with_payer(&ixs, Some(&user), &[signer], params.recent_blockhash)
}

/// Build a sell transaction for a **normal** pool (BaseMint=Token, QuoteMint=WSOL).
fn build_normal_sell(
    signer: &Keypair,
    ctx: &AmmSwapContext,
    params: &AmmSellParams,
) -> Transaction {
    let user = signer.pubkey();
    let user_quote_ata = derive_ata(&user, &ctx.quote_mint, &ctx.quote_token_program);
    let cu = params.compute_unit_limit.unwrap_or(300_000);

    let mut ixs = Vec::with_capacity(5);
    ixs.push(compute_unit_limit_ix(cu));
    if let Some(p) = params.compute_unit_price_micro_lamports {
        ixs.push(compute_unit_price_ix(p));
    }
    ixs.push(create_ata_idempotent_ix(&user, &user, &ctx.quote_mint, &ctx.quote_token_program));
    ixs.push(amm_sell_ix(params.token_amount, params.min_sol_out, ctx, &user));
    ixs.push(close_account_ix(&user_quote_ata, &user, &user, &ctx.quote_token_program));

    Transaction::new_signed_with_payer(&ixs, Some(&user), &[signer], params.recent_blockhash)
}

/// Build a sell transaction for a **reversed** pool (on-chain BaseMint=WSOL).
/// Internally uses BuyExactQuoteIn (spend Token to buy WSOL).
fn build_reversed_sell(
    signer: &Keypair,
    ctx: &AmmSwapContext,
    params: &AmmSellParams,
) -> Transaction {
    let user = signer.pubkey();
    let on_chain_ctx = AmmSwapContext {
        pool: ctx.pool,
        base_mint: ctx.quote_mint,
        quote_mint: ctx.base_mint,
        base_token_program: ctx.quote_token_program,
        quote_token_program: ctx.base_token_program,
        pool_base_token_account: ctx.pool_quote_token_account,
        pool_quote_token_account: ctx.pool_base_token_account,
        coin_creator: ctx.coin_creator,
        protocol_fee_recipient: ctx.protocol_fee_recipient,
        base_reserve: ctx.quote_reserve,
        quote_reserve: ctx.base_reserve,
        is_reversed: false,
    };

    let user_wsol_ata = derive_ata(&user, &on_chain_ctx.base_mint, &on_chain_ctx.base_token_program);
    let cu = params.compute_unit_limit.unwrap_or(300_000);

    let mut ixs = Vec::with_capacity(5);
    ixs.push(compute_unit_limit_ix(cu));
    if let Some(p) = params.compute_unit_price_micro_lamports {
        ixs.push(compute_unit_price_ix(p));
    }
    ixs.push(create_ata_idempotent_ix(&user, &user, &on_chain_ctx.base_mint, &on_chain_ctx.base_token_program));
    // Buy WSOL (base) with Token (quote): calculate WSOL amount from token input
    let total_fee_bps = 100u128;
    let effective = (params.token_amount as u128) * 10_000 / (10_000 + total_fee_bps);
    let denom = (on_chain_ctx.quote_reserve as u128) + effective;
    let wsol_out = if denom > 0 {
        ((on_chain_ctx.base_reserve as u128) * effective / denom) as u64
    } else {
        params.min_sol_out
    };
    ixs.push(amm_buy_ix(wsol_out, params.token_amount, &on_chain_ctx, &user));
    ixs.push(close_account_ix(&user_wsol_ata, &user, &user, &on_chain_ctx.base_token_program));

    Transaction::new_signed_with_payer(&ixs, Some(&user), &[signer], params.recent_blockhash)
}

// =====================================================================
// Public API
// =====================================================================

/// Build a signed AMM buy transaction. Handles reversed pools automatically.
pub fn build_amm_buy_transaction(
    signer: &Keypair,
    ctx: &AmmSwapContext,
    params: &AmmBuyParams,
) -> Transaction {
    if ctx.is_reversed {
        build_reversed_buy(signer, ctx, params)
    } else {
        build_normal_buy(signer, ctx, params)
    }
}

/// Build a signed AMM sell transaction. Handles reversed pools automatically.
pub fn build_amm_sell_transaction(
    signer: &Keypair,
    ctx: &AmmSwapContext,
    params: &AmmSellParams,
) -> Transaction {
    if ctx.is_reversed {
        build_reversed_sell(signer, ctx, params)
    } else {
        build_normal_sell(signer, ctx, params)
    }
}

fn send_tx(rpc: &RpcClient, tx: &Transaction) -> Result<Signature, AmmError> {
    Ok(rpc.send_transaction_with_config(
        tx,
        RpcSendTransactionConfig {
            skip_preflight: true,
            preflight_commitment: Some(CommitmentLevel::Confirmed),
            ..Default::default()
        },
    )?)
}

/// Build, sign and send an AMM buy transaction.
pub fn quick_amm_buy(
    rpc: &RpcClient,
    signer: &Keypair,
    ctx: &AmmSwapContext,
    params: &AmmBuyParams,
) -> Result<Signature, AmmError> {
    let tx = build_amm_buy_transaction(signer, ctx, params);
    send_tx(rpc, &tx)
}

/// Build, sign and send an AMM sell transaction.
pub fn quick_amm_sell(
    rpc: &RpcClient,
    signer: &Keypair,
    ctx: &AmmSwapContext,
    params: &AmmSellParams,
) -> Result<Signature, AmmError> {
    let tx = build_amm_sell_transaction(signer, ctx, params);
    send_tx(rpc, &tx)
}

// =====================================================================
// Quote (x*y=k with fees)
// =====================================================================

/// Calculate expected token output for a given SOL input (buy).
pub fn amm_quote_buy(ctx: &AmmSwapContext, sol_amount_in: u64, total_fee_bps: u64) -> u64 {
    let fee_mult = 10_000u128.saturating_sub(total_fee_bps as u128);
    let amount_after_fee = (sol_amount_in as u128) * fee_mult / 10_000;
    let denom = (ctx.quote_reserve as u128) + amount_after_fee;
    if denom == 0 {
        return 0;
    }
    ((ctx.base_reserve as u128) * amount_after_fee / denom) as u64
}

/// Calculate expected SOL output for a given token input (sell).
pub fn amm_quote_sell(ctx: &AmmSwapContext, token_amount_in: u64, total_fee_bps: u64) -> u64 {
    let fee_mult = 10_000u128.saturating_sub(total_fee_bps as u128);
    let amount_after_fee = (token_amount_in as u128) * fee_mult / 10_000;
    let denom = (ctx.base_reserve as u128) + amount_after_fee;
    if denom == 0 {
        return 0;
    }
    ((ctx.quote_reserve as u128) * amount_after_fee / denom) as u64
}
