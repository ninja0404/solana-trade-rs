use solana_client::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signer::Signer};
use solana_trade::pump::keypair_from_base58;
use solana_trade::pump_amm::*;
use std::str::FromStr;
#[allow(unused_imports)]
use std::str;

fn main() {
    let rpc_url = "https://api.mainnet-beta.solana.com";
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    let private_key = std::env::var("SOLANA_PRIVATE_KEY")
        .expect("请设置环境变量 SOLANA_PRIVATE_KEY");
    let signer = keypair_from_base58(&private_key).expect("私钥解析失败");
    println!("钱包: {}", signer.pubkey());

    // PumpAMM pool 地址（可通过 POOL 环境变量传入）
    let pool_str = std::env::var("POOL").expect("请设置环境变量 POOL（PumpAMM pool 地址）");
    let pool_addr = Pubkey::from_str(&pool_str).expect("无效 pool 地址");

    println!("读取 pool 数据: {pool_addr}");
    let ctx = read_amm_swap_context(&rpc, &pool_addr).expect("读取 pool 失败");

    println!("Base mint:  {} ({})", ctx.base_mint, if ctx.is_reversed { "WSOL/reversed" } else { "Token" });
    println!("Quote mint: {} ({})", ctx.quote_mint, if ctx.is_reversed { "Token" } else { "WSOL" });
    println!("Base reserve:  {}", ctx.base_reserve);
    println!("Quote reserve: {}", ctx.quote_reserve);
    println!("Creator: {}", ctx.coin_creator);
    println!("Fee recipient: {}", ctx.protocol_fee_recipient);
    println!("Reversed: {}", ctx.is_reversed);

    // 估算 0.001 SOL 能买到多少 token（假设总费率 125 bps）
    let sol_in = 1_000_000u64; // 0.001 SOL
    let est_tokens = amm_quote_buy(&ctx, sol_in, 125);
    println!("\n报价: {sol_in} lamports -> ~{est_tokens} tokens (fee=125bps)");

    let blockhash = rpc.get_latest_blockhash().expect("获取 blockhash 失败");

    // ======================== Buy ========================
    let buy_params = AmmBuyParams {
        sol_amount_lamports: sol_in,
        slippage_bps: 1000, // 10%
        recent_blockhash: blockhash,
        compute_unit_limit: Some(300_000),
        compute_unit_price_micro_lamports: Some(100_000),
    };

    println!("\n>>> AMM Buy (0.001 SOL) ...");
    match quick_amm_buy(&rpc, &signer, &ctx, &buy_params) {
        Ok(sig) => println!("Buy 成功! signature: {sig}"),
        Err(e) => {
            eprintln!("Buy 失败: {e}");
            return;
        }
    }

    // 等待确认
    println!("等待确认...");
    let token_mint = if ctx.is_reversed { &ctx.quote_mint } else { &ctx.base_mint };
    let token_prog = if ctx.is_reversed { &ctx.quote_token_program } else { &ctx.base_token_program };
    let user_ata = solana_trade::pump::derive_ata(&signer.pubkey(), token_mint, token_prog);

    let mut sell_amount: u64 = 0;
    for i in 1..=15 {
        std::thread::sleep(std::time::Duration::from_secs(2));
        match rpc.get_token_account_balance(&user_ata) {
            Ok(bal) => {
                sell_amount = bal.amount.parse().unwrap_or(0);
                if sell_amount > 0 {
                    println!("[{i}] Token: {} (raw: {sell_amount})", bal.ui_amount_string);
                    break;
                }
            }
            Err(_) => println!("[{i}] 等待..."),
        }
    }

    if sell_amount == 0 {
        println!("余额为 0，跳过卖出");
        return;
    }

    // ======================== Sell ========================
    let blockhash = rpc.get_latest_blockhash().expect("获取 blockhash 失败");
    let sell_params = AmmSellParams {
        token_amount: sell_amount,
        min_sol_out: 0,
        recent_blockhash: blockhash,
        compute_unit_limit: Some(300_000),
        compute_unit_price_micro_lamports: Some(100_000),
    };

    println!(">>> AMM Sell (全部 {sell_amount} tokens) ...");
    match quick_amm_sell(&rpc, &signer, &ctx, &sell_params) {
        Ok(sig) => println!("Sell 成功! signature: {sig}"),
        Err(e) => eprintln!("Sell 失败: {e}"),
    }
}
