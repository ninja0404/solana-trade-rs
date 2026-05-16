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

pub static WSOL_MINT: LazyLock<Pubkey> =
    LazyLock::new(|| Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap());

pub static BUYBACK_FEE_RECIPIENTS: LazyLock<[Pubkey; 8]> = LazyLock::new(|| {
    [
        Pubkey::from_str("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD").unwrap(),
        Pubkey::from_str("9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7").unwrap(),
        Pubkey::from_str("GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL").unwrap(),
        Pubkey::from_str("3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR").unwrap(),
        Pubkey::from_str("5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6").unwrap(),
        Pubkey::from_str("EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL").unwrap(),
        Pubkey::from_str("5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD").unwrap(),
        Pubkey::from_str("A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW").unwrap(),
    ]
});

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
const BUY_V2_DISC: [u8; 8] = [184, 23, 238, 97, 103, 197, 211, 61];
const BUY_EXACT_QUOTE_IN_V2_DISC: [u8; 8] = [194, 171, 28, 70, 104, 77, 91, 47];
const SELL_V2_DISC: [u8; 8] = [93, 246, 130, 60, 231, 233, 64, 178];

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

pub fn derive_sharing_config(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"sharing-config", mint.as_ref()], &FEE_PROGRAM_ID).0
}

// =====================================================================
// Global Account Reader
// =====================================================================

pub struct BondingCurveInfo {
    pub creator: Pubkey,
    pub is_mayhem_mode: bool,
    pub is_cashback_coin: bool,
    pub quote_mint: Pubkey,
}

impl BondingCurveInfo {
    pub fn quote_mint_or_wsol(&self) -> Pubkey {
        if self.quote_mint == Pubkey::default() {
            *WSOL_MINT
        } else {
            self.quote_mint
        }
    }
}

pub fn read_bonding_curve_info(
    rpc_client: &RpcClient,
    mint: &Pubkey,
) -> Result<BondingCurveInfo, PumpError> {
    let bonding_curve = derive_bonding_curve(mint);
    let account = rpc_client.get_account(&bonding_curve)?;
    let data = &account.data;
    if data.len() < 81 {
        return Err(PumpError::InvalidParam(
            "BondingCurve account data too short".into(),
        ));
    }

    let creator_bytes: [u8; 32] = data[49..81]
        .try_into()
        .map_err(|_| PumpError::InvalidParam("bad creator slice".into()))?;
    let quote_mint = if data.len() >= 115 {
        Pubkey::new_from_array(
            data[83..115]
                .try_into()
                .map_err(|_| PumpError::InvalidParam("bad quote_mint slice".into()))?,
        )
    } else {
        Pubkey::default()
    };

    Ok(BondingCurveInfo {
        creator: Pubkey::new_from_array(creator_bytes),
        is_mayhem_mode: data.get(81).copied().unwrap_or_default() == 1,
        is_cashback_coin: data.get(82).copied().unwrap_or_default() == 1,
        quote_mint,
    })
}

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
        return Err(PumpError::InvalidParam(
            "Global account data too short".into(),
        ));
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

/// Buy instruction – 16 accounts (same layout as BuyExactSolIn).
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
        ],
    )
}

/// BuyExactSolIn instruction – 16 accounts.
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
        ],
    )
}

/// Sell instruction – account count depends on cashback flag:
///   Non-cashback: 14 accounts
///   Cashback:     15 accounts (adds user_volume_accumulator)
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

    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&SELL_DISC);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());

    let mut accounts = vec![
        AccountMeta::new_readonly(*GLOBAL_PDA, false), //  0
        AccountMeta::new(*fee_recipient, false),       //  1
        AccountMeta::new_readonly(*mint, false),       //  2
        AccountMeta::new(bonding_curve, false),        //  3
        AccountMeta::new(assoc_bonding_curve, false),  //  4
        AccountMeta::new(assoc_user, false),           //  5
        AccountMeta::new(*user, true),                 //  6
        AccountMeta::new_readonly(system_program::id(), false), //  7
        AccountMeta::new(*creator_vault, false),       //  8
        AccountMeta::new_readonly(*token_program, false), //  9
        AccountMeta::new_readonly(*EVENT_AUTHORITY, false), // 10
        AccountMeta::new_readonly(*PUMP_PROGRAM_ID, false), // 11
        AccountMeta::new_readonly(*FEE_CONFIG_PDA, false), // 12
        AccountMeta::new_readonly(*FEE_PROGRAM_ID, false), // 13
    ];
    if is_cashback {
        accounts.push(AccountMeta::new(
            derive_user_volume_accumulator(user),
            false,
        )); // 14
    }

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

pub struct BuyV2Params {
    pub base_mint: Pubkey,
    /// Defaults to WSOL if `None`.
    pub quote_mint: Option<Pubkey>,
    pub buy_mode: BuyV2Mode,
    pub creator_vault: Pubkey,
    pub base_token_program: Option<Pubkey>,
    /// Defaults to SPL Token program if `None`.
    pub quote_token_program: Option<Pubkey>,
    pub fee_recipient: Pubkey,
    pub buyback_fee_recipient: Pubkey,
    pub recent_blockhash: Hash,
    pub compute_unit_limit: Option<u32>,
    pub compute_unit_price_micro_lamports: Option<u64>,
}

pub enum BuyV2Mode {
    Buy {
        token_amount: u64,
        max_quote_cost: u64,
    },
    ExactQuoteIn {
        spendable_quote_in: u64,
        min_tokens_out: u64,
    },
}

pub struct SellV2Params {
    pub base_mint: Pubkey,
    /// Defaults to WSOL if `None`.
    pub quote_mint: Option<Pubkey>,
    pub amount_tokens: u64,
    pub min_quote_out: u64,
    pub creator_vault: Pubkey,
    pub base_token_program: Option<Pubkey>,
    /// Defaults to SPL Token program if `None`.
    pub quote_token_program: Option<Pubkey>,
    pub fee_recipient: Pubkey,
    pub buyback_fee_recipient: Pubkey,
    pub recent_blockhash: Hash,
    pub compute_unit_limit: Option<u32>,
    pub compute_unit_price_micro_lamports: Option<u64>,
}

// =====================================================================
// Transaction Builders  (pure, no RPC)
// =====================================================================

fn pump_v2_accounts(
    user: &Pubkey,
    base_mint: &Pubkey,
    quote_mint: &Pubkey,
    base_token_program: &Pubkey,
    quote_token_program: &Pubkey,
    creator_vault: &Pubkey,
    fee_recipient: &Pubkey,
    buyback_fee_recipient: &Pubkey,
    include_global_volume_accumulator: bool,
) -> Vec<AccountMeta> {
    let bonding_curve = derive_bonding_curve(base_mint);
    let associated_quote_fee_recipient = derive_ata(fee_recipient, quote_mint, quote_token_program);
    let associated_quote_buyback_fee_recipient =
        derive_ata(buyback_fee_recipient, quote_mint, quote_token_program);
    let associated_base_bonding_curve = derive_ata(&bonding_curve, base_mint, base_token_program);
    let associated_quote_bonding_curve =
        derive_ata(&bonding_curve, quote_mint, quote_token_program);
    let associated_base_user = derive_ata(user, base_mint, base_token_program);
    let associated_quote_user = derive_ata(user, quote_mint, quote_token_program);
    let associated_creator_vault = derive_ata(creator_vault, quote_mint, quote_token_program);
    let user_volume_accumulator = derive_user_volume_accumulator(user);
    let associated_user_volume_accumulator =
        derive_ata(&user_volume_accumulator, quote_mint, quote_token_program);

    let mut accounts = vec![
        AccountMeta::new_readonly(*GLOBAL_PDA, false),
        AccountMeta::new_readonly(*base_mint, false),
        AccountMeta::new_readonly(*quote_mint, false),
        AccountMeta::new_readonly(*base_token_program, false),
        AccountMeta::new_readonly(*quote_token_program, false),
        AccountMeta::new_readonly(*ATA_PROGRAM_ID, false),
        AccountMeta::new(*fee_recipient, false),
        AccountMeta::new(associated_quote_fee_recipient, false),
        AccountMeta::new(*buyback_fee_recipient, false),
        AccountMeta::new(associated_quote_buyback_fee_recipient, false),
        AccountMeta::new(bonding_curve, false),
        AccountMeta::new(associated_base_bonding_curve, false),
        AccountMeta::new(associated_quote_bonding_curve, false),
        AccountMeta::new(*user, true),
        AccountMeta::new(associated_base_user, false),
        AccountMeta::new(associated_quote_user, false),
        AccountMeta::new(*creator_vault, false),
        AccountMeta::new(associated_creator_vault, false),
        AccountMeta::new_readonly(derive_sharing_config(base_mint), false),
    ];

    if include_global_volume_accumulator {
        accounts.push(AccountMeta::new_readonly(*GLOBAL_VOLUME_ACCUMULATOR, false));
    }
    accounts.push(AccountMeta::new(user_volume_accumulator, false));
    accounts.push(AccountMeta::new(associated_user_volume_accumulator, false));
    accounts.push(AccountMeta::new_readonly(*FEE_CONFIG_PDA, false));
    accounts.push(AccountMeta::new_readonly(*FEE_PROGRAM_ID, false));
    accounts.push(AccountMeta::new_readonly(system_program::id(), false));
    accounts.push(AccountMeta::new_readonly(*EVENT_AUTHORITY, false));
    accounts.push(AccountMeta::new_readonly(*PUMP_PROGRAM_ID, false));
    accounts
}

fn buy_v2_ix(
    amount: u64,
    max_quote_cost: u64,
    user: &Pubkey,
    params: &BuyV2Params,
    quote_mint: &Pubkey,
    base_token_program: &Pubkey,
    quote_token_program: &Pubkey,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&BUY_V2_DISC);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&max_quote_cost.to_le_bytes());

    Instruction::new_with_bytes(
        *PUMP_PROGRAM_ID,
        &data,
        pump_v2_accounts(
            user,
            &params.base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            &params.creator_vault,
            &params.fee_recipient,
            &params.buyback_fee_recipient,
            true,
        ),
    )
}

fn buy_exact_quote_in_v2_ix(
    spendable_quote_in: u64,
    min_tokens_out: u64,
    user: &Pubkey,
    params: &BuyV2Params,
    quote_mint: &Pubkey,
    base_token_program: &Pubkey,
    quote_token_program: &Pubkey,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&BUY_EXACT_QUOTE_IN_V2_DISC);
    data.extend_from_slice(&spendable_quote_in.to_le_bytes());
    data.extend_from_slice(&min_tokens_out.to_le_bytes());

    Instruction::new_with_bytes(
        *PUMP_PROGRAM_ID,
        &data,
        pump_v2_accounts(
            user,
            &params.base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            &params.creator_vault,
            &params.fee_recipient,
            &params.buyback_fee_recipient,
            true,
        ),
    )
}

fn sell_v2_ix(
    amount: u64,
    min_quote_out: u64,
    user: &Pubkey,
    params: &SellV2Params,
    quote_mint: &Pubkey,
    base_token_program: &Pubkey,
    quote_token_program: &Pubkey,
) -> Instruction {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&SELL_V2_DISC);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_quote_out.to_le_bytes());

    Instruction::new_with_bytes(
        *PUMP_PROGRAM_ID,
        &data,
        pump_v2_accounts(
            user,
            &params.base_mint,
            quote_mint,
            base_token_program,
            quote_token_program,
            &params.creator_vault,
            &params.fee_recipient,
            &params.buyback_fee_recipient,
            false,
        ),
    )
}

fn maybe_create_quote_ata_ix(
    user: &Pubkey,
    quote_mint: &Pubkey,
    quote_token_program: &Pubkey,
) -> Option<Instruction> {
    if quote_mint == &*WSOL_MINT {
        None
    } else {
        Some(create_ata_idempotent_ix(
            user,
            user,
            quote_mint,
            quote_token_program,
        ))
    }
}

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
        BuyMode::Buy {
            token_amount,
            max_sol_cost,
        } => {
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
        BuyMode::ExactSolIn {
            amount_sol_lamports,
            min_tokens_out,
            track_volume,
        } => {
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

/// Construct a fully-signed buy_v2 / buy_exact_quote_in_v2 transaction.
pub fn build_buy_v2_transaction(
    signer: &Keypair,
    params: &BuyV2Params,
) -> Result<Transaction, PumpError> {
    let user = signer.pubkey();
    let quote_mint = params.quote_mint.unwrap_or(*WSOL_MINT);
    let base_token_prog = params.base_token_program.unwrap_or(*TOKEN_PROGRAM_ID);
    let quote_token_prog = params.quote_token_program.unwrap_or(*TOKEN_PROGRAM_ID);
    let cu_limit = params.compute_unit_limit.unwrap_or(300_000);

    let mut ixs = Vec::with_capacity(5);
    ixs.push(compute_unit_limit_ix(cu_limit));
    if let Some(price) = params.compute_unit_price_micro_lamports {
        ixs.push(compute_unit_price_ix(price));
    }
    ixs.push(create_ata_idempotent_ix(
        &user,
        &user,
        &params.base_mint,
        &base_token_prog,
    ));
    if let Some(ix) = maybe_create_quote_ata_ix(&user, &quote_mint, &quote_token_prog) {
        ixs.push(ix);
    }
    match &params.buy_mode {
        BuyV2Mode::Buy {
            token_amount,
            max_quote_cost,
        } => {
            ixs.push(buy_v2_ix(
                *token_amount,
                *max_quote_cost,
                &user,
                params,
                &quote_mint,
                &base_token_prog,
                &quote_token_prog,
            ));
        }
        BuyV2Mode::ExactQuoteIn {
            spendable_quote_in,
            min_tokens_out,
        } => {
            ixs.push(buy_exact_quote_in_v2_ix(
                *spendable_quote_in,
                *min_tokens_out,
                &user,
                params,
                &quote_mint,
                &base_token_prog,
                &quote_token_prog,
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

/// Construct a fully-signed sell_v2 transaction.
pub fn build_sell_v2_transaction(
    signer: &Keypair,
    params: &SellV2Params,
) -> Result<Transaction, PumpError> {
    let user = signer.pubkey();
    let quote_mint = params.quote_mint.unwrap_or(*WSOL_MINT);
    let base_token_prog = params.base_token_program.unwrap_or(*TOKEN_PROGRAM_ID);
    let quote_token_prog = params.quote_token_program.unwrap_or(*TOKEN_PROGRAM_ID);
    let cu_limit = params.compute_unit_limit.unwrap_or(300_000);

    let mut ixs = Vec::with_capacity(5);
    ixs.push(compute_unit_limit_ix(cu_limit));
    if let Some(price) = params.compute_unit_price_micro_lamports {
        ixs.push(compute_unit_price_ix(price));
    }
    ixs.push(create_ata_idempotent_ix(
        &user,
        &user,
        &params.base_mint,
        &base_token_prog,
    ));
    if let Some(ix) = maybe_create_quote_ata_ix(&user, &quote_mint, &quote_token_prog) {
        ixs.push(ix);
    }
    ixs.push(sell_v2_ix(
        params.amount_tokens,
        params.min_quote_out,
        &user,
        params,
        &quote_mint,
        &base_token_prog,
        &quote_token_prog,
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

pub fn quick_buy_v2(
    rpc_client: &RpcClient,
    signer: &Keypair,
    params: &BuyV2Params,
) -> Result<Signature, PumpError> {
    let tx = build_buy_v2_transaction(signer, params)?;
    send_transaction(rpc_client, &tx)
}

pub fn quick_sell_v2(
    rpc_client: &RpcClient,
    signer: &Keypair,
    params: &SellV2Params,
) -> Result<Signature, PumpError> {
    let tx = build_sell_v2_transaction(signer, params)?;
    send_transaction(rpc_client, &tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buy_v2_params(mode: BuyV2Mode) -> BuyV2Params {
        BuyV2Params {
            base_mint: Pubkey::new_unique(),
            quote_mint: Some(*WSOL_MINT),
            buy_mode: mode,
            creator_vault: Pubkey::new_unique(),
            base_token_program: Some(*TOKEN_2022_PROGRAM_ID),
            quote_token_program: Some(*TOKEN_PROGRAM_ID),
            fee_recipient: Pubkey::new_unique(),
            buyback_fee_recipient: BUYBACK_FEE_RECIPIENTS[0],
            recent_blockhash: Hash::new_unique(),
            compute_unit_limit: None,
            compute_unit_price_micro_lamports: None,
        }
    }

    fn sell_v2_params() -> SellV2Params {
        SellV2Params {
            base_mint: Pubkey::new_unique(),
            quote_mint: Some(*WSOL_MINT),
            amount_tokens: 1_000,
            min_quote_out: 1,
            creator_vault: Pubkey::new_unique(),
            base_token_program: Some(*TOKEN_2022_PROGRAM_ID),
            quote_token_program: Some(*TOKEN_PROGRAM_ID),
            fee_recipient: Pubkey::new_unique(),
            buyback_fee_recipient: BUYBACK_FEE_RECIPIENTS[0],
            recent_blockhash: Hash::new_unique(),
            compute_unit_limit: None,
            compute_unit_price_micro_lamports: None,
        }
    }

    #[test]
    fn buy_v2_uses_unified_account_layout() {
        let user = Pubkey::new_unique();
        let params = buy_v2_params(BuyV2Mode::Buy {
            token_amount: 1_000,
            max_quote_cost: 2_000,
        });
        let ix = buy_v2_ix(
            1_000,
            2_000,
            &user,
            &params,
            &WSOL_MINT,
            &TOKEN_2022_PROGRAM_ID,
            &TOKEN_PROGRAM_ID,
        );
        assert_eq!(ix.data.len(), 24);
        assert_eq!(ix.accounts.len(), 27);
    }

    #[test]
    fn buy_exact_quote_in_v2_uses_unified_account_layout() {
        let user = Pubkey::new_unique();
        let params = buy_v2_params(BuyV2Mode::ExactQuoteIn {
            spendable_quote_in: 2_000,
            min_tokens_out: 1,
        });
        let ix = buy_exact_quote_in_v2_ix(
            2_000,
            1,
            &user,
            &params,
            &WSOL_MINT,
            &TOKEN_2022_PROGRAM_ID,
            &TOKEN_PROGRAM_ID,
        );
        assert_eq!(ix.data.len(), 24);
        assert_eq!(ix.accounts.len(), 27);
    }

    #[test]
    fn sell_v2_uses_unified_account_layout() {
        let user = Pubkey::new_unique();
        let params = sell_v2_params();
        let ix = sell_v2_ix(
            1_000,
            1,
            &user,
            &params,
            &WSOL_MINT,
            &TOKEN_2022_PROGRAM_ID,
            &TOKEN_PROGRAM_ID,
        );
        assert_eq!(ix.data.len(), 24);
        assert_eq!(ix.accounts.len(), 26);
    }

    #[test]
    fn legacy_cashback_sell_has_single_remaining_account() {
        let user = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let creator_vault = Pubkey::new_unique();
        let fee_recipient = Pubkey::new_unique();
        let ix = sell_ix(
            1_000,
            1,
            &user,
            &mint,
            &creator_vault,
            &TOKEN_PROGRAM_ID,
            &fee_recipient,
            true,
        );
        assert_eq!(ix.accounts.len(), 15);
    }
}
