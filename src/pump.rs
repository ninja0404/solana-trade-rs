//! Pump.fun bonding-curve (内盘) trading module.
//!
//! `quick_buy` / `quick_sell` construct and send pump.fun BuyExactSolIn / Sell
//! transactions **without** additional RPC data-fetching calls. All needed
//! account addresses are derived locally from the passed-in parameters; the
//! only network call is the final `sendTransaction`.

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

pub static PUMP_PROGRAM_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P").unwrap());

pub static FEE_PROGRAM_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ").unwrap());

pub static FEE_RECIPIENT: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("7VtfL8fvgNfhz17qKRMjzQEXgbdpnHHHQRh54R9jP2RJ").unwrap());

static ATA_PROGRAM_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap());

static COMPUTE_BUDGET_PROGRAM_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("ComputeBudget111111111111111111111111111111").unwrap());

pub static TOKEN_PROGRAM_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap());

pub static TOKEN_2022_PROGRAM_ID: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap());

// =====================================================================
// Pre-derived PDAs (constant for the pump program)
// =====================================================================

pub static GLOBAL_PDA: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::find_program_address(&[b"global"], &PUMP_PROGRAM_ID).0);

pub static EVENT_AUTHORITY: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::find_program_address(&[b"__event_authority"], &PUMP_PROGRAM_ID).0);

pub static GLOBAL_VOLUME_ACCUMULATOR: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(&[b"global_volume_accumulator"], &PUMP_PROGRAM_ID).0
});

pub static FEE_CONFIG_PDA: LazyLock<Pubkey> = LazyLock::new(|| {
    Pubkey::find_program_address(&[b"fee_config", PUMP_PROGRAM_ID.as_ref()], &FEE_PROGRAM_ID).0
});

// =====================================================================
// Instruction Discriminators (Anchor / Borsh, 8 bytes)
// =====================================================================

const BUY_DISC: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
const BUY_EXACT_SOL_IN_DISC: [u8; 8] = [56, 252, 116, 8, 158, 223, 205, 95];
const SELL_DISC: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

// =====================================================================
// Error
// =====================================================================

#[derive(Debug, thiserror::Error)]
pub enum PumpError {
    #[error("RPC error: {0}")]
    Rpc(#[from] solana_client::client_error::ClientError),

    #[error("invalid parameter: {0}")]
    InvalidParam(String),
}

// =====================================================================
// PDA Derivation (all pure, no RPC)
// =====================================================================

pub fn derive_bonding_curve(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &PUMP_PROGRAM_ID).0
}

pub fn derive_ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

pub fn derive_user_volume_accumulator(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"user_volume_accumulator", user.as_ref()],
        &PUMP_PROGRAM_ID,
    )
    .0
}

/// Derive the creator-vault PDA from the bonding-curve's `creator` pubkey.
pub fn derive_creator_vault(creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"creator-vault", creator.as_ref()], &PUMP_PROGRAM_ID).0
}

/// Derive the bonding-curve-v2 PDA (Cashback upgrade, required since ~2025).
pub fn derive_bonding_curve_v2(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bonding-curve-v2", mint.as_ref()], &PUMP_PROGRAM_ID).0
}

// =====================================================================
// Global Account Reader
// =====================================================================

/// Check whether the bonding-curve-v2 PDA exists on-chain for a given mint.
/// Tokens created after the Cashback upgrade have it; older tokens do not.
pub fn check_bonding_curve_v2_exists(rpc_client: &RpcClient, mint: &Pubkey) -> bool {
    let bc_v2 = derive_bonding_curve_v2(mint);
    rpc_client.get_account(&bc_v2).is_ok()
}

/// Read the current fee_recipient from the on-chain Global account.
/// This should be called once at startup and cached.
pub fn read_fee_recipient(rpc_client: &RpcClient) -> Result<Pubkey, PumpError> {
    let account = rpc_client.get_account(&GLOBAL_PDA)?;
    let data = &account.data;
    // Global account layout (Anchor): discriminator(8) + initialized(1) + authority(32)
    // + fee_recipient(32) starts at offset 41
    if data.len() < 73 {
        return Err(PumpError::InvalidParam("Global account data too short".into()));
    }
    let bytes: [u8; 32] = data[41..73]
        .try_into()
        .map_err(|_| PumpError::InvalidParam("bad fee_recipient slice".into()))?;
    Ok(Pubkey::new_from_array(bytes))
}

// =====================================================================
// Keypair Helper
// =====================================================================

pub fn keypair_from_base58(base58_key: &str) -> Result<Keypair, PumpError> {
    let bytes = bs58::decode(base58_key)
        .into_vec()
        .map_err(|e| PumpError::InvalidParam(format!("invalid base58: {e}")))?;
    Keypair::try_from(bytes.as_slice())
        .map_err(|e| PumpError::InvalidParam(format!("invalid keypair bytes: {e}")))
}

// =====================================================================
// Instruction Builders (private)
// =====================================================================

fn compute_unit_limit_ix(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(2u8);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(*COMPUTE_BUDGET_PROGRAM_ID, &data, vec![])
}

fn compute_unit_price_ix(micro_lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3u8);
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction::new_with_bytes(*COMPUTE_BUDGET_PROGRAM_ID, &data, vec![])
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

/// Buy instruction – 17 accounts (same layout as BuyExactSolIn).
/// Uses token amount + max SOL cost instead of exact SOL input.
fn buy_ix(
    amount: u64,
    max_sol_cost: u64,
    user: &Pubkey,
    mint: &Pubkey,
    creator_vault: &Pubkey,
    token_program: &Pubkey,
    fee_recipient: &Pubkey,
) -> Instruction {
    let bonding_curve = derive_bonding_curve(mint);
    let assoc_bonding_curve = derive_ata(&bonding_curve, mint, token_program);
    let assoc_user = derive_ata(user, mint, token_program);
    let user_vol_accum = derive_user_volume_accumulator(user);

    let mut data = Vec::with_capacity(25);
    data.extend_from_slice(&BUY_DISC);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&max_sol_cost.to_le_bytes());
    data.push(1u8); // track_volume = true

    Instruction::new_with_bytes(
        *PUMP_PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new_readonly(*GLOBAL_PDA, false),
            AccountMeta::new(*fee_recipient, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(bonding_curve, false),
            AccountMeta::new(assoc_bonding_curve, false),
            AccountMeta::new(assoc_user, false),
            AccountMeta::new(*user, true),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new(*creator_vault, false),
            AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(*PUMP_PROGRAM_ID, false),
            AccountMeta::new_readonly(*GLOBAL_VOLUME_ACCUMULATOR, false),
            AccountMeta::new(user_vol_accum, false),
            AccountMeta::new_readonly(*FEE_CONFIG_PDA, false),
            AccountMeta::new_readonly(*FEE_PROGRAM_ID, false),
            AccountMeta::new(derive_bonding_curve_v2(mint), false),
        ],
    )
}

/// BuyExactSolIn instruction – 17 accounts.
///
///  0  global                    (R)
///  1  fee_recipient             (W)
///  2  mint                      (R)
///  3  bonding_curve             (W)
///  4  associated_bonding_curve  (W)
///  5  associated_user           (W)
///  6  user                      (W, Signer)
///  7  system_program            (R)
///  8  token_program             (R)
///  9  creator_vault             (W)
/// 10  event_authority           (R)
/// 11  program                   (R)
/// 12  global_volume_accumulator (R)
/// 13  user_volume_accumulator   (W)
/// 14  fee_config                (R)
/// 15  fee_program               (R)
/// 16  bonding_curve_v2          (R)  ← Cashback upgrade
fn buy_exact_sol_in_ix(
    spendable_sol_in: u64,
    min_tokens_out: u64,
    track_volume: bool,
    user: &Pubkey,
    mint: &Pubkey,
    creator_vault: &Pubkey,
    token_program: &Pubkey,
    fee_recipient: &Pubkey,
) -> Instruction {
    let bonding_curve = derive_bonding_curve(mint);
    let assoc_bonding_curve = derive_ata(&bonding_curve, mint, token_program);
    let assoc_user = derive_ata(user, mint, token_program);
    let user_vol_accum = derive_user_volume_accumulator(user);
    let bonding_curve_v2 = derive_bonding_curve_v2(mint);

    let mut data = Vec::with_capacity(25);
    data.extend_from_slice(&BUY_EXACT_SOL_IN_DISC);
    data.extend_from_slice(&spendable_sol_in.to_le_bytes());
    data.extend_from_slice(&min_tokens_out.to_le_bytes());
    data.push(track_volume as u8);

    Instruction::new_with_bytes(
        *PUMP_PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new_readonly(*GLOBAL_PDA, false),
            AccountMeta::new(*fee_recipient, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(bonding_curve, false),
            AccountMeta::new(assoc_bonding_curve, false),
            AccountMeta::new(assoc_user, false),
            AccountMeta::new(*user, true),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new(*creator_vault, false),
            AccountMeta::new_readonly(*EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(*PUMP_PROGRAM_ID, false),
            AccountMeta::new_readonly(*GLOBAL_VOLUME_ACCUMULATOR, false),
            AccountMeta::new(user_vol_accum, false),
            AccountMeta::new_readonly(*FEE_CONFIG_PDA, false),
            AccountMeta::new_readonly(*FEE_PROGRAM_ID, false),
            AccountMeta::new(bonding_curve_v2, false),
        ],
    )
}

/// Sell instruction – account count depends on cashback flag:
///   Non-cashback: 15 accounts (14 base + bonding_curve_v2)
///   Cashback:     16 accounts (14 base + user_volume_accumulator + bonding_curve_v2)
fn sell_ix(
    amount: u64,
    min_sol_output: u64,
    user: &Pubkey,
    mint: &Pubkey,
    creator_vault: &Pubkey,
    token_program: &Pubkey,
    fee_recipient: &Pubkey,
    is_cashback: bool,
) -> Instruction {
    let bonding_curve = derive_bonding_curve(mint);
    let assoc_bonding_curve = derive_ata(&bonding_curve, mint, token_program);
    let assoc_user = derive_ata(user, mint, token_program);
    let bonding_curve_v2 = derive_bonding_curve_v2(mint);

    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&SELL_DISC);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());

    let mut accounts = vec![
        AccountMeta::new_readonly(*GLOBAL_PDA, false),           //  0
        AccountMeta::new(*fee_recipient, false),                 //  1
        AccountMeta::new_readonly(*mint, false),                 //  2
        AccountMeta::new(bonding_curve, false),                  //  3
        AccountMeta::new(assoc_bonding_curve, false),            //  4
        AccountMeta::new(assoc_user, false),                     //  5
        AccountMeta::new(*user, true),                           //  6
        AccountMeta::new_readonly(system_program::id(), false),  //  7
        AccountMeta::new(*creator_vault, false),                 //  8
        AccountMeta::new_readonly(*token_program, false),        //  9
        AccountMeta::new_readonly(*EVENT_AUTHORITY, false),      // 10
        AccountMeta::new_readonly(*PUMP_PROGRAM_ID, false),      // 11
        AccountMeta::new_readonly(*FEE_CONFIG_PDA, false),       // 12
        AccountMeta::new_readonly(*FEE_PROGRAM_ID, false),       // 13
    ];
    if is_cashback {
        accounts.push(AccountMeta::new(derive_user_volume_accumulator(user), false)); // 14
    }
    accounts.push(AccountMeta::new(bonding_curve_v2, false)); // 14 or 15

    Instruction::new_with_bytes(*PUMP_PROGRAM_ID, &data, accounts)
}

// =====================================================================
// Trade Parameters
// =====================================================================

pub struct BuyParams {
    pub mint: Pubkey,
    /// Buy mode: either specify token amount (Buy) or SOL amount (BuyExactSolIn).
    pub buy_mode: BuyMode,
    /// Pre-derived creator vault PDA (use [`derive_creator_vault`]).
    pub creator_vault: Pubkey,
    /// Defaults to SPL Token program if `None`.
    pub token_program: Option<Pubkey>,
    /// Fee recipient from the Global account. Read via [`read_fee_recipient`].
    pub fee_recipient: Pubkey,
    pub recent_blockhash: Hash,
    /// Defaults to 200 000 if `None`.
    pub compute_unit_limit: Option<u32>,
    /// If `Some`, a `SetComputeUnitPrice` instruction is prepended.
    pub compute_unit_price_micro_lamports: Option<u64>,
}

pub enum BuyMode {
    /// `Buy` instruction: specify token amount and max SOL willing to pay.
    Buy {
        token_amount: u64,
        max_sol_cost: u64,
    },
    /// `BuyExactSolIn` instruction: specify exact SOL input, let program calculate tokens.
    ExactSolIn {
        amount_sol_lamports: u64,
        min_tokens_out: u64,
        track_volume: bool,
    },
}

pub struct SellParams {
    pub mint: Pubkey,
    pub amount_tokens: u64,
    pub min_sol_out: u64,
    pub creator_vault: Pubkey,
    pub token_program: Option<Pubkey>,
    pub fee_recipient: Pubkey,
    pub recent_blockhash: Hash,
    /// Bonding curve byte[82]: true if cashback coin (adds user_volume_accumulator to sell).
    pub is_cashback: bool,
    pub compute_unit_limit: Option<u32>,
    pub compute_unit_price_micro_lamports: Option<u64>,
}

// =====================================================================
// Transaction Builders  (pure, no RPC)
// =====================================================================

/// Construct a fully-signed buy transaction. No network calls.
pub fn build_buy_transaction(
    signer: &Keypair,
    params: &BuyParams,
) -> Result<Transaction, PumpError> {
    let user = signer.pubkey();
    let token_prog = params.token_program.unwrap_or(*TOKEN_PROGRAM_ID);
    let cu_limit = params.compute_unit_limit.unwrap_or(200_000);

    let mut ixs = Vec::with_capacity(4);
    ixs.push(compute_unit_limit_ix(cu_limit));
    if let Some(price) = params.compute_unit_price_micro_lamports {
        ixs.push(compute_unit_price_ix(price));
    }
    ixs.push(create_ata_idempotent_ix(
        &user,
        &user,
        &params.mint,
        &token_prog,
    ));
    match &params.buy_mode {
        BuyMode::Buy { token_amount, max_sol_cost } => {
            ixs.push(buy_ix(
                *token_amount,
                *max_sol_cost,
                &user,
                &params.mint,
                &params.creator_vault,
                &token_prog,
                &params.fee_recipient,
            ));
        }
        BuyMode::ExactSolIn { amount_sol_lamports, min_tokens_out, track_volume } => {
            ixs.push(buy_exact_sol_in_ix(
                *amount_sol_lamports,
                *min_tokens_out,
                *track_volume,
                &user,
                &params.mint,
                &params.creator_vault,
                &token_prog,
                &params.fee_recipient,
            ));
        }
    }

    Ok(Transaction::new_signed_with_payer(
        &ixs,
        Some(&user),
        &[signer],
        params.recent_blockhash,
    ))
}

/// Construct a fully-signed sell transaction. No network calls.
pub fn build_sell_transaction(
    signer: &Keypair,
    params: &SellParams,
) -> Result<Transaction, PumpError> {
    let user = signer.pubkey();
    let token_prog = params.token_program.unwrap_or(*TOKEN_PROGRAM_ID);
    let cu_limit = params.compute_unit_limit.unwrap_or(200_000);

    let mut ixs = Vec::with_capacity(3);
    ixs.push(compute_unit_limit_ix(cu_limit));
    if let Some(price) = params.compute_unit_price_micro_lamports {
        ixs.push(compute_unit_price_ix(price));
    }
    ixs.push(sell_ix(
        params.amount_tokens,
        params.min_sol_out,
        &user,
        &params.mint,
        &params.creator_vault,
        &token_prog,
        &params.fee_recipient,
        params.is_cashback,
    ));

    Ok(Transaction::new_signed_with_payer(
        &ixs,
        Some(&user),
        &[signer],
        params.recent_blockhash,
    ))
}

// =====================================================================
// Send helper
// =====================================================================

pub fn send_transaction(
    rpc_client: &RpcClient,
    transaction: &Transaction,
) -> Result<Signature, PumpError> {
    Ok(rpc_client.send_transaction_with_config(
        transaction,
        RpcSendTransactionConfig {
            skip_preflight: true,
            preflight_commitment: Some(CommitmentLevel::Confirmed),
            ..Default::default()
        },
    )?)
}

// =====================================================================
// QuickBuy / QuickSell  (build → sign → send)
// =====================================================================

/// Build, sign and send a pump.fun buy transaction.
///
/// The only network call is `sendTransaction`; all accounts are derived
/// locally from the supplied parameters.
pub fn quick_buy(
    rpc_client: &RpcClient,
    signer: &Keypair,
    params: &BuyParams,
) -> Result<Signature, PumpError> {
    let tx = build_buy_transaction(signer, params)?;
    send_transaction(rpc_client, &tx)
}

/// Build, sign and send a pump.fun sell transaction.
pub fn quick_sell(
    rpc_client: &RpcClient,
    signer: &Keypair,
    params: &SellParams,
) -> Result<Signature, PumpError> {
    let tx = build_sell_transaction(signer, params)?;
    send_transaction(rpc_client, &tx)
}
