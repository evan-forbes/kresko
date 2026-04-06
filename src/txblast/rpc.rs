use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct AddressUtxo {
    pub txid: String,
    #[serde(rename = "outputIndex")]
    pub output_index: u32,
    pub script: String,
    pub satoshis: u64,
    #[serde(default)]
    pub height: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawTransactionVerbose {
    pub vin: Vec<RawTransactionInput>,
    #[serde(default)]
    pub blockhash: Option<String>,
    #[serde(default)]
    pub confirmations: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawTransactionInput {
    pub coinbase: Option<String>,
}

pub struct ZebraRpcClient {
    client: Client,
    url: String,
    id_counter: AtomicU64,
}

impl ZebraRpcClient {
    pub fn new(url: &str) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to create HTTP client");

        Self {
            client,
            url: url.to_string(),
            id_counter: AtomicU64::new(1),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.id_counter.fetch_add(1, Ordering::SeqCst);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("RPC call to {method} failed"))?;

        let json: Value = resp.json().await.context("failed to parse RPC response")?;

        if let Some(error) = json.get("error") {
            if !error.is_null() {
                anyhow::bail!("RPC error from {method}: {error}");
            }
        }

        Ok(json["result"].clone())
    }

    pub async fn get_blockchain_info(&self) -> Result<Value> {
        self.call("getblockchaininfo", serde_json::json!([])).await
    }

    pub async fn send_raw_transaction(&self, hex_tx: &str) -> Result<String> {
        let result = self
            .call("sendrawtransaction", serde_json::json!([hex_tx]))
            .await?;
        result
            .as_str()
            .map(|s| s.to_string())
            .context("unexpected sendrawtransaction response")
    }

    pub async fn get_address_utxos(&self, address: &str) -> Result<Vec<AddressUtxo>> {
        let result = self
            .call(
                "getaddressutxos",
                serde_json::json!([{
                    "addresses": [address],
                }]),
            )
            .await?;

        serde_json::from_value(result).context("unexpected getaddressutxos response")
    }

    pub async fn get_raw_transaction_verbose(&self, txid: &str) -> Result<RawTransactionVerbose> {
        let result = self
            .call("getrawtransaction", serde_json::json!([txid, 1]))
            .await?;

        serde_json::from_value(result).context("unexpected getrawtransaction response")
    }

    pub async fn try_get_raw_transaction_verbose(
        &self,
        txid: &str,
    ) -> Result<Option<RawTransactionVerbose>> {
        match self.get_raw_transaction_verbose(txid).await {
            Ok(tx) => Ok(Some(tx)),
            Err(error) if is_missing_transaction_error(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn get_block_count(&self) -> Result<u32> {
        let result = self.call("getblockcount", serde_json::json!([])).await?;
        result
            .as_u64()
            .map(|n| n as u32)
            .context("unexpected getblockcount response")
    }

    pub async fn get_best_block_hash(&self) -> Result<String> {
        let result = self.call("getbestblockhash", serde_json::json!([])).await?;
        result
            .as_str()
            .map(ToOwned::to_owned)
            .context("unexpected getbestblockhash response")
    }

    pub async fn get_block_hash(&self, height: u32) -> Result<String> {
        let result = self
            .call("getblockhash", serde_json::json!([height]))
            .await?;
        result
            .as_str()
            .map(ToOwned::to_owned)
            .context("unexpected getblockhash response")
    }

    pub async fn z_get_treestate(&self, height: u32) -> Result<Value> {
        self.call("z_gettreestate", serde_json::json!([height.to_string()]))
            .await
    }

    pub async fn getblock_raw(&self, height: u32) -> Result<Vec<u8>> {
        let result = self
            .call("getblock", serde_json::json!([height.to_string(), 0]))
            .await?;
        let hex_str = result
            .as_str()
            .context("unexpected getblock response: expected hex string")?;
        hex::decode(hex_str).context("invalid hex in getblock response")
    }
}

fn is_missing_transaction_error(error: &anyhow::Error) -> bool {
    let error = error.to_string().to_ascii_lowercase();
    error.contains("no such mempool or blockchain transaction")
        || error.contains("transaction not found")
        || error.contains("not found")
}
