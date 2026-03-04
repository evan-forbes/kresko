use anyhow::{Context, Result};
use std::time::Duration;

use super::rpc::ZebraRpcClient;

/// Pre-allocated shielded address reused across transactions.
pub struct ShieldedAddr {
    pub to: String,
}

impl ShieldedAddr {
    pub async fn init(client: &ZebraRpcClient) -> Result<Self> {
        let to = client.z_get_new_address("sapling").await?;
        Ok(Self { to })
    }
}

/// Send a shielded transaction using z_sendmany with a pre-allocated destination address.
pub async fn send_tx(
    client: &ZebraRpcClient,
    addr: &ShieldedAddr,
    amount: f64,
) -> Result<String> {
    // Find a funded address from z_listunspent
    let unspent = client.z_list_unspent().await?;
    let unspent = unspent.as_array().context("z_listunspent not array")?;

    if unspent.is_empty() {
        anyhow::bail!("no shielded balance available");
    }

    let funded = unspent
        .iter()
        .find(|u| u["amount"].as_f64().unwrap_or(0.0) >= amount + 0.0001)
        .context("no shielded UTXO with sufficient balance")?;

    let funded_address = funded["address"].as_str().context("missing address")?;

    let op_id = client
        .z_send_many(funded_address, &[(&addr.to, amount)])
        .await?;

    wait_for_operation(client, &op_id).await
}

async fn wait_for_operation(client: &ZebraRpcClient, op_id: &str) -> Result<String> {
    for _ in 0..120 {
        tokio::time::sleep(Duration::from_secs(1)).await;

        let status = client.z_get_operation_status(&[op_id]).await?;
        let ops = status.as_array().context("operation status not array")?;

        if let Some(op) = ops.first() {
            let state = op["status"].as_str().unwrap_or("");
            match state {
                "success" => {
                    let txid = op["result"]["txid"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string();
                    return Ok(txid);
                }
                "failed" => {
                    let msg = op["error"]["message"].as_str().unwrap_or("unknown error");
                    anyhow::bail!("shielded tx failed: {msg}");
                }
                _ => continue,
            }
        }
    }

    anyhow::bail!("shielded tx operation {op_id} timed out");
}
