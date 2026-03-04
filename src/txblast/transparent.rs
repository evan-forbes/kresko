use anyhow::{Context, Result};

use super::rpc::ZebraRpcClient;

/// Addresses reused across transparent transactions to avoid per-tx RPC overhead.
pub struct TransparentAddrs {
    pub to: String,
    pub change: String,
}

impl TransparentAddrs {
    pub async fn init(client: &ZebraRpcClient) -> Result<Self> {
        let to = client.get_new_address().await?;
        let change = client.get_new_address().await?;
        Ok(Self { to, change })
    }
}

/// Send a transparent transaction using pre-allocated addresses.
pub async fn send_tx(
    client: &ZebraRpcClient,
    addrs: &TransparentAddrs,
    amount: f64,
) -> Result<String> {
    // List unspent outputs
    let utxos = client.list_unspent().await?;
    let utxos = utxos
        .as_array()
        .context("listunspent did not return array")?;

    if utxos.is_empty() {
        anyhow::bail!("no unspent transparent outputs available");
    }

    // Pick the first UTXO with sufficient balance
    let utxo = utxos
        .iter()
        .find(|u| u["amount"].as_f64().unwrap_or(0.0) >= amount + 0.0001)
        .context("no UTXO with sufficient balance")?;

    let txid = utxo["txid"].as_str().context("utxo missing txid")?;
    let vout = utxo["vout"].as_u64().context("utxo missing vout")?;
    let utxo_amount = utxo["amount"].as_f64().unwrap_or(0.0);

    // Fee
    let fee = 0.0001_f64;
    let change = utxo_amount - amount - fee;

    let inputs = serde_json::json!([{
        "txid": txid,
        "vout": vout,
    }]);

    let mut outputs = serde_json::json!({
        &addrs.to: amount,
    });

    // Add change output if significant
    if change > 0.00001 {
        outputs[&addrs.change] = serde_json::json!(change);
    }

    let raw_tx = client.create_raw_transaction(inputs, outputs).await?;
    let signed_tx = client.sign_raw_transaction(&raw_tx).await?;
    let txid = client.send_raw_transaction(&signed_tx).await?;

    Ok(txid)
}
