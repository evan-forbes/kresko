//! Seed a running node's chain state from a generated genesis payload.
//!
//! Mirrors the seeding half of `scripts/node_init.sh` (and the harness's former
//! `seed_node`/`submit_block`): submit the genesis block, then every premine
//! block, to a node's RPC via `submitblock`, tolerating the duplicate/rejected
//! retry semantics a freshly-started node needs. The caller owns the node
//! process lifecycle; this command only drives the RPC.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::time::Duration;

use crate::txblast::rpc::ZebraRpcClient;

/// A `rejected` result can mean the node is still opening its state DB, so retry
/// a bounded number of times before treating it as a real failure.
const SUBMIT_RETRIES: usize = 10;
const SUBMIT_RETRY_DELAY: Duration = Duration::from_secs(2);

pub async fn run(rpc_endpoint: &str, genesis_path: &str, premine_path: &str) -> Result<()> {
    let genesis_hex = std::fs::read_to_string(genesis_path)
        .with_context(|| format!("reading genesis block from {genesis_path}"))?
        .trim()
        .to_string();
    let premine = std::fs::read_to_string(premine_path)
        .with_context(|| format!("reading premine blocks from {premine_path}"))?;
    let blocks: Vec<&str> = premine
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    let client = ZebraRpcClient::new(rpc_endpoint);

    // Idempotence: a node that already holds the seed chain must not be
    // re-seeded. Resubmitting genesis to a node whose tip has moved past it
    // returns a bare "rejected", which reads as a chain failure rather than
    // "this already ran".
    let height = current_height(&client).await?;
    if height as usize >= blocks.len() {
        println!(
            "already seeded to height {height} (>= {} premine blocks), skipping",
            blocks.len()
        );
        return Ok(());
    }

    submit_block(&client, &genesis_hex, "genesis").await?;
    let total = blocks.len();
    for (n, block_hex) in blocks.iter().enumerate() {
        let ordinal = n + 1;
        submit_block(&client, block_hex, &format!("seed block {ordinal}/{total}")).await?;
        if ordinal == 1 || ordinal % 25 == 0 || ordinal == total {
            println!("seeded {ordinal}/{total} blocks");
        }
    }

    let height = current_height(&client).await?;
    println!("seeded to height {height}");
    Ok(())
}

async fn current_height(client: &ZebraRpcClient) -> Result<u64> {
    let info = client.get_blockchain_info().await?;
    Ok(info.get("blocks").and_then(Value::as_u64).unwrap_or(0))
}

/// `submitblock` with the accept/duplicate/retry semantics node_init.sh uses:
/// `null` means accepted; `"inconclusive"` accepted-but-not-yet-best;
/// `"duplicate*"` already present (fine on a re-run); `"rejected"` is retried a
/// bounded number of times before failing.
async fn submit_block(client: &ZebraRpcClient, block_hex: &str, label: &str) -> Result<()> {
    for attempt in 1..=SUBMIT_RETRIES {
        let result = client
            .call_raw("submitblock", json!([block_hex]))
            .await
            .with_context(|| format!("{label}: submitblock RPC failed"))?;

        if result.is_null() {
            return Ok(());
        }
        if let Some(text) = result.as_str() {
            if text == "inconclusive" || text.starts_with("duplicate") {
                return Ok(());
            }
            if text == "rejected" && attempt < SUBMIT_RETRIES {
                tokio::time::sleep(SUBMIT_RETRY_DELAY).await;
                continue;
            }
        }
        anyhow::bail!("{label}: submitblock returned {result}");
    }
    unreachable!("submit loop returns or bails within {SUBMIT_RETRIES} attempts")
}
