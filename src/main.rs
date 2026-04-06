mod bootstrap;
mod cloud;
mod commands;
mod config;
mod s3;
mod ssh;
mod tmux;
mod txblast;
mod zebra_config;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "kresko", about = "Zcash experimental network deployer")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize experiment directory structure, configs, and .env
    Init {
        /// Chain ID
        #[arg(short = 'c', long)]
        chain_id: String,

        /// Experiment name
        #[arg(short = 'e', long)]
        experiment: String,

        /// Cloud provider
        #[arg(long, default_value = "digitalocean")]
        provider: String,

        /// Path to SSH public key
        #[arg(long)]
        ssh_pub_key_path: Option<String>,

        /// SSH key name in cloud provider
        #[arg(long)]
        ssh_key_name: Option<String>,

        /// Mining mode: "generate" (default, PoW disabled) or "pow" (real PoW mining)
        #[arg(long, default_value = "generate")]
        mining_mode: String,

        /// Target block time in seconds (default: 75, post-Blossom)
        #[arg(long)]
        block_time: Option<u32>,
    },

    /// Add nodes to the experiment config
    Add {
        /// Node type (miner)
        #[arg(short = 't', long, default_value = "miner")]
        node_type: String,

        /// Number of nodes to add
        #[arg(short = 'c', long, default_value = "1")]
        count: usize,

        /// Cloud provider
        #[arg(long)]
        provider: Option<String>,

        /// Region (or "random")
        #[arg(long, default_value = "random")]
        region: String,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Spin up cloud instances
    Up {
        /// Number of parallel workers
        #[arg(long, default_value = "4")]
        workers: usize,

        /// Path to SSH public key
        #[arg(long)]
        ssh_pub_key_path: Option<String>,

        /// SSH key name in cloud provider
        #[arg(long)]
        ssh_key_name: Option<String>,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Generate deployment payload (configs, peers, binaries)
    Genesis {
        /// Path to pre-built zebrad binary
        #[arg(long)]
        zebrad_binary: String,

        /// Path to kresko binary to ship to remote nodes (defaults to current executable)
        #[arg(long, alias = "txblast-binary")]
        kresko_binary: Option<String>,

        /// Build directory name
        #[arg(long, default_value = "build")]
        build_dir: String,

        /// Extra empty local-genesis blocks to seed after funding blocks so premine outputs mature
        #[arg(long, default_value_t = 125)]
        maturity_padding_blocks: u32,

        /// Bootstrap mode: auto (cached for PoW, generated otherwise), generated, or cached
        #[arg(long, default_value = "auto")]
        bootstrap_mode: String,

        /// Initial Orchard lanes to create per miner during shielded txblast warmup
        #[arg(long, default_value_t = 384)]
        orchard_lanes_per_miner: usize,

        /// Target value of each initial Orchard lane note, in zatoshis
        #[arg(long, default_value_t = 100_000)]
        orchard_lane_value_zats: u64,

        /// Preferred minimum value for reservoir notes used by fanout, in zatoshis
        #[arg(long, default_value_t = 500_000)]
        orchard_fanout_source_value_zats: u64,

        /// Number of child lane notes created by each fanout transaction
        #[arg(long, default_value_t = 4)]
        orchard_fanout_outputs: usize,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Deploy payload to cloud instances and start nodes
    Deploy {
        /// Path to SSH private key
        #[arg(long)]
        ssh_key_path: Option<String>,

        /// Upload payload directly via SCP instead of S3
        #[arg(long)]
        direct_payload_upload: bool,

        /// Number of parallel workers
        #[arg(long, default_value = "4")]
        workers: usize,

        /// Continue even if some miners fail
        #[arg(long)]
        ignore_failed_miners: bool,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Query node status (block heights, sync progress)
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Health check: are all nodes reachable, advancing, and in sync?
    /// Exits with code 1 if unhealthy.
    Check {
        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// List running kresko instances in the cloud
    List {
        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Progress chain by generating blocks on miner RPC endpoints
    Progress {
        /// Block interval in seconds
        #[arg(short = 't', long = "block-time", default_value = "10")]
        block_time: u64,

        /// Pick miners randomly each interval instead of rotating
        #[arg(long)]
        random: bool,

        /// Number of miners to ping concurrently each interval
        #[arg(short = 'c', long, default_value = "1")]
        concurrent: usize,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Start transaction blaster on remote nodes
    Txblast {
        /// Comma-separated instance indices or "all"
        #[arg(short = 'i', long, default_value = "all")]
        instances: String,

        /// Transaction type: transparent, shielded, or both
        #[arg(long, default_value = "transparent")]
        tx_type: String,

        /// Transactions per second
        #[arg(long, default_value = "10")]
        rate: u64,

        /// Amount per transaction (in ZEC)
        #[arg(long, default_value = "0.001")]
        amount: f64,

        /// Maximum Orchard transactions allowed in flight per node
        #[arg(long)]
        orchard_max_in_flight: Option<usize>,

        /// Target number of ready Orchard lanes to maintain
        #[arg(long)]
        orchard_target_ready_lanes: Option<usize>,

        /// Trigger fanout when ready Orchard lanes fall below this watermark
        #[arg(long)]
        orchard_lane_low_watermark: Option<usize>,

        /// Maximum Orchard fanout transactions allowed in flight per node
        #[arg(long)]
        orchard_fanout_max_in_flight: Option<usize>,

        /// Orchard progress log interval in seconds
        #[arg(long)]
        orchard_progress_interval_secs: Option<u64>,

        /// Enable txblast JSONL tracing on remote nodes
        #[arg(long)]
        trace_enable: bool,

        /// Trace directory for txblast JSONL files on remote nodes
        #[arg(long)]
        trace_dir: Option<String>,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Query txblast Orchard readiness across remote nodes
    TxblastStatus {
        /// Comma-separated instance indices or "all"
        #[arg(short = 'i', long, default_value = "all")]
        instances: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Trace directory containing txblast JSONL files on remote nodes
        #[arg(long, default_value = "/root/traces")]
        trace_dir: String,

        /// Consider non-ready status stale after this many seconds
        #[arg(long, default_value_t = 120)]
        stall_secs: i64,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Distribute cached treasury funds into per-node runtime funded keys
    FundRuntimeKeys {
        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Run PoW miner locally (intended to run on remote nodes)
    Mine {
        /// RPC endpoint
        #[arg(long, default_value = "http://localhost:18232")]
        rpc_endpoint: String,
    },

    /// Start PoW mining on remote nodes
    StartMiners {
        /// Comma-separated instance indices or "all"
        #[arg(short = 'i', long, default_value = "all")]
        instances: String,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Run txblast locally (intended to run on remote nodes)
    TxblastLocal {
        /// RPC endpoint
        #[arg(long, default_value = "http://localhost:18232")]
        rpc_endpoint: String,

        /// Transaction type: transparent, shielded, or both
        #[arg(long, default_value = "transparent")]
        tx_type: String,

        /// Transactions per second
        #[arg(long, default_value = "10")]
        rate: u64,

        /// Amount per transaction (in ZEC)
        #[arg(long, default_value = "0.001")]
        amount: f64,

        /// Initial Orchard lanes to create from matured transparent premine funds
        #[arg(long)]
        orchard_lanes_per_miner: Option<usize>,

        /// Target value of each initial Orchard lane note, in zatoshis
        #[arg(long)]
        orchard_lane_value_zats: Option<u64>,

        /// Preferred minimum value for reservoir notes used by fanout, in zatoshis
        #[arg(long)]
        orchard_fanout_source_value_zats: Option<u64>,

        /// Number of child lane notes created by each fanout transaction
        #[arg(long)]
        orchard_fanout_outputs: Option<usize>,

        /// Maximum Orchard transactions allowed in flight
        #[arg(long)]
        orchard_max_in_flight: Option<usize>,

        /// Target number of ready Orchard lanes to maintain
        #[arg(long)]
        orchard_target_ready_lanes: Option<usize>,

        /// Trigger fanout when ready Orchard lanes fall below this watermark
        #[arg(long)]
        orchard_lane_low_watermark: Option<usize>,

        /// Maximum Orchard fanout transactions allowed in flight
        #[arg(long)]
        orchard_fanout_max_in_flight: Option<usize>,

        /// Orchard progress log interval in seconds
        #[arg(long)]
        orchard_progress_interval_secs: Option<u64>,

        /// Enable txblast JSONL tracing
        #[arg(long)]
        trace_enable: bool,

        /// Trace directory for txblast JSONL files
        #[arg(long)]
        trace_dir: Option<String>,

        /// Path to premine funded key JSON (optional, auto-detected on nodes)
        #[arg(long)]
        funded_key_path: Option<String>,

        /// Expected runtime funding transaction id for shielded bootstrap diagnostics
        #[arg(long)]
        expected_runtime_funding_txid: Option<String>,
    },

    /// Fund runtime keys locally from the cached bootstrap treasury (intended to run on remote nodes)
    FundRuntimeKeysLocal {
        /// RPC endpoint
        #[arg(long, default_value = "http://localhost:18232")]
        rpc_endpoint: String,

        /// Directory containing local genesis bootstrap artifacts on the remote node
        #[arg(long, default_value = "/root/payload/local_genesis")]
        local_genesis_dir: String,

        /// Minimum confirmed balance to place on each runtime funded key
        #[arg(long)]
        minimum_recipient_zats: u64,

        /// Timeout while waiting for the funding transaction to confirm
        #[arg(long, default_value_t = 600)]
        confirm_timeout_secs: u64,

        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Verify local runtime funding visibility without submitting funding transactions
        #[arg(long)]
        verify_only: bool,

        /// Expected transparent runtime funding transaction id
        #[arg(long)]
        expected_funding_txid: Option<String>,
    },

    /// Query txblast Orchard readiness from local trace files (intended to run on remote nodes)
    TxblastStatusLocal {
        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Trace directory containing txblast JSONL files
        #[arg(long, default_value = "/root/traces")]
        trace_dir: String,

        /// Consider non-ready status stale after this many seconds
        #[arg(long, default_value_t = 120)]
        stall_secs: i64,
    },

    /// Kill tmux sessions on remote nodes
    KillSession {
        /// Session name to kill
        #[arg(short = 's', long)]
        session: String,

        /// Timeout in seconds for graceful shutdown
        #[arg(long, default_value = "30")]
        timeout: u64,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Download logs and data from remote nodes
    Download {
        #[command(subcommand)]
        target: Option<DownloadTarget>,

        /// Node name pattern (or "all")
        #[arg(short = 'n', long, default_value = "all")]
        nodes: String,

        /// Number of parallel workers
        #[arg(long, default_value = "4")]
        workers: usize,

        /// Skip remote compression before download
        #[arg(long)]
        no_compress: bool,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Upload collected data to S3
    UploadData {
        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Stop services and clean up remote nodes
    Reset {
        /// Comma-separated miner indices or "all"
        #[arg(long, default_value = "all")]
        miners: String,

        /// Number of parallel workers
        #[arg(long, default_value = "4")]
        workers: usize,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Destroy cloud instances
    Down {
        /// Destroy all kresko instances across all experiments
        #[arg(long)]
        all: bool,

        /// Number of parallel workers
        #[arg(long, default_value = "4")]
        workers: usize,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },
}

#[derive(Subcommand)]
enum DownloadTarget {
    /// Download block height/time/size traces via node RPC and store JSONL locally
    Heights {
        /// Number of active nodes to download from
        #[arg(short = 'n', long = "nodes", default_value_t = 1)]
        node_count: usize,
    },
    /// Download selected structured trace JSONL tables from remote nodes
    Traces {
        /// Comma-separated trace tables: all, peer_message, trace_dropped, txblast_event, txblast_registry, txblast_note, txblast_trace_dropped
        #[arg(long, default_value = "all")]
        tables: String,
    },
}

impl Commands {
    fn directory(&self) -> Option<&str> {
        match self {
            Commands::Init { .. }
            | Commands::TxblastLocal { .. }
            | Commands::FundRuntimeKeysLocal { .. }
            | Commands::TxblastStatusLocal { .. }
            | Commands::Mine { .. } => None,
            Commands::Add { directory, .. }
            | Commands::Up { directory, .. }
            | Commands::Genesis { directory, .. }
            | Commands::Deploy { directory, .. }
            | Commands::Status { directory, .. }
            | Commands::Check { directory, .. }
            | Commands::List { directory }
            | Commands::Progress { directory, .. }
            | Commands::StartMiners { directory, .. }
            | Commands::FundRuntimeKeys { directory }
            | Commands::Txblast { directory, .. }
            | Commands::TxblastStatus { directory, .. }
            | Commands::KillSession { directory, .. }
            | Commands::Download { directory, .. }
            | Commands::UploadData { directory }
            | Commands::Reset { directory, .. }
            | Commands::Down { directory, .. } => Some(directory),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load .env files with override so they always win over shell env vars.
    // Priority (lowest → highest): CWD, ancestor of experiment dir, experiment dir.
    let _ = dotenvy::dotenv_override();
    if let Some(dir) = cli.command.directory() {
        // Walk up from the experiment directory's parent looking for a shared .env.
        // This lets users place credentials in a parent directory shared across experiments.
        if let Some(parent) = std::path::Path::new(dir).canonicalize().ok() {
            let mut ancestor = parent.parent().map(|p| p.to_path_buf());
            while let Some(dir) = ancestor {
                let env_path = dir.join(".env");
                if env_path.is_file() {
                    let _ = dotenvy::from_path_override(&env_path);
                    break;
                }
                ancestor = dir.parent().map(|p| p.to_path_buf());
            }
        }
        // Experiment directory .env wins over everything.
        let env_path = std::path::Path::new(dir).join(".env");
        let _ = dotenvy::from_path_override(&env_path);
    }

    match cli.command {
        Commands::Init {
            chain_id,
            experiment,
            provider,
            ssh_pub_key_path,
            ssh_key_name,
            mining_mode,
            block_time,
        } => {
            let mining_mode: config::MiningMode = mining_mode.parse()?;
            commands::init::run(
                &chain_id,
                &experiment,
                &provider,
                ssh_pub_key_path,
                ssh_key_name,
                mining_mode,
                block_time,
            )?;
        }
        Commands::Add {
            node_type,
            count,
            provider,
            region,
            directory,
        } => {
            commands::add::run(&node_type, count, provider.as_deref(), &region, &directory)?;
        }
        Commands::Up {
            workers,
            ssh_pub_key_path,
            ssh_key_name,
            directory,
        } => {
            commands::up::run(workers, ssh_pub_key_path, ssh_key_name, &directory).await?;
        }
        Commands::Genesis {
            zebrad_binary,
            kresko_binary,
            build_dir,
            maturity_padding_blocks,
            bootstrap_mode,
            orchard_lanes_per_miner,
            orchard_lane_value_zats,
            orchard_fanout_source_value_zats,
            orchard_fanout_outputs,
            directory,
        } => {
            commands::genesis::run(
                &zebrad_binary,
                kresko_binary.as_deref(),
                &build_dir,
                maturity_padding_blocks,
                &bootstrap_mode,
                orchard_lanes_per_miner,
                orchard_lane_value_zats,
                orchard_fanout_source_value_zats,
                orchard_fanout_outputs,
                &directory,
            )?;
        }
        Commands::Deploy {
            ssh_key_path,
            direct_payload_upload,
            workers,
            ignore_failed_miners,
            directory,
        } => {
            commands::deploy::run(
                ssh_key_path.as_deref(),
                direct_payload_upload,
                workers,
                ignore_failed_miners,
                &directory,
            )
            .await?;
        }
        Commands::Status { json, directory } => {
            commands::status::run(json, &directory).await?;
        }
        Commands::Check { json, directory } => {
            commands::check::run(json, &directory).await?;
        }
        Commands::List { directory } => {
            commands::list::run(&directory).await?;
        }
        Commands::Progress {
            block_time,
            random,
            concurrent,
            directory,
        } => {
            commands::progress::run(block_time, random, concurrent, &directory).await?;
        }
        Commands::Mine { rpc_endpoint } => {
            commands::mine::run(&rpc_endpoint).await?;
        }
        Commands::StartMiners {
            instances,
            directory,
        } => {
            commands::start_miners::run(&instances, &directory).await?;
        }
        Commands::FundRuntimeKeys { directory } => {
            commands::fund_runtime_keys::run(&directory).await?;
        }
        Commands::Txblast {
            instances,
            tx_type,
            rate,
            amount,
            orchard_max_in_flight,
            orchard_target_ready_lanes,
            orchard_lane_low_watermark,
            orchard_fanout_max_in_flight,
            orchard_progress_interval_secs,
            trace_enable,
            trace_dir,
            directory,
        } => {
            let tx_type: config::TxType = tx_type.parse()?;
            commands::txblast::run(
                &instances,
                tx_type,
                rate,
                amount,
                orchard_max_in_flight,
                orchard_target_ready_lanes,
                orchard_lane_low_watermark,
                orchard_fanout_max_in_flight,
                orchard_progress_interval_secs,
                trace_enable,
                trace_dir.as_deref(),
                &directory,
            )
            .await?;
        }
        Commands::TxblastStatus {
            instances,
            json,
            trace_dir,
            stall_secs,
            directory,
        } => {
            commands::txblast_status::run(&instances, json, &trace_dir, stall_secs, &directory)
                .await?;
        }
        Commands::TxblastLocal {
            rpc_endpoint,
            tx_type,
            rate,
            amount,
            orchard_lanes_per_miner,
            orchard_lane_value_zats,
            orchard_fanout_source_value_zats,
            orchard_fanout_outputs,
            orchard_max_in_flight,
            orchard_target_ready_lanes,
            orchard_lane_low_watermark,
            orchard_fanout_max_in_flight,
            orchard_progress_interval_secs,
            trace_enable,
            trace_dir,
            funded_key_path,
            expected_runtime_funding_txid,
        } => {
            let tx_type: config::TxType = tx_type.parse()?;
            txblast::run_local(
                &rpc_endpoint,
                tx_type,
                rate,
                amount,
                orchard_lanes_per_miner,
                orchard_lane_value_zats,
                orchard_fanout_source_value_zats,
                orchard_fanout_outputs,
                orchard_max_in_flight,
                orchard_target_ready_lanes,
                orchard_lane_low_watermark,
                orchard_fanout_max_in_flight,
                orchard_progress_interval_secs,
                trace_enable,
                trace_dir.as_deref(),
                funded_key_path.as_deref(),
                expected_runtime_funding_txid.as_deref(),
            )
            .await?;
        }
        Commands::FundRuntimeKeysLocal {
            rpc_endpoint,
            local_genesis_dir,
            minimum_recipient_zats,
            confirm_timeout_secs,
            json,
            verify_only,
            expected_funding_txid,
        } => {
            commands::fund_runtime_keys::run_local(
                &rpc_endpoint,
                &local_genesis_dir,
                minimum_recipient_zats,
                confirm_timeout_secs,
                json,
                verify_only,
                expected_funding_txid.as_deref(),
            )
            .await?;
        }
        Commands::TxblastStatusLocal {
            json,
            trace_dir,
            stall_secs,
        } => {
            commands::txblast_status::run_local(json, &trace_dir, stall_secs)?;
        }
        Commands::KillSession {
            session,
            timeout,
            directory,
        } => {
            commands::kill_session::run(&session, timeout, &directory).await?;
        }
        Commands::Download {
            target,
            nodes,
            workers,
            no_compress,
            directory,
        } => match target {
            Some(DownloadTarget::Heights { node_count }) => {
                commands::download_heights::run(node_count, workers, &directory).await?;
            }
            Some(DownloadTarget::Traces { tables }) => {
                commands::download::run_traces(&nodes, workers, &tables, &directory).await?;
            }
            None => {
                commands::download::run_logs(&nodes, workers, no_compress, &directory).await?;
            }
        },
        Commands::UploadData { directory } => {
            commands::upload_data::run(&directory).await?;
        }
        Commands::Reset {
            miners,
            workers,
            directory,
        } => {
            commands::reset::run(&miners, workers, &directory).await?;
        }
        Commands::Down {
            all,
            workers,
            directory,
        } => {
            commands::down::run(all, workers, &directory).await?;
        }
    }

    Ok(())
}
