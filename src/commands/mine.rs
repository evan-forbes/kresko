use anyhow::{Context, Result};
use hex::FromHex;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use zebra_chain::{
    block::{self, Header},
    fmt::HexDebug,
    parameters::Network,
    serialization::{CompactSizeMessage, ZcashSerialize},
    work::{
        difficulty::CompactDifficulty,
        equihash::{Solution, SolverAction, SolverCancelled},
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

#[derive(Clone, Debug)]
struct TemplateSummary {
    height: u64,
    longpollid: Option<String>,
    submit_old: Option<bool>,
    previous_block_hash: Option<String>,
    transactions: usize,
    transaction_bytes: usize,
}

#[derive(Debug)]
struct TemplateUpdate {
    template: serde_json::Value,
    summary: TemplateSummary,
    is_provisional: bool,
    reason: &'static str,
}

struct PendingTemplate {
    template: serde_json::Value,
    is_provisional: bool,
}

#[derive(Clone, Copy)]
struct TemplateRefreshPolicy {
    interval: Option<Duration>,
    mine_provisional_empty_templates: bool,
}

struct RawTemplateBlock {
    header: Header,
    transactions: Vec<Vec<u8>>,
}

impl RawTemplateBlock {
    fn serialize_with_header(&self, header: &Header) -> Result<Vec<u8>> {
        let mut block_bytes = Vec::new();
        header
            .zcash_serialize(&mut block_bytes)
            .context("failed to serialize solved header")?;
        CompactSizeMessage::try_from(self.transactions.len())
            .context("too many transactions in block template")?
            .zcash_serialize(&mut block_bytes)
            .context("failed to serialize transaction count")?;
        for tx in &self.transactions {
            block_bytes.extend_from_slice(tx);
        }
        Ok(block_bytes)
    }
}

/// A source of Equihash solutions for a header template.
///
/// Tests substitute a solver that has the same cancellation boundaries but
/// does no Equihash work.
pub trait Solver: Send + Sync {
    /// Solves `header`, or returns `Err(SolverCancelled)` when `action` stops it.
    fn solve(
        &self,
        header: Header,
        action: tokio::sync::watch::Receiver<SolverAction>,
    ) -> Result<Header, SolverCancelled>;
}

/// The production solver: Equihash 200,9 via tromp.
pub struct EquihashSolver;

impl Solver for EquihashSolver {
    fn solve(
        &self,
        header: Header,
        action: tokio::sync::watch::Receiver<SolverAction>,
    ) -> Result<Header, SolverCancelled> {
        Solution::solve_cancellable(header, move || *action.borrow()).map(|solved| {
            solved
                .into_iter()
                .next()
                .expect("solve returns at least one solution")
        })
    }
}

/// The parts of the mining loop that a test replaces.
pub struct MinerOptions {
    /// Where solutions come from.
    pub solver: Arc<dyn Solver>,

    /// Path of the structured solve log.
    pub log_path: PathBuf,

    /// Submit a solution even after the node has committed a block at the same
    /// height.
    ///
    /// A solver pass cannot be interrupted, so a solution can arrive seconds
    /// after the tip already moved past it. Submitting it publishes a sibling
    /// of a block this node has already accepted, which forks the chain and
    /// wins nothing. Real miners drop these, so the default is `false`. Set it
    /// to `true` to reproduce the orphan rate of runs before this check
    /// existed.
    pub submit_stale_solutions: bool,

    /// Minimum time to keep a still-valid template before accepting an update.
    /// `None` keeps the template until its work becomes invalid or it wins.
    pub template_refresh_interval: Option<Duration>,

    /// Mine the provisional empty template returned immediately after a tip
    /// change instead of waiting for a fully assembled template.
    pub mine_provisional_empty_templates: bool,

    /// Stop after this many solver runs. `None` mines until the process exits.
    pub max_runs: Option<u64>,
}

impl Default for MinerOptions {
    fn default() -> Self {
        Self {
            solver: Arc::new(EquihashSolver),
            log_path: PathBuf::from("mine.log.jsonl"),
            submit_stale_solutions: false,
            template_refresh_interval: Some(Duration::from_secs(60)),
            mine_provisional_empty_templates: false,
            max_runs: None,
        }
    }
}

pub async fn run_with(
    rpc_endpoint: &str,
    zebrad_config: &Path,
    options: MinerOptions,
) -> Result<()> {
    println!("Starting PoW miner against {rpc_endpoint}");

    // Verify connection
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let info = rpc_call(&client, rpc_endpoint, "getblockchaininfo", &[]).await?;
    let chain = info["result"]["chain"].as_str().unwrap_or("unknown");
    let height = info["result"]["blocks"].as_u64().unwrap_or(0);
    println!("Connected: chain={chain}, height={height}");
    let _network = network_from_chain_name(chain)?;
    if !zebrad_config.exists() {
        eprintln!(
            "zebrad config {} not found; using RPC chain network",
            zebrad_config.display()
        );
    }

    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&options.log_path)?;
    println!(
        "Logging structured metrics to {}",
        options.log_path.display()
    );

    let mut longpollid: Option<String> = None;
    let mut pending_template: Option<PendingTemplate> = None;
    let mut templates_received: u64 = 0;
    let mut solutions_found: u64 = 0;
    let mut blocks_submitted: u64 = 0;
    let mut blocks_rejected: u64 = 0;
    let mut stale_cancellations: u64 = 0;
    let mut stale_solutions: u64 = 0;
    let mut runs: u64 = 0;

    'mine: loop {
        if options.max_runs.is_some_and(|max| runs >= max) {
            return Ok(());
        }
        runs += 1;

        // 1. Get block template
        let (template, current_is_provisional) = if let Some(pending) = pending_template.take() {
            (pending.template, pending.is_provisional)
        } else {
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
            (template, false)
        };

        templates_received += 1;
        let template_summary = summarize_template(&template);
        let template_height = template_summary.height;
        let tx_count = template_summary.transactions;
        let tx_bytes = template_summary.transaction_bytes;
        longpollid = template_summary.longpollid.clone();

        let mempool_client = client.clone();
        let mempool_endpoint = rpc_endpoint.to_string();
        let mempool_probe =
            tokio::spawn(async move { get_mempool_info(&mempool_client, &mempool_endpoint).await });

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
                mempool_transactions: None,
                mempool_bytes: None,
                cancel_reason: None,
                next_height: None,
                next_transactions: None,
                next_transaction_bytes: None,
                next_prev_hash: None,
                error: None,
            },
        );

        // 2. Parse template into a raw block. Template transactions can use
        // transaction versions newer than Kresko needs to understand.
        let block = match block_from_template(&template) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Failed to parse block template: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let header = block.header;

        // 3. Keep long-polling until work becomes invalid or the freshness
        // policy requests a nonce-boundary refresh.
        let (action_tx, action_rx) = tokio::sync::watch::channel(SolverAction::Continue);
        let mut submission_action_rx = action_tx.subscribe();
        let poll_client = client.clone();
        let poll_endpoint = rpc_endpoint.to_string();
        let poll_summary = template_summary.clone();
        let refresh_policy = TemplateRefreshPolicy {
            interval: options.template_refresh_interval,
            mine_provisional_empty_templates: options.mine_provisional_empty_templates,
        };
        let template_update = Arc::new(Mutex::new(None));
        let mut poll_handle = Some(tokio::spawn(supervise_template_updates(
            poll_client,
            poll_endpoint,
            poll_summary,
            action_tx,
            refresh_policy,
            current_is_provisional,
            template_update.clone(),
        )));

        // 4. Solve in a blocking thread
        let solve_start = Instant::now();
        let solver = options.solver.clone();
        let solve_result = tokio::task::spawn_blocking(move || solver.solve(header, action_rx))
            .await
            .context("solver thread panicked")?;

        let solve_time = solve_start.elapsed();
        let (mempool_transactions, mempool_bytes) = if mempool_probe.is_finished() {
            match mempool_probe.await {
                Ok(Ok((transactions, bytes))) => (Some(transactions), Some(bytes)),
                Ok(Err(error)) => {
                    eprintln!("Mempool metrics request failed: {error}");
                    (None, None)
                }
                Err(error) => {
                    eprintln!("Mempool metrics task failed: {error}");
                    (None, None)
                }
            }
        } else {
            mempool_probe.abort();
            (None, None)
        };

        match solve_result {
            Ok(solved_header) => {
                solutions_found += 1;
                println!(
                    "Solution found in {:.1}s for height {template_height}",
                    solve_time.as_secs_f64()
                );

                let block_hash = format!("{}", block::Hash::from(&solved_header));
                let block_bytes = block
                    .serialize_with_header(&solved_header)
                    .context("failed to serialize solved block")?;
                let block_hex = hex::encode(&block_bytes);

                // Check immediately before submission. A tip change can race
                // with the solver's final digit-boundary callback.
                if *submission_action_rx.borrow_and_update() == SolverAction::StopNow
                    && !options.submit_stale_solutions
                {
                    let update = wait_for_template_update(
                        &mut poll_handle,
                        &template_update,
                        SolverAction::StopNow,
                    )
                    .await;
                    let next_summary = update.as_ref().map(|update| &update.summary);
                    let cancel_reason = update
                        .as_ref()
                        .map(|update| update.reason)
                        .or(Some("work_invalidated"));
                    stale_solutions += 1;
                    println!(
                        "Discarding solution for height {template_height}: its work became invalid \
                         before submission (solve_time={:.1}s)",
                        solve_time.as_secs_f64()
                    );

                    log_entry(
                        &mut log_file,
                        &MineLogEntry {
                            ts_unix_ms: now_unix_ms(),
                            height: template_height,
                            event: "solution_discarded_stale",
                            solve_time_ms: Some(solve_time.as_millis()),
                            block_hash: Some(block_hash),
                            submit_result: None,
                            transactions: Some(tx_count),
                            transaction_bytes: Some(tx_bytes),
                            template_prev_hash: template_summary.previous_block_hash.clone(),
                            template_longpollid: template_summary.longpollid.clone(),
                            mempool_transactions,
                            mempool_bytes,
                            cancel_reason,
                            next_height: next_summary.map(|summary| summary.height),
                            next_transactions: next_summary.map(|summary| summary.transactions),
                            next_transaction_bytes: next_summary
                                .map(|summary| summary.transaction_bytes),
                            next_prev_hash: next_summary
                                .and_then(|summary| summary.previous_block_hash.clone()),
                            error: None,
                        },
                    );

                    if let Some(update) = update {
                        pending_template = Some(PendingTemplate {
                            template: update.template,
                            is_provisional: update.is_provisional,
                        });
                    }
                    continue 'mine;
                }

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
                                .is_some_and(|s| s.starts_with("duplicate"));

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

                abort_template_supervisor(&mut poll_handle);

                println!(
                    "  stats: templates={templates_received} solutions={solutions_found} \
                     submitted={blocks_submitted} rejected={blocks_rejected} \
                     stale={stale_cancellations} discarded={stale_solutions}"
                );
            }
            Err(SolverCancelled) => {
                let action = *submission_action_rx.borrow_and_update();
                let update =
                    wait_for_template_update(&mut poll_handle, &template_update, action).await;
                let next_summary = update.as_ref().map(|update| &update.summary);
                let cancel_reason = update.as_ref().map(|update| update.reason);
                stale_cancellations += 1;
                println!(
                    "Template changed after {:.1}s, restarting solver... action={:?} reason={}",
                    solve_time.as_secs_f64(),
                    action,
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
                        next_height: next_summary.map(|summary| summary.height),
                        next_transactions: next_summary.map(|summary| summary.transactions),
                        next_transaction_bytes: next_summary
                            .map(|summary| summary.transaction_bytes),
                        next_prev_hash: next_summary
                            .and_then(|summary| summary.previous_block_hash.clone()),
                        error: None,
                    },
                );

                if let Some(update) = update {
                    pending_template = Some(PendingTemplate {
                        template: update.template,
                        is_provisional: update.is_provisional,
                    });
                }
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

async fn wait_for_template_update(
    handle: &mut Option<tokio::task::JoinHandle<()>>,
    update: &Arc<Mutex<Option<TemplateUpdate>>>,
    action: SolverAction,
) -> Option<TemplateUpdate> {
    if let Some(handle) = handle.take() {
        if action == SolverAction::StopNow {
            if let Err(error) = handle.await {
                eprintln!("Template supervisor failed: {error}");
            }
        } else {
            handle.abort();
        }
    }

    update
        .lock()
        .expect("template update mutex is not poisoned")
        .take()
}

fn abort_template_supervisor(handle: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = handle.take() {
        handle.abort();
    }
}

async fn supervise_template_updates(
    client: reqwest::Client,
    endpoint: String,
    current: TemplateSummary,
    action_tx: tokio::sync::watch::Sender<SolverAction>,
    refresh_policy: TemplateRefreshPolicy,
    mut current_is_provisional: bool,
    update: Arc<Mutex<Option<TemplateUpdate>>>,
) {
    let started = Instant::now();
    let mut longpollid = current.longpollid.clone();
    let mut replacement_reason = None;

    loop {
        let template = match get_block_template(&client, &endpoint, longpollid.as_deref()).await {
            Ok(template) => template,
            Err(error) => {
                eprintln!("Template supervisor request failed: {error}; keeping current work");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let summary = summarize_template(&template);

        match summary.submit_old {
            Some(true) => {
                let replaces_provisional = current_is_provisional && summary.transactions > 0;
                let refresh_due = refresh_policy
                    .interval
                    .is_some_and(|interval| started.elapsed() >= interval);
                let cannot_rearm = summary.longpollid.is_none();

                if cannot_rearm {
                    action_tx.send_replace(SolverAction::StopNow);
                    store_template_update(
                        &update,
                        TemplateUpdate {
                            template,
                            summary,
                            is_provisional: false,
                            reason: "template_monitor_unavailable",
                        },
                    );
                    return;
                }

                longpollid = summary.longpollid.clone();

                if replacement_reason.is_none() && (replaces_provisional || refresh_due) {
                    replacement_reason = Some(if replaces_provisional {
                        "provisional_replaced"
                    } else {
                        "template_refresh"
                    });
                }

                if let Some(reason) = replacement_reason {
                    store_template_update(
                        &update,
                        TemplateUpdate {
                            template,
                            summary,
                            is_provisional: false,
                            reason,
                        },
                    );
                    action_tx.send_replace(SolverAction::StopAtNonceBoundary);
                    current_is_provisional = false;
                }
            }
            Some(false) | None => {
                action_tx.send_replace(SolverAction::StopNow);
                let reason = classify_cancellation(&current, &summary);

                let update_value = if refresh_policy.mine_provisional_empty_templates {
                    let is_provisional =
                        !same_work(&current, &summary) && summary.transactions == 0;
                    TemplateUpdate {
                        template,
                        summary,
                        is_provisional,
                        reason,
                    }
                } else {
                    loop {
                        match get_block_template(&client, &endpoint, None).await {
                            Ok(template) => {
                                let summary = summarize_template(&template);
                                break TemplateUpdate {
                                    template,
                                    summary,
                                    is_provisional: false,
                                    reason,
                                };
                            }
                            Err(error) => {
                                eprintln!(
                                    "Failed to fetch a full template after invalidation: {error}"
                                );
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }
                    }
                };

                store_template_update(&update, update_value);
                return;
            }
        }
    }
}

fn store_template_update(update: &Arc<Mutex<Option<TemplateUpdate>>>, value: TemplateUpdate) {
    *update
        .lock()
        .expect("template update mutex is not poisoned") = Some(value);
}

fn same_work(current: &TemplateSummary, next: &TemplateSummary) -> bool {
    current.height == next.height && current.previous_block_hash == next.previous_block_hash
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
        submit_old: template["submitold"].as_bool(),
        previous_block_hash: template["previousblockhash"].as_str().map(String::from),
        transactions,
        transaction_bytes,
    }
}

fn classify_cancellation(current: &TemplateSummary, next: &TemplateSummary) -> &'static str {
    if !same_work(current, next) {
        "tip_changed"
    } else if next.submit_old != Some(true) {
        "work_invalidated"
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

/// Construct a raw block from a `getblocktemplate` JSON response.
///
/// Kresko only needs to solve the header. Transactions are kept as raw bytes so
/// the miner can submit templates containing transaction versions that are
/// newer than the local `zebra-chain` transaction parser supports.
fn block_from_template(template: &serde_json::Value) -> Result<RawTemplateBlock> {
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

    let coinbase_hex = template["coinbasetxn"]["data"]
        .as_str()
        .context("missing coinbasetxn.data")?;
    let coinbase_bytes = hex::decode(coinbase_hex).context("invalid coinbase hex")?;
    let mut transactions = vec![coinbase_bytes];

    if let Some(tx_templates) = template["transactions"].as_array() {
        for tx_template in tx_templates {
            let tx_hex = tx_template["data"]
                .as_str()
                .context("missing transaction data")?;
            let tx_bytes = hex::decode(tx_hex).context("invalid transaction hex")?;
            transactions.push(tx_bytes);
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

    Ok(RawTemplateBlock {
        header,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::watch;

    /// The tip the miner is handed a template for.
    const TIP0: &str = "00000000000000000000000000000000000000000000000000000000000000aa";
    /// The tip after a competing block lands at the height being mined.
    const TIP1: &str = "00000000000000000000000000000000000000000000000000000000000000bb";
    const ZERO32: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    /// How long one simulated solver pass takes.
    const PASS: Duration = Duration::from_millis(400);
    /// When the competing block lands, measured from the start of the run.
    const TIP_CHANGE_AFTER: Duration = Duration::from_millis(120);

    #[derive(Clone)]
    struct Template {
        height: u64,
        prev_hash: String,
        longpollid: String,
        transactions: usize,
        submit_old: Option<bool>,
    }

    impl Template {
        fn to_json(&self, include_submit_old: bool) -> serde_json::Value {
            let transactions: Vec<_> = (0..self.transactions)
                .map(|i| serde_json::json!({ "data": format!("{:02x}", i as u8).repeat(8) }))
                .collect();

            let mut template = serde_json::json!({
                "version": 4,
                "height": self.height,
                "previousblockhash": self.prev_hash,
                "longpollid": self.longpollid,
                "defaultroots": { "merkleroot": ZERO32, "blockcommitmentshash": ZERO32 },
                "bits": "1f07ffff",
                "curtime": 1_700_000_000u64,
                "coinbasetxn": { "data": "00".repeat(16) },
                "transactions": transactions,
            });
            if include_submit_old && let Some(submit_old) = self.submit_old {
                template["submitold"] = serde_json::Value::Bool(submit_old);
            }
            template
        }
    }

    /// A `zakurad` stand-in that serves templates, holds long polls open until
    /// the template changes, and records every submitted block.
    struct MockNode {
        state: watch::Sender<Template>,
        submitted: Mutex<Vec<String>>,
    }

    impl MockNode {
        fn new(template: Template) -> Arc<Self> {
            let (state, _) = watch::channel(template);
            Arc::new(Self {
                state,
                submitted: Mutex::new(Vec::new()),
            })
        }

        async fn handle(&self, request: &serde_json::Value) -> serde_json::Value {
            match request["method"].as_str().unwrap_or_default() {
                "getblockchaininfo" => {
                    let blocks = self.state.borrow().height.saturating_sub(1);
                    serde_json::json!({ "chain": "test", "blocks": blocks })
                }

                "getmempoolinfo" => {
                    let size = self.state.borrow().transactions;
                    serde_json::json!({ "size": size, "bytes": size * 200 })
                }

                "submitblock" => {
                    let block_hex = request["params"][0]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    self.submitted.lock().expect("not poisoned").push(block_hex);
                    serde_json::Value::Null
                }

                "getblocktemplate" => {
                    let client_lpid = request["params"][0]["longpollid"]
                        .as_str()
                        .map(str::to_string);
                    let mut state = self.state.subscribe();

                    loop {
                        let current = state.borrow_and_update().clone();
                        if client_lpid.as_deref() != Some(current.longpollid.as_str()) {
                            return current.to_json(client_lpid.is_some());
                        }
                        if state.changed().await.is_err() {
                            return current.to_json(client_lpid.is_some());
                        }
                    }
                }

                _ => serde_json::Value::Null,
            }
        }
    }

    fn find_header_end(buffer: &[u8]) -> Option<usize> {
        buffer.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn content_length(head: &[u8]) -> usize {
        String::from_utf8_lossy(head)
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())?
            })
            .unwrap_or(0)
    }

    /// Serves `node` on an ephemeral port and returns its RPC endpoint.
    async fn serve(node: Arc<MockNode>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let node = node.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];

                    let body = loop {
                        let Ok(read) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);

                        let Some(head_end) = find_header_end(&buffer) else {
                            continue;
                        };
                        let start = head_end + 4;
                        let length = content_length(&buffer[..head_end]);
                        if buffer.len() >= start + length {
                            break buffer[start..start + length].to_vec();
                        }
                    };

                    let request: serde_json::Value =
                        serde_json::from_slice(&body).expect("valid JSON-RPC body");
                    let result = node.handle(&request).await;
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request["id"].clone(),
                        "result": result,
                    });

                    let payload = serde_json::to_vec(&response).expect("serializable response");
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        payload.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&payload).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        endpoint
    }

    /// Stands in for Equihash with digit-boundary and nonce-boundary checks.
    struct SimulatedSolver {
        pass: Duration,
        passes_to_solution: usize,
        honor_actions: bool,
    }

    impl Solver for SimulatedSolver {
        fn solve(
            &self,
            mut header: Header,
            action: watch::Receiver<SolverAction>,
        ) -> Result<Header, SolverCancelled> {
            let mut passes = 0;

            loop {
                if self.honor_actions && *action.borrow() != SolverAction::Continue {
                    return Err(SolverCancelled);
                }

                for _digit in 0..10 {
                    std::thread::sleep(self.pass / 10);
                    if self.honor_actions && *action.borrow() == SolverAction::StopNow {
                        return Err(SolverCancelled);
                    }
                }
                passes += 1;

                if passes >= self.passes_to_solution {
                    header.nonce = HexDebug([1; 32]);
                    return Ok(header);
                }
            }
        }
    }

    /// The parent hash a submitted block builds on, in display order.
    fn parent_of(block_hex: &str) -> String {
        let bytes = hex::decode(block_hex).expect("valid block hex");
        let mut parent = bytes[4..36].to_vec();
        parent.reverse();
        hex::encode(parent)
    }

    fn options(name: &str, submit_stale_solutions: bool) -> MinerOptions {
        MinerOptions {
            solver: Arc::new(SimulatedSolver {
                pass: PASS,
                passes_to_solution: 1,
                honor_actions: false,
            }),
            log_path: std::env::temp_dir().join(format!("kresko-{name}.jsonl")),
            submit_stale_solutions,
            template_refresh_interval: None,
            mine_provisional_empty_templates: false,
            max_runs: Some(1),
        }
    }

    /// Mines one template, replaces it mid-pass with `next`, and returns every
    /// block the node was asked to accept.
    async fn mine_through_template_change(
        name: &str,
        next: Template,
        submit_stale_solutions: bool,
    ) -> Vec<String> {
        let node = MockNode::new(Template {
            height: 101,
            prev_hash: TIP0.to_string(),
            longpollid: "A".to_string(),
            transactions: 0,
            submit_old: Some(true),
        });
        let endpoint = serve(node.clone()).await;

        let miner = tokio::spawn({
            let options = options(name, submit_stale_solutions);
            async move {
                run_with(&endpoint, Path::new("/nonexistent/zebrad.toml"), options)
                    .await
                    .expect("miner ran")
            }
        });

        tokio::time::sleep(TIP_CHANGE_AFTER).await;
        node.state.send_replace(next);

        tokio::time::timeout(Duration::from_secs(20), miner)
            .await
            .expect("miner finished")
            .expect("miner did not panic");

        node.submitted.lock().expect("not poisoned").clone()
    }

    fn template(height: u64, prev_hash: &str, longpollid: &str) -> Template {
        Template {
            height,
            prev_hash: prev_hash.to_string(),
            longpollid: longpollid.to_string(),
            transactions: 0,
            submit_old: Some(true),
        }
    }

    fn template_update_slot() -> Arc<Mutex<Option<TemplateUpdate>>> {
        Arc::new(Mutex::new(None))
    }

    async fn finish_supervisor(
        supervisor: tokio::task::JoinHandle<()>,
        update: Arc<Mutex<Option<TemplateUpdate>>>,
        action: SolverAction,
    ) -> TemplateUpdate {
        let mut supervisor = Some(supervisor);
        wait_for_template_update(&mut supervisor, &update, action)
            .await
            .expect("supervisor stored a template update")
    }

    async fn wait_for_action(
        action_rx: &mut watch::Receiver<SolverAction>,
        expected: SolverAction,
    ) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while *action_rx.borrow_and_update() != expected {
                action_rx
                    .changed()
                    .await
                    .expect("action sender remains open");
            }
        })
        .await
        .expect("supervisor published the expected action");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn supervisor_rearms_after_valid_mempool_work() {
        let initial = template(101, TIP0, "A");
        let node = MockNode::new(initial.clone());
        let endpoint = serve(node.clone()).await;
        let (action_tx, action_rx) = watch::channel(SolverAction::Continue);
        let update = template_update_slot();
        let supervisor = tokio::spawn(supervise_template_updates(
            reqwest::Client::new(),
            endpoint,
            summarize_template(&initial.to_json(false)),
            action_tx,
            TemplateRefreshPolicy {
                interval: None,
                mine_provisional_empty_templates: false,
            },
            false,
            update.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(20)).await;
        node.state.send_replace(Template {
            transactions: 3,
            longpollid: "B".to_string(),
            submit_old: Some(true),
            ..initial.clone()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*action_rx.borrow(), SolverAction::Continue);

        node.state.send_replace(Template {
            height: 102,
            prev_hash: TIP1.to_string(),
            longpollid: "C".to_string(),
            transactions: 0,
            submit_old: Some(false),
        });
        let update = finish_supervisor(supervisor, update, SolverAction::StopNow).await;

        assert_eq!(update.reason, "tip_changed");
        assert_eq!(update.summary.height, 102);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn missing_submitold_stops_work_conservatively() {
        let initial = template(101, TIP0, "A");
        let node = MockNode::new(initial.clone());
        let endpoint = serve(node.clone()).await;
        let (action_tx, action_rx) = watch::channel(SolverAction::Continue);
        let update = template_update_slot();
        let supervisor = tokio::spawn(supervise_template_updates(
            reqwest::Client::new(),
            endpoint,
            summarize_template(&initial.to_json(false)),
            action_tx,
            TemplateRefreshPolicy {
                interval: None,
                mine_provisional_empty_templates: false,
            },
            false,
            update.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(20)).await;
        node.state.send_replace(Template {
            longpollid: "B".to_string(),
            submit_old: None,
            ..initial
        });
        let update = finish_supervisor(supervisor, update, SolverAction::StopNow).await;

        assert_eq!(*action_rx.borrow(), SolverAction::StopNow);
        assert_eq!(update.reason, "work_invalidated");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn submitold_false_stops_work_with_the_same_parent() {
        let initial = template(101, TIP0, "A");
        let node = MockNode::new(initial.clone());
        let endpoint = serve(node.clone()).await;
        let (action_tx, action_rx) = watch::channel(SolverAction::Continue);
        let update = template_update_slot();
        let supervisor = tokio::spawn(supervise_template_updates(
            reqwest::Client::new(),
            endpoint,
            summarize_template(&initial.to_json(false)),
            action_tx,
            TemplateRefreshPolicy {
                interval: None,
                mine_provisional_empty_templates: true,
            },
            false,
            update.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(20)).await;
        node.state.send_replace(Template {
            longpollid: "B".to_string(),
            submit_old: Some(false),
            ..initial
        });
        let update = finish_supervisor(supervisor, update, SolverAction::StopNow).await;

        assert_eq!(*action_rx.borrow(), SolverAction::StopNow);
        assert_eq!(update.reason, "work_invalidated");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn provisional_policy_returns_the_tip_change_template_immediately() {
        let initial = template(101, TIP0, "A");
        let node = MockNode::new(initial.clone());
        let endpoint = serve(node.clone()).await;
        let (action_tx, _action_rx) = watch::channel(SolverAction::Continue);
        let update = template_update_slot();
        let supervisor = tokio::spawn(supervise_template_updates(
            reqwest::Client::new(),
            endpoint,
            summarize_template(&initial.to_json(false)),
            action_tx,
            TemplateRefreshPolicy {
                interval: None,
                mine_provisional_empty_templates: true,
            },
            false,
            update.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(20)).await;
        node.state.send_replace(Template {
            height: 102,
            prev_hash: TIP1.to_string(),
            longpollid: "B".to_string(),
            transactions: 0,
            submit_old: Some(false),
        });
        let update = finish_supervisor(supervisor, update, SolverAction::StopNow).await;

        assert_eq!(update.summary.height, 102);
        assert_eq!(update.summary.transactions, 0);
        assert!(update.is_provisional);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rpc_errors_do_not_cancel_valid_work() {
        let initial = template(101, TIP0, "A");
        let (action_tx, action_rx) = watch::channel(SolverAction::Continue);
        let update = template_update_slot();
        let supervisor = tokio::spawn(supervise_template_updates(
            reqwest::Client::new(),
            "http://127.0.0.1:1".to_string(),
            summarize_template(&initial.to_json(false)),
            action_tx,
            TemplateRefreshPolicy {
                interval: None,
                mine_provisional_empty_templates: false,
            },
            false,
            update,
        ));

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*action_rx.borrow(), SolverAction::Continue);
        supervisor.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn freshness_interval_refreshes_on_the_next_valid_update() {
        let initial = template(101, TIP0, "A");
        let node = MockNode::new(initial.clone());
        let endpoint = serve(node.clone()).await;
        let (action_tx, mut action_rx) = watch::channel(SolverAction::Continue);
        let update = template_update_slot();
        let supervisor = tokio::spawn(supervise_template_updates(
            reqwest::Client::new(),
            endpoint,
            summarize_template(&initial.to_json(false)),
            action_tx,
            TemplateRefreshPolicy {
                interval: Some(Duration::from_millis(50)),
                mine_provisional_empty_templates: false,
            },
            false,
            update.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(70)).await;
        node.state.send_replace(Template {
            transactions: 3,
            longpollid: "B".to_string(),
            submit_old: Some(true),
            ..initial
        });
        wait_for_action(&mut action_rx, SolverAction::StopAtNonceBoundary).await;
        let update = finish_supervisor(supervisor, update, SolverAction::StopAtNonceBoundary).await;

        assert_eq!(update.reason, "template_refresh");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_template_replaces_provisional_work_at_a_nonce_boundary() {
        let initial = template(101, TIP0, "A");
        let node = MockNode::new(initial.clone());
        let endpoint = serve(node.clone()).await;
        let (action_tx, mut action_rx) = watch::channel(SolverAction::Continue);
        let update = template_update_slot();
        let supervisor = tokio::spawn(supervise_template_updates(
            reqwest::Client::new(),
            endpoint,
            summarize_template(&initial.to_json(false)),
            action_tx,
            TemplateRefreshPolicy {
                interval: None,
                mine_provisional_empty_templates: true,
            },
            true,
            update.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(20)).await;
        node.state.send_replace(Template {
            transactions: 3,
            longpollid: "B".to_string(),
            submit_old: Some(true),
            ..initial
        });
        wait_for_action(&mut action_rx, SolverAction::StopAtNonceBoundary).await;
        let update = finish_supervisor(supervisor, update, SolverAction::StopAtNonceBoundary).await;

        assert_eq!(update.reason, "provisional_replaced");
        assert_eq!(update.summary.transactions, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tip_change_upgrades_a_pending_nonce_boundary_stop() {
        let initial = template(101, TIP0, "A");
        let node = MockNode::new(initial.clone());
        let endpoint = serve(node.clone()).await;
        let (action_tx, mut action_rx) = watch::channel(SolverAction::Continue);
        let update = template_update_slot();
        let supervisor = tokio::spawn(supervise_template_updates(
            reqwest::Client::new(),
            endpoint,
            summarize_template(&initial.to_json(false)),
            action_tx,
            TemplateRefreshPolicy {
                interval: None,
                mine_provisional_empty_templates: true,
            },
            true,
            update.clone(),
        ));

        tokio::time::sleep(Duration::from_millis(20)).await;
        node.state.send_replace(Template {
            transactions: 3,
            longpollid: "B".to_string(),
            submit_old: Some(true),
            ..initial
        });
        wait_for_action(&mut action_rx, SolverAction::StopAtNonceBoundary).await;

        node.state.send_replace(Template {
            height: 102,
            prev_hash: TIP1.to_string(),
            longpollid: "C".to_string(),
            transactions: 0,
            submit_old: Some(false),
        });
        wait_for_action(&mut action_rx, SolverAction::StopNow).await;
        let update = finish_supervisor(supervisor, update, SolverAction::StopNow).await;

        assert_eq!(update.reason, "tip_changed");
        assert_eq!(update.summary.height, 102);
    }

    #[test]
    fn stop_now_interrupts_a_simulated_pass() {
        let header = block_from_template(&template(101, TIP0, "A").to_json(false))
            .expect("template parses")
            .header;
        let (action_tx, action_rx) = watch::channel(SolverAction::Continue);
        let solver = SimulatedSolver {
            pass: PASS,
            passes_to_solution: 1,
            honor_actions: true,
        };
        let started = Instant::now();
        let solve = std::thread::spawn(move || solver.solve(header, action_rx));

        std::thread::sleep(Duration::from_millis(100));
        action_tx.send_replace(SolverAction::StopNow);

        assert_eq!(
            solve.join().expect("solver did not panic"),
            Err(SolverCancelled)
        );
        assert!(started.elapsed() < PASS);
    }

    /// A block at height 101 arrives while the solver is inside a pass for the
    /// same height. The solution that pass produces is for a height the node
    /// has already filled, so the miner must not publish it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_solution_found_after_the_tip_moved_is_discarded() {
        let submitted = mine_through_template_change(
            "stale-discarded",
            Template {
                height: 102,
                prev_hash: TIP1.to_string(),
                longpollid: "B".to_string(),
                transactions: 0,
                submit_old: Some(false),
            },
            false,
        )
        .await;

        assert!(
            submitted.is_empty(),
            "the miner published {} block(s) for a height the node had already filled",
            submitted.len()
        );
    }

    /// The same run with the guard off, which is how the fleet mined before it
    /// existed: the miner publishes a sibling of a block its own node has
    /// already committed, which is exactly the observed orphan.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn without_the_guard_the_miner_publishes_an_orphan() {
        let submitted = mine_through_template_change(
            "stale-submitted",
            Template {
                height: 102,
                prev_hash: TIP1.to_string(),
                longpollid: "B".to_string(),
                transactions: 0,
                submit_old: Some(false),
            },
            true,
        )
        .await;

        assert_eq!(submitted.len(), 1, "expected one submitted block");
        assert_eq!(
            parent_of(&submitted[0]),
            TIP0,
            "the submitted block extends the superseded tip"
        );
    }

    /// A mempool change cancels the solver too, but it does not invalidate the
    /// height being mined, so the solution must still be published.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_solution_found_after_a_mempool_change_is_published() {
        let submitted = mine_through_template_change(
            "mempool-changed",
            Template {
                height: 101,
                prev_hash: TIP0.to_string(),
                longpollid: "C".to_string(),
                transactions: 3,
                submit_old: Some(true),
            },
            false,
        )
        .await;

        assert_eq!(submitted.len(), 1, "expected one submitted block");
        assert_eq!(parent_of(&submitted[0]), TIP0);
    }
}
