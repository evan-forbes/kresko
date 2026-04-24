use anyhow::{Context, Result};
use hex::FromHex;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write as IoWrite;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use zebra_chain::{
    block::{self, Block, Header},
    fmt::HexDebug,
    parameters::{EquihashParams, Network, testnet},
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
    transaction_bytes: Option<usize>,
    template_prev_hash: Option<String>,
    template_longpollid: Option<String>,
    mempool_transactions: Option<usize>,
    mempool_bytes: Option<u64>,
    cancel_reason: Option<&'static str>,
    next_height: Option<u64>,
    next_transactions: Option<usize>,
    next_transaction_bytes: Option<usize>,
    next_prev_hash: Option<String>,
    error: Option<String>,
}

#[derive(Debug)]
struct TemplateSummary {
    height: u64,
    longpollid: Option<String>,
    previous_block_hash: Option<String>,
    transactions: usize,
    transaction_bytes: usize,
}

pub async fn run(rpc_endpoint: &str, zebrad_config: &Path) -> Result<()> {
    println!("Starting PoW miner against {rpc_endpoint}");

    // Verify connection
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let info = rpc_call(&client, rpc_endpoint, "getblockchaininfo", &[]).await?;
    let chain = info["result"]["chain"].as_str().unwrap_or("unknown");
    let height = info["result"]["blocks"].as_u64().unwrap_or(0);
    println!("Connected: chain={chain}, height={height}");
    let rpc_network = network_from_chain_name(chain)?;
    let network = match load_equihash_params(zebrad_config)? {
        Some(equihash_params) if is_test_chain_name(chain) => {
            configured_testnet_network(equihash_params)?
        }
        _ => rpc_network,
    };
    println!(
        "Mining with Equihash parameters: {:?}",
        network.equihash_params()
    );

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
        let template = loop {
            match get_block_template(&client, rpc_endpoint, longpollid.as_deref()).await {
                Ok(template) => break template,
                Err(err) => {
                    if longpollid.is_some() {
                        eprintln!(
                            "Failed to get block template with longpollid: {err}; clearing \
                             longpoll state and retrying immediately"
                        );
                        longpollid = None;
                        continue;
                    }

                    eprintln!("Failed to get block template: {err}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        };

        templates_received += 1;
        let template_summary = summarize_template(&template);
        let template_height = template_summary.height;
        let tx_count = template_summary.transactions;
        let tx_bytes = template_summary.transaction_bytes;
        longpollid = template_summary.longpollid.clone();

        let mempool_probe = get_mempool_info(&client, rpc_endpoint).await;
        let (mempool_transactions, mempool_bytes, mempool_error) = match mempool_probe {
            Ok((transactions, bytes)) => (Some(transactions), Some(bytes), None),
            Err(err) => (None, None, Some(err.to_string())),
        };

        println!(
            "Got template: height={template_height}, transactions={tx_count}, tx_bytes={tx_bytes}"
        );

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
                transaction_bytes: Some(tx_bytes),
                template_prev_hash: template_summary.previous_block_hash.clone(),
                template_longpollid: template_summary.longpollid.clone(),
                mempool_transactions,
                mempool_bytes,
                cancel_reason: None,
                next_height: None,
                next_transactions: None,
                next_transaction_bytes: None,
                next_prev_hash: None,
                error: mempool_error,
            },
        );

        // 2. Parse template into a Block
        let block = match block_from_template(&template, &network) {
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
            let next_template =
                get_block_template(&poll_client, &poll_endpoint, poll_lpid.as_deref()).await;
            let _ = cancel_tx.send(true);
            next_template
                .ok()
                .map(|template| summarize_template(&template))
        });

        // 4. Solve in a blocking thread
        let solve_start = Instant::now();
        let solve_network = network.clone();
        let solve_result = tokio::task::spawn_blocking(move || {
            let cancel_fn = move || {
                if *cancel_rx.borrow() {
                    Err(SolverCancelled)
                } else {
                    Ok(())
                }
            };
            Solution::solve(header, &solve_network, cancel_fn)
        })
        .await
        .context("solver thread panicked")?;

        let solve_time = solve_start.elapsed();

        match solve_result {
            Ok(solved_headers) => {
                poll_handle.abort();
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
                            println!(
                                "Block submitted at height {template_height}: hash={block_hash}"
                            );
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
                                transaction_bytes: Some(tx_bytes),
                                template_prev_hash: template_summary.previous_block_hash.clone(),
                                template_longpollid: template_summary.longpollid.clone(),
                                mempool_transactions,
                                mempool_bytes,
                                cancel_reason: None,
                                next_height: None,
                                next_transactions: None,
                                next_transaction_bytes: None,
                                next_prev_hash: None,
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
                                transaction_bytes: Some(tx_bytes),
                                template_prev_hash: template_summary.previous_block_hash.clone(),
                                template_longpollid: template_summary.longpollid.clone(),
                                mempool_transactions,
                                mempool_bytes,
                                cancel_reason: None,
                                next_height: None,
                                next_transactions: None,
                                next_transaction_bytes: None,
                                next_prev_hash: None,
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
                let next_template = match poll_handle.await {
                    Ok(summary) => summary,
                    Err(err) => {
                        eprintln!("Long-poll task failed after cancellation: {err}");
                        None
                    }
                };
                let cancel_reason = next_template
                    .as_ref()
                    .map(|summary| classify_cancellation(&template_summary, summary));
                stale_cancellations += 1;
                println!(
                    "Template changed after {:.1}s, restarting solver... reason={}",
                    solve_time.as_secs_f64(),
                    cancel_reason.unwrap_or("unknown")
                );

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
                        transaction_bytes: Some(tx_bytes),
                        template_prev_hash: template_summary.previous_block_hash.clone(),
                        template_longpollid: template_summary.longpollid.clone(),
                        mempool_transactions,
                        mempool_bytes,
                        cancel_reason,
                        next_height: next_template.as_ref().map(|summary| summary.height),
                        next_transactions: next_template
                            .as_ref()
                            .map(|summary| summary.transactions),
                        next_transaction_bytes: next_template
                            .as_ref()
                            .map(|summary| summary.transaction_bytes),
                        next_prev_hash: next_template
                            .as_ref()
                            .and_then(|summary| summary.previous_block_hash.clone()),
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

fn summarize_template(template: &serde_json::Value) -> TemplateSummary {
    let transactions = template["transactions"]
        .as_array()
        .map(|entries| entries.len())
        .unwrap_or(0);
    let transaction_bytes = template["transactions"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry["data"].as_str())
                .map(|hex| hex.len() / 2)
                .sum()
        })
        .unwrap_or(0);

    TemplateSummary {
        height: template["height"].as_u64().unwrap_or(0),
        longpollid: template["longpollid"].as_str().map(String::from),
        previous_block_hash: template["previousblockhash"].as_str().map(String::from),
        transactions,
        transaction_bytes,
    }
}

fn classify_cancellation(current: &TemplateSummary, next: &TemplateSummary) -> &'static str {
    if next.height != current.height || next.previous_block_hash != current.previous_block_hash {
        "tip_changed"
    } else if next.transactions != current.transactions
        || next.transaction_bytes != current.transaction_bytes
    {
        "mempool_changed"
    } else {
        "template_changed"
    }
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

async fn get_mempool_info(client: &reqwest::Client, endpoint: &str) -> Result<(usize, u64)> {
    let payload = rpc_call(client, endpoint, "getmempoolinfo", &[]).await?;
    let result = payload
        .get("result")
        .context("missing result in getmempoolinfo response")?;
    let transactions = result
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .context("missing getmempoolinfo.result.size")? as usize;
    let bytes = result
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .context("missing getmempoolinfo.result.bytes")?;
    Ok((transactions, bytes))
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
fn block_from_template(template: &serde_json::Value, network: &Network) -> Result<Block> {
    let version = template["version"].as_u64().context("missing version")? as u32;

    let prev_hash_hex = template["previousblockhash"]
        .as_str()
        .context("missing previousblockhash")?;
    let previous_block_hash =
        block::Hash::from_hex(prev_hash_hex).context("invalid previousblockhash hex")?;

    let default_roots = &template["defaultroots"];

    let merkle_root_hex = default_roots["merkleroot"]
        .as_str()
        .context("missing defaultroots.merkleroot")?;
    let merkle_root_bytes = hex_to_32_bytes(merkle_root_hex).context("invalid merkleroot hex")?;
    let merkle_root = block::merkle::Root(merkle_root_bytes);

    // All kresko experiments activate NU6.1 at height 1, so we always use
    // the NU5+ commitment path (blockcommitmentshash).
    let commitment_hex = default_roots["blockcommitmentshash"]
        .as_str()
        .context("missing defaultroots.blockcommitmentshash")?;
    let commitment_bytes =
        hex_to_32_bytes(commitment_hex).context("invalid blockcommitmentshash hex")?;

    let bits_hex = template["bits"].as_str().context("missing bits")?;
    let difficulty_threshold = CompactDifficulty::from_hex(bits_hex)
        .map_err(|e| anyhow::anyhow!("invalid bits hex: {e}"))?;

    let cur_time = template["curtime"].as_u64().context("missing curtime")? as i64;
    let time =
        chrono::DateTime::from_timestamp(cur_time, 0).context("invalid curtime timestamp")?;

    // Parse transactions
    let coinbase_hex = template["coinbasetxn"]["data"]
        .as_str()
        .context("missing coinbasetxn.data")?;
    let coinbase_bytes = hex::decode(coinbase_hex).context("invalid coinbase hex")?;
    let mut transactions: Vec<Arc<zebra_chain::transaction::Transaction>> = vec![
        coinbase_bytes
            .zcash_deserialize_into()
            .context("failed to deserialize coinbase transaction")?,
    ];

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
        solution: Solution::for_proposal(network),
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
    let mut bytes =
        <[u8; 32]>::from_hex(hex_str).map_err(|e| anyhow::anyhow!("hex decode error: {e}"))?;
    bytes.reverse();
    Ok(bytes)
}

fn network_from_chain_name(chain: &str) -> Result<Network> {
    match chain.to_ascii_lowercase().as_str() {
        "main" | "mainnet" => Ok(Network::Mainnet),
        "test" | "testnet" => Ok(Network::new_default_testnet()),
        other => anyhow::bail!(
            "unsupported chain reported by getblockchaininfo: '{other}'. Expected main/test network."
        ),
    }
}

fn is_test_chain_name(chain: &str) -> bool {
    matches!(chain.to_ascii_lowercase().as_str(), "test" | "testnet")
}

fn configured_testnet_network(equihash_params: EquihashParams) -> Result<Network> {
    testnet::Parameters::build()
        .with_equihash_params(equihash_params)
        .to_network()
        .map_err(|err| anyhow::anyhow!("failed to build mining network parameters: {err}"))
}

fn load_equihash_params(config_path: &Path) -> Result<Option<EquihashParams>> {
    if !config_path.exists() {
        eprintln!(
            "zebrad config {} not found; falling back to RPC chain defaults",
            config_path.display()
        );
        return Ok(None);
    }

    let config = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read zebrad config {}", config_path.display()))?;
    let parsed: toml::Value = toml::from_str(&config)
        .with_context(|| format!("failed to parse zebrad config {}", config_path.display()))?;
    let Some(value) = parsed
        .get("network")
        .and_then(|network| network.get("testnet_parameters"))
        .and_then(|params| params.get("equihash_params"))
        .and_then(toml::Value::as_str)
    else {
        return Ok(None);
    };

    let equihash_params = match value {
        "common" => EquihashParams::Common,
        "regtest" => EquihashParams::Regtest,
        other => anyhow::bail!(
            "unsupported network.testnet_parameters.equihash_params in {}: {other}",
            config_path.display(),
        ),
    };
    println!(
        "Loaded Equihash parameters {:?} from {}",
        equihash_params,
        config_path.display()
    );
    Ok(Some(equihash_params))
}
