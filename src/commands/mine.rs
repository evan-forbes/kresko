use anyhow::{Context, Result};
use hex::FromHex;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write as IoWrite;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use zebra_chain::{
    block::{self, Block, Header},
    fmt::HexDebug,
    serialization::{ZcashDeserializeInto, ZcashSerialize},
    work::{
        difficulty::CompactDifficulty,
        equihash::{Solution, SolverCancelled},
    },
};

/// Structured log entry written to mine.log.jsonl on each solve attempt.
#[derive(Debug, Serialize)]
struct MineLogEntry {
    ts_unix_ms: u128,
    height: u64,
    event: &'static str,
    solve_time_ms: Option<u128>,
    block_hash: Option<String>,
    submit_result: Option<String>,
    transactions: Option<usize>,
    error: Option<String>,
}

pub async fn run(rpc_endpoint: &str) -> Result<()> {
    println!("Starting PoW miner against {rpc_endpoint}");

    // Verify connection
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let info = rpc_call(&client, rpc_endpoint, "getblockchaininfo", &[]).await?;
    let chain = info["result"]["chain"]
        .as_str()
        .unwrap_or("unknown");
    let height = info["result"]["blocks"]
        .as_u64()
        .unwrap_or(0);
    println!("Connected: chain={chain}, height={height}");

    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("mine.log.jsonl")?;
    println!("Logging structured metrics to mine.log.jsonl");

    let mut longpollid: Option<String> = None;
    let mut templates_received: u64 = 0;
    let mut solutions_found: u64 = 0;
    let mut blocks_submitted: u64 = 0;
    let mut blocks_rejected: u64 = 0;
    let mut stale_cancellations: u64 = 0;

    loop {
        // 1. Get block template
        let template = match get_block_template(&client, rpc_endpoint, longpollid.as_deref()).await
        {
            Ok(t) => t,
            Err(e) => {
                eprintln!("Failed to get block template: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        templates_received += 1;
        let template_height = template["height"].as_u64().unwrap_or(0);
        longpollid = template["longpollid"].as_str().map(String::from);
        let tx_count = template["transactions"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);

        println!("Got template: height={template_height}, transactions={tx_count}");

        log_entry(
            &mut log_file,
            &MineLogEntry {
                ts_unix_ms: now_unix_ms(),
                height: template_height,
                event: "template_received",
                solve_time_ms: None,
                block_hash: None,
                submit_result: None,
                transactions: Some(tx_count),
                error: None,
            },
        );

        // 2. Parse template into a Block
        let block = match block_from_template(&template) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Failed to parse block template: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let header = *block.header;

        // 3. Set up cancellation via long-poll
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let poll_client = client.clone();
        let poll_endpoint = rpc_endpoint.to_string();
        let poll_lpid = longpollid.clone();
        let poll_handle = tokio::spawn(async move {
            // Long-poll: this request blocks until the template changes
            let _ = get_block_template(&poll_client, &poll_endpoint, poll_lpid.as_deref()).await;
            let _ = cancel_tx.send(true);
        });

        // 4. Solve in a blocking thread
        let solve_start = Instant::now();
        let solve_result = tokio::task::spawn_blocking(move || {
            let cancel_fn = move || {
                if *cancel_rx.borrow() {
                    Err(SolverCancelled)
                } else {
                    Ok(())
                }
            };
            Solution::solve(header, cancel_fn)
        })
        .await
        .context("solver thread panicked")?;

        // 5. Cancel the polling task
        poll_handle.abort();

        let solve_time = solve_start.elapsed();

        match solve_result {
            Ok(solved_headers) => {
                solutions_found += 1;
                let solved_header = solved_headers.into_iter().next().unwrap();
                println!(
                    "Solution found in {:.1}s for height {template_height}",
                    solve_time.as_secs_f64()
                );

                // Reconstruct block with solved header
                let solved_block = Block {
                    header: Arc::new(solved_header),
                    transactions: block.transactions,
                };

                let block_hash = format!("{}", block::Hash::from(&*solved_block.header));

                let mut block_bytes = Vec::new();
                solved_block
                    .zcash_serialize(&mut block_bytes)
                    .context("failed to serialize solved block")?;
                let block_hex = hex::encode(&block_bytes);

                match submit_block(&client, rpc_endpoint, &block_hex).await {
                    Ok(result) => {
                        let result_str = if result.is_null() {
                            None
                        } else {
                            result.as_str().map(String::from)
                        };
                        let accepted = result_str.is_none()
                            || result_str.as_deref() == Some("")
                            || result_str
                                .as_deref()
                                .map_or(false, |s| s.starts_with("duplicate"));

                        if accepted {
                            blocks_submitted += 1;
                            println!("Block submitted at height {template_height}: hash={block_hash}");
                        } else {
                            blocks_rejected += 1;
                            eprintln!(
                                "Block rejected at height {template_height}: {}",
                                result_str.as_deref().unwrap_or("unknown")
                            );
                        }

                        log_entry(
                            &mut log_file,
                            &MineLogEntry {
                                ts_unix_ms: now_unix_ms(),
                                height: template_height,
                                event: "solution_found",
                                solve_time_ms: Some(solve_time.as_millis()),
                                block_hash: Some(block_hash),
                                submit_result: result_str,
                                transactions: Some(tx_count),
                                error: None,
                            },
                        );
                    }
                    Err(e) => {
                        blocks_rejected += 1;
                        eprintln!("Submit failed for height {template_height}: {e}");

                        log_entry(
                            &mut log_file,
                            &MineLogEntry {
                                ts_unix_ms: now_unix_ms(),
                                height: template_height,
                                event: "submit_failed",
                                solve_time_ms: Some(solve_time.as_millis()),
                                block_hash: Some(block_hash),
                                submit_result: None,
                                transactions: Some(tx_count),
                                error: Some(format!("{e}")),
                            },
                        );
                    }
                }

                println!(
                    "  stats: templates={templates_received} solutions={solutions_found} \
                     submitted={blocks_submitted} rejected={blocks_rejected} stale={stale_cancellations}"
                );
            }
            Err(SolverCancelled) => {
                stale_cancellations += 1;
                println!("Template changed after {:.1}s, restarting solver...", solve_time.as_secs_f64());

                log_entry(
                    &mut log_file,
                    &MineLogEntry {
                        ts_unix_ms: now_unix_ms(),
                        height: template_height,
                        event: "solver_cancelled",
                        solve_time_ms: Some(solve_time.as_millis()),
                        block_hash: None,
                        submit_result: None,
                        transactions: Some(tx_count),
                        error: None,
                    },
                );
            }
        }
    }
}

fn log_entry(file: &mut std::fs::File, entry: &MineLogEntry) {
    if let Ok(line) = serde_json::to_string(entry) {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

async fn get_block_template(
    client: &reqwest::Client,
    endpoint: &str,
    longpollid: Option<&str>,
) -> Result<serde_json::Value> {
    let mut params = serde_json::json!({
        "mode": "template",
        "capabilities": ["coinbasetxn", "longpoll"],
    });

    if let Some(lpid) = longpollid {
        params["longpollid"] = serde_json::Value::String(lpid.to_string());
    }

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "kresko-mine",
        "method": "getblocktemplate",
        "params": [params],
    });

    // Use a long timeout for long-polling requests
    let timeout = if longpollid.is_some() {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(30)
    };

    let resp = client
        .post(endpoint)
        .timeout(timeout)
        .json(&body)
        .send()
        .await
        .context("getblocktemplate request failed")?;

    let payload: serde_json::Value = resp.json().await.context("failed to parse RPC response")?;

    if let Some(err) = payload.get("error").filter(|v| !v.is_null()) {
        anyhow::bail!("getblocktemplate RPC error: {err}");
    }

    payload
        .get("result")
        .cloned()
        .context("missing result in getblocktemplate response")
}

async fn submit_block(
    client: &reqwest::Client,
    endpoint: &str,
    block_hex: &str,
) -> Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "kresko-mine",
        "method": "submitblock",
        "params": [block_hex],
    });

    let resp = client
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .context("submitblock request failed")?;

    let payload: serde_json::Value = resp.json().await.context("failed to parse RPC response")?;

    if let Some(err) = payload.get("error").filter(|v| !v.is_null()) {
        anyhow::bail!("submitblock RPC error: {err}");
    }

    Ok(payload
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

async fn rpc_call(
    client: &reqwest::Client,
    endpoint: &str,
    method: &str,
    params: &[serde_json::Value],
) -> Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "kresko-mine",
        "method": method,
        "params": params,
    });

    let resp = client
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("{method} request failed"))?;

    let payload: serde_json::Value = resp.json().await.context("failed to parse RPC response")?;

    if let Some(err) = payload.get("error").filter(|v| !v.is_null()) {
        anyhow::bail!("{method} RPC error: {err}");
    }

    Ok(payload)
}

/// Construct a zebra-chain `Block` from a `getblocktemplate` JSON response.
///
/// This reimplements the logic from `zebra_rpc::proposal_block_from_template` using
/// only `zebra-chain` types, avoiding the heavy `zebra-rpc` dependency tree.
fn block_from_template(template: &serde_json::Value) -> Result<Block> {
    let version = template["version"]
        .as_u64()
        .context("missing version")? as u32;

    let prev_hash_hex = template["previousblockhash"]
        .as_str()
        .context("missing previousblockhash")?;
    let previous_block_hash =
        block::Hash::from_hex(prev_hash_hex).context("invalid previousblockhash hex")?;

    let default_roots = &template["defaultroots"];

    let merkle_root_hex = default_roots["merkleroot"]
        .as_str()
        .context("missing defaultroots.merkleroot")?;
    let merkle_root_bytes =
        hex_to_32_bytes(merkle_root_hex).context("invalid merkleroot hex")?;
    let merkle_root = block::merkle::Root(merkle_root_bytes);

    // All kresko experiments activate NU6.1 at height 1, so we always use
    // the NU5+ commitment path (blockcommitmentshash).
    let commitment_hex = default_roots["blockcommitmentshash"]
        .as_str()
        .context("missing defaultroots.blockcommitmentshash")?;
    let commitment_bytes =
        hex_to_32_bytes(commitment_hex).context("invalid blockcommitmentshash hex")?;

    let bits_hex = template["bits"]
        .as_str()
        .context("missing bits")?;
    let difficulty_threshold = CompactDifficulty::from_hex(bits_hex)
        .map_err(|e| anyhow::anyhow!("invalid bits hex: {e}"))?;

    let cur_time = template["curtime"]
        .as_u64()
        .context("missing curtime")? as i64;
    let time = chrono::DateTime::from_timestamp(cur_time, 0)
        .context("invalid curtime timestamp")?;

    // Parse transactions
    let coinbase_hex = template["coinbasetxn"]["data"]
        .as_str()
        .context("missing coinbasetxn.data")?;
    let coinbase_bytes = hex::decode(coinbase_hex).context("invalid coinbase hex")?;
    let mut transactions: Vec<Arc<zebra_chain::transaction::Transaction>> =
        vec![coinbase_bytes
            .zcash_deserialize_into()
            .context("failed to deserialize coinbase transaction")?];

    if let Some(tx_templates) = template["transactions"].as_array() {
        for tx_template in tx_templates {
            let tx_hex = tx_template["data"]
                .as_str()
                .context("missing transaction data")?;
            let tx_bytes = hex::decode(tx_hex).context("invalid transaction hex")?;
            transactions.push(
                tx_bytes
                    .zcash_deserialize_into()
                    .context("failed to deserialize transaction")?,
            );
        }
    }

    let header = Header {
        version,
        previous_block_hash,
        merkle_root,
        commitment_bytes: HexDebug(commitment_bytes),
        time,
        difficulty_threshold,
        nonce: HexDebug([0; 32]),
        solution: Solution::for_proposal(),
    };

    Ok(Block {
        header: Arc::new(header),
        transactions,
    })
}

/// Parse a hex string into a 32-byte array in serialized (internal) order.
/// The RPC returns hashes in display order (big-endian / reversed), so we
/// reverse the bytes to get the serialized order that zebra-chain stores.
fn hex_to_32_bytes(hex_str: &str) -> Result<[u8; 32]> {
    let mut bytes = <[u8; 32]>::from_hex(hex_str)
        .map_err(|e| anyhow::anyhow!("hex decode error: {e}"))?;
    bytes.reverse();
    Ok(bytes)
}
