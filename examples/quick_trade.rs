use solana_client::rpc_client::RpcClient;
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey, signer::Signer};
use solana_trade::pump::{self, *};
use std::str::FromStr;

fn main() {
    let rpc_url = "https://api.mainnet-beta.solana.com";
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    // ---- 填入你的私钥（base58 格式）----
    let private_key_base58 = std::env::var("SOLANA_PRIVATE_KEY")
        .expect("请设置环境变量 SOLANA_PRIVATE_KEY（base58 格式私钥）");

    let signer = keypair_from_base58(&private_key_base58).expect("私钥解析失败");
    println!("钱包地址: {}", signer.pubkey());

    let mint_str = std::env::var("MINT").unwrap_or("7epcMCrN9pPb8nhEYwovZd56gFoF7a7GLhBVwbgepump".into());
    let mint = Pubkey::from_str(&mint_str).expect("无效的 mint 地址");

    // creator_vault 需要从 bonding curve 的 creator 字段派生。
    // 如果你已知 creator 公钥，可以这样派生：
    //   let creator_vault = derive_creator_vault(&creator_pubkey);
    //
    // 这里演示从 RPC 读取 bonding curve account 来解析 creator。
    let bonding_curve = derive_bonding_curve(&mint);
    println!("Bonding curve: {bonding_curve}");

    let (creator_vault, is_cashback) = fetch_creator_vault_and_cashback(&rpc_client, &bonding_curve);
    println!("Creator vault: {creator_vault}");
    println!("Is cashback: {is_cashback}");

    // 检测 mint 使用的是哪个 Token 程序
    let mint_account = rpc_client.get_account(&mint).expect("无法读取 mint 账户");
    let token_prog = mint_account.owner;
    println!("Token program: {token_prog}");

    // 从 Global 账户读取 fee_recipient
    let fee_recipient = read_fee_recipient(&rpc_client).expect("读取 fee_recipient 失败");
    println!("Fee recipient: {fee_recipient}");

    let blockhash = rpc_client
        .get_latest_blockhash()
        .expect("获取 blockhash 失败");

    // ======================== Buy 0.001 SOL ========================
    let buy_params = BuyParams {
        mint,
        buy_mode: BuyMode::ExactSolIn {
            amount_sol_lamports: 1_000_000, // 0.001 SOL
            min_tokens_out: 1,
            track_volume: true,
        },
        creator_vault,
        token_program: Some(token_prog),
        fee_recipient,
        recent_blockhash: blockhash,
        compute_unit_limit: Some(300_000),
        compute_unit_price_micro_lamports: Some(100_000),
    };

    println!("\n>>> 发送 Buy 交易 (0.001 SOL) ...");
    match quick_buy(&rpc_client, &signer, &buy_params) {
        Ok(sig) => println!("Buy 成功! signature: {sig}"),
        Err(e) => {
            eprintln!("Buy 失败: {e}");
            return;
        }
    }

    // 等交易确认
    println!("等待交易确认...");
    let user_ata = derive_ata(&signer.pubkey(), &mint, &token_prog);
    let mut sell_amount: u64 = 0;
    for i in 1..=15 {
        std::thread::sleep(std::time::Duration::from_secs(2));
        match rpc_client.get_token_account_balance(&user_ata) {
            Ok(bal) => {
                sell_amount = bal.amount.parse().unwrap_or(0);
                if sell_amount > 0 {
                    println!("[{i}] Token 余额: {} (raw: {sell_amount})", bal.ui_amount_string);
                    break;
                }
                println!("[{i}] 余额为 0，继续等待...");
            }
            Err(_) => println!("[{i}] 账户尚未可见，继续等待..."),
        }
    }

    if sell_amount == 0 {
        println!("Token 余额为 0，跳过卖出");
        return;
    }

    let blockhash = rpc_client
        .get_latest_blockhash()
        .expect("获取 blockhash 失败");

    let sell_params = SellParams {
        mint,
        amount_tokens: sell_amount,
        min_sol_out: 0,
        creator_vault,
        token_program: Some(token_prog),
        fee_recipient,
        recent_blockhash: blockhash,
        is_cashback,
        compute_unit_limit: Some(300_000),
        compute_unit_price_micro_lamports: Some(100_000),
    };

    println!(">>> 发送 Sell 交易 (全部 {sell_amount} tokens) ...");
    match quick_sell(&rpc_client, &signer, &sell_params) {
        Ok(sig) => println!("Sell 成功! signature: {sig}"),
        Err(e) => eprintln!("Sell 失败: {e}"),
    }
}

/// Read creator and cashback flag from bonding curve account.
/// Layout: disc(8) + reserves(40) + complete(1) + creator(32) + is_mayhem(1) + is_cashback(1)
fn fetch_creator_vault_and_cashback(rpc_client: &RpcClient, bonding_curve: &Pubkey) -> (Pubkey, bool) {
    let account = rpc_client
        .get_account(bonding_curve)
        .expect("无法读取 bonding curve 账户，代币可能不存在或已毕业");

    let data = &account.data;
    assert!(data.len() >= 81, "bonding curve 数据长度不足: {}", data.len());

    let creator_bytes: [u8; 32] = data[49..81].try_into().unwrap();
    let creator = Pubkey::new_from_array(creator_bytes);
    println!("Creator: {creator}");

    let is_cashback = data.len() > 82 && data[82] == 1;

    (derive_creator_vault(&creator), is_cashback)
}
