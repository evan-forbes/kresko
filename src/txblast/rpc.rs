use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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

    pub async fn get_new_address(&self) -> Result<String> {
        let result = self.call("getnewaddress", serde_json::json!([])).await?;
        result
            .as_str()
            .map(|s| s.to_string())
            .context("unexpected getnewaddress response")
    }

    pub async fn z_get_new_address(&self, addr_type: &str) -> Result<String> {
        let result = self
            .call("z_getnewaddress", serde_json::json!([addr_type]))
            .await?;
        result
            .as_str()
            .map(|s| s.to_string())
            .context("unexpected z_getnewaddress response")
    }

    pub async fn z_send_many(&self, from_address: &str, amounts: &[(&str, f64)]) -> Result<String> {
        let outputs: Vec<Value> = amounts
            .iter()
            .map(|(addr, amt)| {
                serde_json::json!({
                    "address": addr,
                    "amount": amt,
                })
            })
            .collect();

        let result = self
            .call("z_sendmany", serde_json::json!([from_address, outputs]))
            .await?;
        result
            .as_str()
            .map(|s| s.to_string())
            .context("unexpected z_sendmany response")
    }

    pub async fn z_get_operation_status(&self, op_ids: &[&str]) -> Result<Value> {
        self.call("z_getoperationstatus", serde_json::json!([op_ids]))
            .await
    }

    pub async fn list_unspent(&self) -> Result<Value> {
        self.call("listunspent", serde_json::json!([])).await
    }

    pub async fn z_list_unspent(&self) -> Result<Value> {
        self.call("z_listunspent", serde_json::json!([])).await
    }

    pub async fn create_raw_transaction(&self, inputs: Value, outputs: Value) -> Result<String> {
        let result = self
            .call("createrawtransaction", serde_json::json!([inputs, outputs]))
            .await?;
        result
            .as_str()
            .map(|s| s.to_string())
            .context("unexpected createrawtransaction response")
    }

    pub async fn sign_raw_transaction(&self, hex_tx: &str) -> Result<String> {
        let result = self
            .call("signrawtransaction", serde_json::json!([hex_tx]))
            .await?;
        result["hex"]
            .as_str()
            .map(|s| s.to_string())
            .context("unexpected signrawtransaction response")
    }
}
