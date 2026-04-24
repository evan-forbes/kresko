mod cloud;
mod commands;
mod config;
mod pow_sim;
mod pow_tuning;
mod premine;
mod run_manifest;
mod s3;
mod ssh;
mod tmux;
mod txblast;
mod zebra_config;

use anyhow::Result;
use clap::{Parser, Subcommand};

const DEFAULT_WORKERS: usize = 16;

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
        #[arg(short = 'p', long, default_value = "digitalocean")]
        provider: String,

        /// Path to SSH public key
        #[arg(short = 'k', long)]
        ssh_pub_key_path: Option<String>,

        /// SSH key name in cloud provider
        #[arg(short = 'K', long)]
        ssh_key_name: Option<String>,

        /// Mining mode: "generate" (default, PoW disabled) or "pow" (real PoW mining)
        #[arg(short = 'm', long, default_value = "generate")]
        mining_mode: String,

        /// Target block time in seconds (default: 75, post-Blossom)
        #[arg(short = 't', long)]
        block_time: Option<u32>,

        /// Equihash parameter set for configured testnets: regtest/easy (48,5) or common (200,9)
        #[arg(long, default_value = "regtest")]
        equihash_params: String,

        /// Optional shared env file to seed the generated experiment .env
        #[arg(short = 's', long)]
        env_source: Option<String>,
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
        #[arg(short = 'p', long)]
        provider: Option<String>,

        /// Use a low-resource instance size (for proving small nodes can keep up)
        #[arg(short = 'l', long, default_value = "false")]
        low_resource: bool,

        /// Region (or "random")
        #[arg(short = 'r', long, default_value = "random")]
        region: String,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Spin up cloud instances
    Up {
        /// Number of parallel workers
        #[arg(short = 'w', long, default_value_t = DEFAULT_WORKERS)]
        workers: usize,

        /// Path to SSH public key
        #[arg(short = 'k', long)]
        ssh_pub_key_path: Option<String>,

        /// SSH key name in cloud provider
        #[arg(short = 'K', long)]
        ssh_key_name: Option<String>,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Sync instance IPs from cloud provider state back into config.json
    SyncIps {
        /// Refresh already-populated IP fields instead of only filling missing values
        #[arg(short = 'o', long, default_value_t = false)]
        overwrite: bool,

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

        /// Extra empty local-genesis blocks to seed after funding blocks so premine outputs mature.
        /// Only used by the non-PoW genesis path; the PoW path uses the cached premine bundle's
        /// own fixed padding (see `src/premine.rs::MATURITY_PADDING_BLOCKS`).
        #[arg(long, default_value_t = 125)]
        maturity_padding_blocks: u32,

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
        #[arg(long, default_value_t = 1)]
        orchard_fanout_outputs: usize,

        /// Directory whose contents are baked into the payload under `scripts/`.
        /// Resolved relative to the experiment directory unless absolute.
        #[arg(long, default_value = "scripts")]
        scripts_dir: String,

        /// Fractional adjustment to the natural calibrated target.
        /// `+0.10` = ~10% looser target (faster initial blocks); `-0.10` =
        /// ~10% tighter. Leave at 0 unless observed block times on your
        /// fleet need a nudge.
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        pow_adjust: f64,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Deploy payload to cloud instances and start nodes
    Deploy {
        /// Path to SSH private key
        #[arg(short = 'k', long)]
        ssh_key_path: Option<String>,

        /// Comma-separated miner indices, "all", or wildcard patterns
        #[arg(short = 'n', long, default_value = "all")]
        nodes: String,

        /// Number of parallel workers
        #[arg(short = 'w', long, default_value_t = DEFAULT_WORKERS)]
        workers: usize,

        /// Continue even if some miners fail
        #[arg(short = 'i', long)]
        ignore_failed_miners: bool,

        /// Reuse an existing healthy `app` tmux session instead of failing.
        #[arg(short = 'r', long, default_value_t = false)]
        reuse_app_session: bool,

        /// Kill any existing `app` tmux session before starting the payload.
        #[arg(
            short = 'x',
            long,
            default_value_t = false,
            conflicts_with = "reuse_app_session"
        )]
        restart_app_session: bool,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Query node status (block heights, sync progress)
    Status {
        /// Output as JSON
        #[arg(short = 'j', long)]
        json: bool,

        /// Aggregate node heights into a compact summary
        #[arg(short = 's', long, default_value_t = false)]
        summary: bool,

        /// Include SSH / tmux / loopback RPC diagnostics
        #[arg(short = 'p', long, default_value_t = false)]
        deep: bool,

        /// Path to SSH private key for deep status checks
        #[arg(short = 'k', long)]
        ssh_key_path: Option<String>,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Health check: are all nodes reachable, advancing, and in sync?
    /// Exits with code 1 if unhealthy.
    Check {
        /// Output as JSON
        #[arg(short = 'j', long)]
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

    /// Remove instances with public_ip="TBD" (failed provisioning) from config
    Prune {
        /// Print what would be removed without modifying config
        #[arg(short = 'n', long)]
        dry_run: bool,

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
        #[arg(short = 'r', long)]
        random: bool,

        /// Number of miners to ping concurrently each interval
        #[arg(short = 'c', long, default_value = "1")]
        concurrent: usize,

        /// Subdirectory under data/ for progress.log.jsonl
        #[arg(short = 's', long)]
        data_subdir: Option<String>,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Start transaction blaster on remote nodes
    Txblast {
        /// Comma-separated instance indices or "all"
        #[arg(short = 'i', long, default_value = "all")]
        instances: String,

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

        /// Number of parallel Orchard proving workers
        #[arg(long)]
        orchard_proving_workers: Option<usize>,

        /// Orchard progress log interval in seconds
        #[arg(long)]
        orchard_progress_interval_secs: Option<u64>,

        /// Deprecated no-op: txblast tracing is always enabled. Retained for script compatibility.
        #[arg(long)]
        trace_enable: bool,

        /// Skip runtime funding preflight and start txblast immediately
        #[arg(long)]
        skip_funding: bool,

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

        /// Path to the zebrad.toml whose network parameters should be used for mining
        #[arg(long, default_value = "/root/.config/zebrad.toml")]
        zebrad_config: String,
    },

    /// Monte Carlo-simulate PoW block production for calibration validation
    /// without spinning up a network. Useful for checking whether a given
    /// miner count / target spacing / profile combination produces stable
    /// block times and a tolerable orphan rate.
    PowSimulate {
        /// Number of single-thread miners to simulate.
        #[arg(long)]
        miners: usize,

        /// Per-thread Equihash (200, 9) solutions per second. Run with
        /// `kresko genesis --pow-profile=... -d <dir>` on a representative
        /// host to measure this, or use a value from the CPU-class table.
        #[arg(long)]
        sol_per_sec: f64,

        /// Target block spacing in seconds.
        #[arg(long, default_value_t = 75)]
        target_spacing: u32,

        /// Number of canonical blocks to simulate.
        #[arg(long, default_value_t = 1000)]
        blocks: u32,

        /// Mean inter-miner block-propagation delay (seconds). Used for
        /// orphan-rate accounting.
        #[arg(long, default_value_t = 2.0)]
        propagation_delay: f64,

        /// DAA round-tuning preset.
        #[arg(long, default_value = "mainnet")]
        pow_profile: String,

        /// Headroom bits used during calibration (see `kresko genesis`).
        #[arg(long, default_value_t = 0)]
        pow_headroom_bits: u8,

        /// Explicit hex target_difficulty_limit (64 chars, big-endian, no
        /// `0x` prefix). When set, skips calibration and uses this directly.
        #[arg(long)]
        target_difficulty_limit: Option<String>,

        /// RNG seed for reproducible runs.
        #[arg(long, default_value_t = 0)]
        seed: u64,

        /// Optional path to write a per-block CSV.
        #[arg(long)]
        csv: Option<String>,
    },

    /// Benchmark the compiled Equihash solver used by live mining
    PowBench {
        /// Equihash parameter set to benchmark: common (200,9) or regtest (48,5).
        #[arg(long, default_value = "common")]
        equihash_params: String,

        /// Minimum benchmark duration in seconds. A run may exceed this
        /// because a single solver call is not interrupted.
        #[arg(long, default_value_t = 10.0)]
        min_seconds: f64,
    },

    /// Run a Monte Carlo matrix and write one aggregate CSV row per run
    PowSimulateMatrix {
        /// Comma-separated Equihash labels to compare.
        #[arg(long, default_value = "common,regtest")]
        equihash_params: String,

        /// Per-thread sol/s values. Use either one value for all params,
        /// positional values matching `--equihash-params`, or keyed values
        /// like `common=1.0,regtest=500.0`.
        #[arg(long)]
        sol_per_sec: String,

        /// Comma-separated single-thread miner counts.
        #[arg(long, default_value = "10,20,40,60,80")]
        miners: String,

        /// Target block spacing in seconds.
        #[arg(long, default_value_t = 75)]
        target_spacing: u32,

        /// Number of canonical blocks to simulate per run.
        #[arg(long, default_value_t = 10000)]
        blocks: u32,

        /// Comma-separated mean inter-miner block-propagation delays.
        #[arg(long, default_value = "0.5,1,2,5,10")]
        propagation_delays: String,

        /// DAA round-tuning preset.
        #[arg(long, default_value = "mainnet")]
        pow_profile: String,

        /// Headroom bits used during calibration (see `kresko genesis`).
        #[arg(long, default_value_t = 0)]
        pow_headroom_bits: u8,

        /// Seeds as comma-separated values or an inclusive range like `1..100`.
        #[arg(long, default_value = "1..100")]
        seeds: String,

        /// Path to write aggregate CSV output.
        #[arg(long, default_value = "pow-sim-matrix.csv")]
        csv: String,
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

        /// Number of parallel Orchard proving workers
        #[arg(long)]
        orchard_proving_workers: Option<usize>,

        /// Orchard progress log interval in seconds
        #[arg(long)]
        orchard_progress_interval_secs: Option<u64>,

        /// Deprecated no-op: txblast tracing is always enabled. Retained for script compatibility.
        #[arg(long)]
        trace_enable: bool,

        /// Skip cached runtime funding verification and refresh before startup
        #[arg(long)]
        skip_funding: bool,

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
        #[arg(short = 't', long, default_value = "30")]
        timeout: u64,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Execute a queue of run manifests back-to-back, with optional resume.
    Queue {
        /// Path to queue.toml file.
        #[arg(short = 'f', long)]
        file: String,

        /// Resume from .kresko-queue-state.json if present.
        #[arg(short = 'r', long, default_value = "false")]
        resume: bool,

        /// Stop the queue on a catastrophic failure (per-node init failures
        /// never halt the queue).
        #[arg(short = 'x', long, default_value = "false")]
        halt_on_failure: bool,

        /// Number of parallel workers
        #[arg(short = 'w', long, default_value_t = DEFAULT_WORKERS)]
        workers: usize,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Execute a single run manifest (init_script -> wait -> stop -> collect).
    Run {
        /// Path to the run manifest TOML file.
        #[arg(short = 'f', long)]
        manifest: String,

        /// Number of parallel workers
        #[arg(short = 'w', long, default_value_t = DEFAULT_WORKERS)]
        workers: usize,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Upload and run a script on miners, or execute an inline command.
    Exec {
        /// Local script file to upload and run.
        #[arg(short = 'f', long, conflicts_with = "command")]
        file: Option<String>,

        /// Inline command (runs as bash -c "<command>").
        #[arg(short = 'c', long, conflicts_with = "file")]
        command: Option<String>,

        /// Only re-run on nodes that failed the last exec in this directory.
        #[arg(short = 'r', long, default_value = "false")]
        on_failed: bool,

        /// After all nodes finish, print each node's captured stdout/stderr.
        #[arg(short = 'o', long, default_value = "false")]
        with_output: bool,

        /// Comma-separated miner indices, "all", or wildcard patterns.
        #[arg(short = 'm', long, default_value = "all")]
        miners: String,

        /// Number of parallel workers
        #[arg(short = 'w', long, default_value_t = DEFAULT_WORKERS)]
        workers: usize,

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
        #[arg(short = 'w', long, default_value_t = DEFAULT_WORKERS)]
        workers: usize,

        /// Skip remote compression before download
        #[arg(short = 'c', long)]
        no_compress: bool,

        /// Subdirectory under data/ for downloaded artifacts
        #[arg(short = 's', long)]
        data_subdir: Option<String>,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Download the standard artifact set: logs, heights, and traces
    Collect {
        /// Node name pattern (or "all")
        #[arg(short = 'n', long, default_value = "all")]
        nodes: String,

        /// Number of parallel workers
        #[arg(short = 'w', long, default_value_t = DEFAULT_WORKERS)]
        workers: usize,

        /// `all` downloads every file from discovered trace directories; otherwise pass a comma-separated table list
        #[arg(short = 't', long, default_value = "all")]
        trace_tables: String,

        /// Subdirectory under data/ for downloaded artifacts
        #[arg(short = 's', long)]
        data_subdir: Option<String>,

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
        #[arg(short = 'm', long, default_value = "all")]
        miners: String,

        /// Number of parallel workers
        #[arg(short = 'w', long, default_value_t = DEFAULT_WORKERS)]
        workers: usize,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Destroy cloud instances
    Down {
        /// Destroy all kresko instances across all experiments
        #[arg(short = 'a', long)]
        all: bool,

        /// Number of parallel workers
        #[arg(short = 'w', long, default_value_t = DEFAULT_WORKERS)]
        workers: usize,

        /// Return after delete requests are sent without polling for provider cleanup
        #[arg(short = 'n', long, default_value_t = false)]
        no_wait: bool,

        /// Maximum time to wait for provider-side deletion confirmation
        #[arg(short = 't', long, default_value_t = 300)]
        timeout_secs: u64,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Force-destroy every kresko-managed cloud instance across all experiments.
    /// Only touches provider-wide kresko markers:
    /// DigitalOcean tag `kresko`, GCP label `kresko=true`, Linode group/tag `kresko`.
    ForceDown {
        /// Number of parallel workers
        #[arg(short = 'w', long, default_value_t = DEFAULT_WORKERS)]
        workers: usize,

        /// Return after delete requests are sent without polling for provider cleanup
        #[arg(short = 'n', long, default_value_t = false)]
        no_wait: bool,

        /// Maximum time to wait for provider-side deletion confirmation
        #[arg(short = 't', long, default_value_t = 300)]
        timeout_secs: u64,

        /// Directory used to discover `.env` credentials
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },
}

#[derive(Subcommand)]
enum DownloadTarget {
    /// Download block height/time/size traces via node RPC and store JSONL locally
    Heights {
        /// Number of heights to request from a node at a time before checking for failures
        #[arg(short = 'b', long)]
        batch_size: Option<usize>,

        /// Ignore existing heights.jsonl and redownload every height from scratch
        #[arg(short = 'f', long, default_value_t = false)]
        force: bool,
    },
    /// Download every file from remote trace directories, or a selected trace table subset
    Traces {
        /// `all` downloads every file from discovered trace directories; otherwise pass a comma-separated table list
        #[arg(short = 't', long, default_value = "all")]
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
            | Commands::Mine { .. }
            | Commands::PowSimulate { .. }
            | Commands::PowBench { .. }
            | Commands::PowSimulateMatrix { .. } => None,
            Commands::Add { directory, .. }
            | Commands::Up { directory, .. }
            | Commands::SyncIps { directory, .. }
            | Commands::Genesis { directory, .. }
            | Commands::Deploy { directory, .. }
            | Commands::Status { directory, .. }
            | Commands::Check { directory, .. }
            | Commands::List { directory }
            | Commands::Prune { directory, .. }
            | Commands::Progress { directory, .. }
            | Commands::StartMiners { directory, .. }
            | Commands::FundRuntimeKeys { directory }
            | Commands::Txblast { directory, .. }
            | Commands::TxblastStatus { directory, .. }
            | Commands::KillSession { directory, .. }
            | Commands::Queue { directory, .. }
            | Commands::Run { directory, .. }
            | Commands::Exec { directory, .. }
            | Commands::Download { directory, .. }
            | Commands::Collect { directory, .. }
            | Commands::UploadData { directory }
            | Commands::Reset { directory, .. }
            | Commands::Down { directory, .. }
            | Commands::ForceDown { directory, .. } => Some(directory),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load .env files with override so they always win over shell env vars.
    // Priority (lowest → highest): CWD, ancestor of experiment dir, experiment dir.
    let _ = dotenvy::dotenv_override();
    let (env_anchor, anchor_exists) = match cli.command.directory() {
        Some(dir) => (Some(dir.to_string()), true),
        None => match &cli.command {
            // For Init, the experiment directory doesn't exist yet; anchor on its
            // would-be parent so a shared parent .env still gets picked up.
            Commands::Init { experiment, .. } => (Some(experiment.clone()), false),
            _ => (None, false),
        },
    };
    if let Some(dir) = env_anchor.as_deref() {
        // Determine the directory from which to start walking up for a shared .env.
        // For existing experiments, that's the experiment dir's parent.
        // For Init, the experiment dir doesn't exist, so we start from where it would
        // be created (i.e., its parent's canonical path).
        let anchor = std::path::Path::new(dir);
        let start = if anchor_exists {
            anchor
                .canonicalize()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        } else {
            anchor
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .and_then(|p| p.canonicalize().ok())
                .or_else(|| std::env::current_dir().ok())
        };
        if let Some(start) = start {
            let mut ancestor = Some(start);
            while let Some(dir) = ancestor {
                let env_path = dir.join(".env");
                if env_path.is_file() {
                    let _ = dotenvy::from_path_override(&env_path);
                    break;
                }
                ancestor = dir.parent().map(|p| p.to_path_buf());
            }
        }
        // Experiment directory .env wins over everything (no-op for Init).
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
            equihash_params,
            env_source,
        } => {
            let mining_mode: config::MiningMode = mining_mode.parse()?;
            let equihash_params: config::EquihashParameterSet = equihash_params.parse()?;
            commands::init::run(
                &chain_id,
                &experiment,
                &provider,
                ssh_pub_key_path,
                ssh_key_name,
                mining_mode,
                block_time,
                equihash_params,
                env_source.as_deref(),
            )?;
        }
        Commands::Add {
            node_type,
            count,
            provider,
            low_resource,
            region,
            directory,
        } => {
            commands::add::run(
                &node_type,
                count,
                provider.as_deref(),
                &region,
                &directory,
                low_resource,
            )
            .await?;
        }
        Commands::Up {
            workers,
            ssh_pub_key_path,
            ssh_key_name,
            directory,
        } => {
            commands::up::run(workers, ssh_pub_key_path, ssh_key_name, &directory).await?;
        }
        Commands::SyncIps {
            overwrite,
            directory,
        } => {
            commands::sync_ips::run(&directory, overwrite).await?;
        }
        Commands::Genesis {
            zebrad_binary,
            kresko_binary,
            build_dir,
            maturity_padding_blocks,
            orchard_lanes_per_miner,
            orchard_lane_value_zats,
            orchard_fanout_source_value_zats,
            orchard_fanout_outputs,
            scripts_dir,
            pow_adjust,
            directory,
        } => {
            let pow_calibration = commands::genesis::PowCalibrationCli {
                adjust_fraction: pow_adjust,
            };
            commands::genesis::run(
                &zebrad_binary,
                kresko_binary.as_deref(),
                &build_dir,
                maturity_padding_blocks,
                orchard_lanes_per_miner,
                orchard_lane_value_zats,
                orchard_fanout_source_value_zats,
                orchard_fanout_outputs,
                &scripts_dir,
                pow_calibration,
                &directory,
            )?;
        }
        Commands::Deploy {
            ssh_key_path,
            nodes,
            workers,
            ignore_failed_miners,
            reuse_app_session,
            restart_app_session,
            directory,
        } => {
            commands::deploy::run(
                ssh_key_path.as_deref(),
                &nodes,
                workers,
                ignore_failed_miners,
                reuse_app_session,
                restart_app_session,
                &directory,
            )
            .await?;
        }
        Commands::Status {
            json,
            summary,
            deep,
            ssh_key_path,
            directory,
        } => {
            commands::status::run(json, summary, deep, ssh_key_path.as_deref(), &directory).await?;
        }
        Commands::Check { json, directory } => {
            commands::check::run(json, &directory).await?;
        }
        Commands::List { directory } => {
            commands::list::run(&directory).await?;
        }
        Commands::Prune { dry_run, directory } => {
            commands::prune::run(&directory, dry_run).await?;
        }
        Commands::Progress {
            block_time,
            random,
            concurrent,
            data_subdir,
            directory,
        } => {
            commands::progress::run(
                block_time,
                random,
                concurrent,
                &directory,
                data_subdir.as_deref(),
            )
            .await?;
        }
        Commands::Mine {
            rpc_endpoint,
            zebrad_config,
        } => {
            commands::mine::run(&rpc_endpoint, std::path::Path::new(&zebrad_config)).await?;
        }
        Commands::PowSimulate {
            miners,
            sol_per_sec,
            target_spacing,
            blocks,
            propagation_delay,
            pow_profile,
            pow_headroom_bits,
            target_difficulty_limit,
            seed,
            csv,
        } => {
            let cli = pow_sim::PowSimulateCli {
                num_miners: miners,
                sol_per_sec_per_thread: sol_per_sec,
                target_spacing_secs: target_spacing,
                blocks,
                propagation_delay_secs: propagation_delay,
                pow_profile: pow_profile.parse()?,
                headroom_bits: pow_headroom_bits,
                target_difficulty_limit_hex: target_difficulty_limit,
                seed,
                csv_path: csv,
            };
            pow_sim::run(cli)?;
        }
        Commands::PowBench {
            equihash_params,
            min_seconds,
        } => {
            let equihash_params: config::EquihashParameterSet = equihash_params.parse()?;
            let result = pow_tuning::benchmark_equihash_solver(pow_tuning::PowBenchInputs {
                equihash_params,
                min_seconds,
            })?;
            let (n, k) = match result.equihash_params {
                config::EquihashParameterSet::Common => (200, 9),
                config::EquihashParameterSet::Regtest => (48, 5),
            };
            println!(
                "Equihash benchmark: params={} ({n},{k}), elapsed={:.3}s",
                result.equihash_params, result.elapsed_secs,
            );
            println!(
                "  nonce_trials={} ({:.3}/s)",
                result.nonce_trials, result.nonce_trials_per_sec,
            );
            println!(
                "  equihash_solutions={} ({:.3} sol/s)",
                result.equihash_solutions, result.sol_per_sec,
            );
            println!(
                "  matrix input: --sol-per-sec {}={:.9}",
                result.equihash_params, result.sol_per_sec,
            );
        }
        Commands::PowSimulateMatrix {
            equihash_params,
            sol_per_sec,
            miners,
            target_spacing,
            blocks,
            propagation_delays,
            pow_profile,
            pow_headroom_bits,
            seeds,
            csv,
        } => {
            let cli = pow_sim::PowSimulateMatrixCli {
                equihash_params,
                sol_per_sec,
                miners,
                target_spacing_secs: target_spacing,
                blocks,
                propagation_delays,
                pow_profile: pow_profile.parse()?,
                headroom_bits: pow_headroom_bits,
                seeds,
                csv_path: csv,
            };
            pow_sim::run_matrix(cli)?;
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
            rate,
            amount,
            orchard_max_in_flight,
            orchard_target_ready_lanes,
            orchard_lane_low_watermark,
            orchard_fanout_max_in_flight,
            orchard_proving_workers,
            orchard_progress_interval_secs,
            trace_enable: _,
            skip_funding,
            trace_dir,
            directory,
        } => {
            commands::txblast::run(
                &instances,
                rate,
                amount,
                orchard_max_in_flight,
                orchard_target_ready_lanes,
                orchard_lane_low_watermark,
                orchard_fanout_max_in_flight,
                orchard_proving_workers,
                orchard_progress_interval_secs,
                skip_funding,
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
            orchard_proving_workers,
            orchard_progress_interval_secs,
            trace_enable: _,
            skip_funding,
            trace_dir,
            funded_key_path,
            expected_runtime_funding_txid,
        } => {
            txblast::run_local(
                &rpc_endpoint,
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
                orchard_proving_workers,
                orchard_progress_interval_secs,
                skip_funding,
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
        Commands::Run {
            manifest,
            workers,
            directory,
        } => {
            commands::run::run(&manifest, workers, &directory).await?;
        }
        Commands::Queue {
            file,
            resume,
            halt_on_failure,
            workers,
            directory,
        } => {
            commands::queue::run_queue(&file, workers, &directory, resume, halt_on_failure).await?;
        }
        Commands::Exec {
            file,
            command,
            on_failed,
            with_output,
            miners,
            workers,
            directory,
        } => {
            let target = match (file, command) {
                (Some(path), None) => commands::exec::ExecTarget::LocalFile(path.into()),
                (None, Some(cmd)) => commands::exec::ExecTarget::InlineCommand(cmd),
                (None, None) => {
                    anyhow::bail!("must provide one of --file or --command");
                }
                (Some(_), Some(_)) => unreachable!("clap conflicts_with prevents this"),
            };
            commands::exec::run(&miners, workers, &directory, target, on_failed, with_output)
                .await?;
        }
        Commands::Download {
            target,
            nodes,
            workers,
            no_compress,
            data_subdir,
            directory,
        } => {
            let subdir = data_subdir.as_deref();
            match target {
                Some(DownloadTarget::Heights { batch_size, force }) => {
                    commands::download_heights::run(
                        &nodes, workers, batch_size, force, &directory, subdir,
                    )
                    .await?;
                }
                Some(DownloadTarget::Traces { tables }) => {
                    commands::download::run_traces(&nodes, workers, &tables, &directory, subdir)
                        .await?;
                }
                None => {
                    commands::download::run_logs(&nodes, workers, no_compress, &directory, subdir)
                        .await?;
                }
            }
        }
        Commands::Collect {
            nodes,
            workers,
            trace_tables,
            data_subdir,
            directory,
        } => {
            commands::collect::run(
                &nodes,
                workers,
                &trace_tables,
                &directory,
                data_subdir.as_deref(),
            )
            .await?;
        }
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
            no_wait,
            timeout_secs,
            directory,
        } => {
            commands::down::run(all, workers, !no_wait, timeout_secs, &directory).await?;
        }
        Commands::ForceDown {
            workers,
            no_wait,
            timeout_secs,
            directory,
        } => {
            commands::down::run_force(workers, !no_wait, timeout_secs, &directory).await?;
        }
    }

    Ok(())
}
