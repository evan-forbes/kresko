pub mod rpc;
pub mod shielded;
pub mod transparent;

use anyhow::Result;
use std::time::Duration;

use crate::config::TxType;

/// Run the transaction blaster locally (called on remote nodes).
pub async fn run_local(rpc_endpoint: &str, tx_type: TxType, rate: u64, amount: f64) -> Result<()> {
    println!(
        "Starting txblast (endpoint={rpc_endpoint}, type={tx_type}, rate={rate}/s, amount={amount})"
    );

    let client = rpc::ZebraRpcClient::new(rpc_endpoint);

    // Verify connection
    let info = client.get_blockchain_info().await?;
    println!(
        "Connected to zebrad: chain={}, blocks={}",
        info["chain"].as_str().unwrap_or("unknown"),
        info["blocks"].as_u64().unwrap_or(0),
    );

    // Pre-allocate addresses to reuse across transactions
    let t_addrs = match tx_type {
        TxType::Transparent | TxType::Both => {
            Some(transparent::TransparentAddrs::init(&client).await?)
        }
        _ => None,
    };
    let s_addr = match tx_type {
        TxType::Shielded | TxType::Both => Some(shielded::ShieldedAddr::init(&client).await?),
        _ => None,
    };

    let interval = Duration::from_secs_f64(1.0 / rate as f64);
    let mut ticker = tokio::time::interval(interval);
    let mut tx_count: u64 = 0;

    loop {
        ticker.tick().await;

        let result = match tx_type {
            TxType::Transparent => {
                transparent::send_tx(&client, t_addrs.as_ref().unwrap(), amount).await
            }
            TxType::Shielded => {
                shielded::send_tx(&client, s_addr.as_ref().unwrap(), amount).await
            }
            TxType::Both => {
                if tx_count % 2 == 0 {
                    transparent::send_tx(&client, t_addrs.as_ref().unwrap(), amount).await
                } else {
                    shielded::send_tx(&client, s_addr.as_ref().unwrap(), amount).await
                }
            }
        };

        tx_count += 1;

        match result {
            Ok(txid) => {
                if tx_count % 100 == 0 {
                    println!("[{tx_count}] sent tx: {txid}");
                }
            }
            Err(e) => {
                eprintln!("[{tx_count}] tx failed: {e}");
                // Brief pause on error to avoid hammering
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}
