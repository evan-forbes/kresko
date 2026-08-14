mod commands;
mod config;
mod pow_sim;
mod pow_tuning;
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
    /// Initialize the ~/.kresko/ home (fleets/, assets/, cache/, .env, config.toml).
    /// Fleets are defined in plain Python scripts using the `kresko` package;
    /// there are no bundled templates to scaffold.
    Init,

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

        /// Extra empty local-genesis blocks to seed after funding blocks so generated outputs mature.
        /// PoW starts after the seeded tip, so these blocks do not require solutions.
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

        /// Override the local-to-fleet benchmark discount. Higher values make
        /// the initial target looser/faster.
        #[arg(long)]
        pow_fleet_discount: Option<f64>,

        /// Per-miner solutions/second to calibrate against, skipping the local
        /// benchmark. Measure it from a run as
        /// `2^256 / pow_limit / observed_spacing / miners`.
        #[arg(long)]
        pow_sol_per_sec: Option<f64>,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Generate deployment payload for public Testnet/Mainnet observer nodes
    GenesisPublic {
        /// Path to pre-built zebrad binary
        #[arg(long)]
        zebrad_binary: String,

        /// Path to kresko binary to ship to remote nodes (defaults to current executable)
        #[arg(long, alias = "txblast-binary")]
        kresko_binary: Option<String>,

        /// Build directory name
        #[arg(long, default_value = "build")]
        build_dir: String,

        /// Directory whose contents are baked into the payload under `scripts/`.
        /// Resolved relative to the experiment directory unless absolute.
        #[arg(long, default_value = "scripts")]
        scripts_dir: String,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Render local-fleet node configs from a generated genesis payload.
    ///
    /// Reads each miner's `payload/<name>` config and writes a localized
    /// `nodes/<name>/zakura.toml` (plus a bootstrap variant and a copy of the
    /// funded key) bound to that node's own 127.0.0.x loopback and directories,
    /// so N nodes coexist on one host. Replaces the mempool-load harness's
    /// former Python `prepare_node_dirs`.
    LocalizeFleet {
        /// How many nodes (from the front of the list) run zakurad's internal miner.
        #[arg(long, default_value_t = 1)]
        miner_nodes: usize,

        /// Experiment/lab directory (must match the harness's resolved lab dir).
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Seed a running node's chain state from a generated genesis payload.
    ///
    /// Submits the genesis block and every premine block to the node's RPC via
    /// `submitblock`. The caller owns the node process lifecycle. Replaces the
    /// mempool-load harness's former Python `seed_node`/`submit_block`.
    SeedLocal {
        /// Node RPC endpoint, e.g. `http://127.0.0.101:18232`.
        #[arg(long)]
        rpc_endpoint: String,

        /// Path to the genesis block hex (`payload/local_genesis/genesis.hex`).
        #[arg(long)]
        genesis: String,

        /// Path to the premine blocks hex, one block per line
        /// (`payload/local_genesis/premine_blocks.hex`).
        #[arg(long)]
        premine: String,
    },

    /// Generate a join bundle for outside NU7 testnet observers (binaries pulled
    /// from GitHub releases by the join script; see scripts/join-nu7-testnet.sh)
    JoinBundle {
        /// Kresko experiment directory containing config.json and payload/local_genesis
        #[arg(long)]
        run_dir: String,

        /// Zebra GitHub release repo (owner/name) the join script downloads zebrad from
        #[arg(long, default_value = "valargroup/zebra")]
        zebra_repo: String,

        /// Zebra GitHub release tag the join script downloads zebrad from
        #[arg(long, default_value = "nu7-testnet-v0.1.2")]
        zebra_release_tag: String,

        /// Kresko GitHub release repo (owner/name) the join script downloads kresko from when --mine is enabled
        #[arg(long, default_value = "valargroup/kresko")]
        kresko_repo: String,

        /// Kresko GitHub release tag the join script downloads kresko from when --mine is enabled
        #[arg(long, default_value = "v0.1.0")]
        kresko_release_tag: String,

        /// Output directory for the generated join bundle
        #[arg(long)]
        out: String,
    },

    /// Public-network txblast workflow (wallet, deposits, plan/prepare/run/stop/status/withdraw/recover)
    Txblast {
        #[command(subcommand)]
        command: TxblastCommand,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Show node height and RPC health across an experiment or run directory
    Status {
        /// Output JSON
        #[arg(long)]
        json: bool,

        /// Print only aggregate height buckets
        #[arg(long)]
        summary: bool,

        /// Also check SSH, local RPC, tmux sessions, and recent logs
        #[arg(long)]
        deep: bool,

        /// SSH private key path for deep checks
        #[arg(long)]
        ssh_key_path: Option<String>,

        /// Experiment or run directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// TOML-aware reads/writes against a deployed zebrad.toml.
    /// Used by node_init.sh in place of awk/sed string surgery.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Run PoW miner locally (intended to run on remote nodes)
    Mine {
        /// RPC endpoint
        #[arg(long)]
        rpc_endpoint: String,

        /// Path to the zebrad.toml whose network parameters should be used for mining
        #[arg(long, default_value = "/root/.config/zebrad.toml")]
        zebrad_config: String,

        /// Submit a solution even after the node has committed a block at the
        /// same height.
        ///
        /// A solver pass cannot be interrupted, so a solution can arrive after
        /// the tip has already moved past it. Submitting it forks the chain and
        /// wins nothing, so the miner drops it by default. This flag restores
        /// the older behaviour for orphan-rate comparisons.
        #[arg(long)]
        submit_stale_solutions: bool,

        /// Keep a still-valid mining template for at least this many seconds.
        /// Set to zero to keep it until the tip changes or it wins.
        #[arg(long, default_value_t = 60)]
        template_refresh_seconds: u64,

        /// Mine the provisional empty template returned immediately after a
        /// tip change instead of waiting for a full template.
        #[arg(long)]
        mine_provisional_empty_templates: bool,
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

    /// Run txblast locally (intended to run on remote nodes)
    TxblastLocal {
        /// RPC endpoint
        #[arg(long)]
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

        /// Network parameters to use when building txblast transactions
        #[arg(long)]
        network: Option<String>,

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

        /// Wallet birthday height for public-network Orchard scans
        #[arg(long)]
        wallet_birthday_height: Option<u32>,

        /// Expected runtime funding transaction id for shielded bootstrap diagnostics
        #[arg(long)]
        expected_runtime_funding_txid: Option<String>,
    },

    /// Fund runtime keys locally from the cached bootstrap treasury (intended to run on remote nodes)
    FundRuntimeKeysLocal {
        /// RPC endpoint
        #[arg(long)]
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
}

#[derive(Subcommand, Clone, Debug)]
enum ConfigCommand {
    /// Print mining.miner_address from a zebrad.toml. Empty output if unset.
    GetMinerAddress {
        /// Path to zebrad.toml
        path: String,
    },
    /// Write mining.miner_address. May be passed multiple --path values to
    /// keep zebrad.toml and zebrad.bootstrap.toml in sync atomically.
    SetMinerAddress {
        /// New miner_address to write.
        #[arg(long)]
        address: String,
        /// Path(s) to zebrad.toml-style files to update.
        #[arg(long)]
        path: Vec<String>,
    },
    /// Print network.testnet_parameters.genesis_hash (lowercased) from a config.
    GetGenesisHash {
        /// Path to zebrad.toml
        path: String,
    },
    /// Strip the optional `network.testnet_parameters.genesis_block_path`.
    StripGenesisBlockPath {
        /// Path to zebrad.toml
        path: String,
    },
    /// Render a bootstrap config (P2P-disabled) from an existing zebrad.toml.
    /// Writes to --out (defaults to <input>.bootstrap.toml).
    RenderBootstrap {
        /// Source zebrad.toml.
        path: String,
        /// Where to write the bootstrap config.
        #[arg(long)]
        out: Option<String>,
    },
}

#[derive(Subcommand, Clone, Debug)]
enum TxblastCommand {
    /// Manage public-network txblast wallet state
    Wallet {
        #[command(subcommand)]
        command: TxblastWalletCommand,
    },
    /// Manage public-network txblast deposits
    Deposit {
        #[command(subcommand)]
        command: TxblastDepositCommand,
    },
    /// Create an immutable public-network txblast funding and rate plan
    Plan {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// Target global bytes per public-network block
        #[arg(long, default_value_t = 1_000_000)]
        target_block_bytes: u64,

        /// Expected public-network block spacing in seconds
        #[arg(long, default_value_t = 75)]
        block_spacing_secs: u64,

        /// Planned run duration in seconds
        #[arg(long, default_value_t = 900)]
        duration_secs: u64,

        /// Comma-separated instance indices or "all"
        #[arg(long, default_value = "all")]
        nodes: String,

        /// Serialized lane-advance transaction size used for rate math
        #[arg(long, default_value_t = 3_000)]
        measured_tx_bytes: u64,

        /// Pause threshold to record for future mempool guardrails
        #[arg(long)]
        max_mempool_bytes: Option<u64>,

        /// Funding safety margin, for example 0.20
        #[arg(long, default_value_t = 0.20)]
        safety_margin: f64,

        /// RPC endpoint to auto-discover confirmed deposits
        #[arg(long)]
        rpc_endpoint: Option<String>,

        /// Record a plan even when imported deposits are insufficient
        #[arg(long)]
        allow_underfunded_plan: bool,

        /// Output JSON
        #[arg(long)]
        json: bool,
    },
    /// Fan deposits out into public-network hot keys and lane inventory
    Prepare {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// Plan id to use, default latest
        #[arg(long)]
        plan: Option<String>,

        /// Show what would happen without moving funds
        #[arg(long)]
        dry_run: bool,
    },
    /// Run public-network txblast with byte-budget guardrails
    Run {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// Plan id to use, default latest
        #[arg(long)]
        plan: Option<String>,

        /// Override target global bytes per public-network block
        #[arg(long)]
        target_block_bytes: Option<u64>,

        /// Advanced override for global byte budget
        #[arg(long)]
        max_global_bytes_per_sec: Option<u64>,

        /// Optional cap for any one node
        #[arg(long)]
        max_node_bytes_per_sec: Option<u64>,

        /// Maximum pending transactions per node
        #[arg(long)]
        max_pending_txs: Option<usize>,

        /// Maximum pending transaction bytes per node
        #[arg(long)]
        max_pending_bytes: Option<u64>,

        /// Pause when observed mempool bytes exceed this value
        #[arg(long)]
        max_mempool_bytes: Option<u64>,

        /// Number of recent blocks used for future feedback control
        #[arg(long)]
        feedback_window_blocks: Option<u64>,

        /// Trace directory for txblast JSONL files on remote nodes
        #[arg(long)]
        trace_dir: Option<String>,

        /// Required on mainnet because fees are burned
        #[arg(long)]
        mainnet_i_understand_fees: bool,
    },
    /// Stop public-network txblast agents without sweeping funds
    Stop {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// Plan id to use, default latest
        #[arg(long)]
        plan: Option<String>,

        /// Show what would happen
        #[arg(long)]
        dry_run: bool,
    },
    /// Show public-network txblast workload status
    Status {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// Plan id to use, default latest
        #[arg(long)]
        plan: Option<String>,

        /// Show what would happen
        #[arg(long)]
        dry_run: bool,
    },
    /// Fan funds back in and withdraw to an external transparent address
    Withdraw {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// Destination transparent address
        #[arg(long)]
        to: String,

        /// Amount in zats or "all"
        #[arg(long, default_value = "all")]
        amount: String,

        /// Show what would happen without moving funds
        #[arg(long)]
        dry_run: bool,

        /// Required on mainnet
        #[arg(long)]
        mainnet_i_understand_finality: bool,
    },
    /// Recover funds using local recovery data
    Recover {
        #[command(subcommand)]
        command: TxblastRecoverCommand,
    },
}

#[derive(Subcommand, Clone, Debug)]
enum TxblastWalletCommand {
    /// Create public-network txblast wallet and recovery files
    Init {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// Network for the wallet, default from config.json
        #[arg(long)]
        network: Option<String>,

        /// Wallet birthday height, default current RPC height when available
        #[arg(long)]
        birthday_height: Option<u32>,

        /// RPC endpoint to query for the default birthday height
        #[arg(long)]
        rpc_endpoint: Option<String>,

        /// Initial lane count per node
        #[arg(long, default_value_t = 100)]
        lanes_per_node: usize,

        /// Value for each lane note, in zatoshis
        #[arg(long, default_value_t = 30_000)]
        lane_value_zats: u64,

        /// Fanout width used by prepare-time fanout
        #[arg(long, default_value_t = 1)]
        fanout_width: usize,

        /// Required for mainnet wallet creation
        #[arg(long)]
        require_mainnet_confirmation: bool,

        /// Replace existing txblast wallet files
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand, Clone, Debug)]
enum TxblastDepositCommand {
    /// Print the control transparent deposit address
    Address {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// Output JSON
        #[arg(long)]
        json: bool,
    },
    /// Register a deposit outpoint or transaction manually
    Import {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// Deposit transaction id
        #[arg(long)]
        txid: String,

        /// Deposit output index
        #[arg(long)]
        vout: Option<u32>,

        /// Expected deposit amount in zatoshis
        #[arg(long)]
        amount_zats: Option<u64>,

        /// Expected destination address
        #[arg(long)]
        address: Option<String>,
    },
    /// Show deposit and spendability status
    Status {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// RPC endpoint to query, default localhost port from config
        #[arg(long)]
        rpc_endpoint: Option<String>,

        /// Confirmations required for spendability
        #[arg(long, default_value_t = 3)]
        confirmations: u32,

        /// Output JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Clone, Debug)]
enum TxblastRecoverCommand {
    /// Reconstruct recoverable inventory from local recovery data
    Inventory {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// Scan start height override
        #[arg(long)]
        from_height: Option<u32>,

        /// Output JSON
        #[arg(long)]
        json: bool,
    },
    /// Emergency sweep all recoverable funds to a transparent address
    Sweep {
        /// Override experiment directory for txblast wallet state
        #[arg(short = 'd', long)]
        directory: Option<String>,

        /// Destination transparent address
        #[arg(long)]
        to: String,

        /// Scan start height override
        #[arg(long)]
        from_height: Option<u32>,

        /// Show what would happen without moving funds
        #[arg(long)]
        dry_run: bool,

        /// Required on mainnet
        #[arg(long)]
        mainnet_i_understand_recovery: bool,
    },
}

impl Commands {
    /// Experiment directory associated with the command, when one applies.
    /// Local-only and pure compute commands (Mine, PoW simulation/bench, TxblastLocal,
    /// FundRuntimeKeysLocal, TxblastStatusLocal) intentionally return None.
    fn directory(&self) -> Option<&str> {
        match self {
            Commands::Init
            | Commands::Config { .. }
            | Commands::Mine { .. }
            | Commands::JoinBundle { .. }
            | Commands::PowSimulate { .. }
            | Commands::PowBench { .. }
            | Commands::PowSimulateMatrix { .. }
            | Commands::TxblastLocal { .. }
            | Commands::FundRuntimeKeysLocal { .. }
            | Commands::SeedLocal { .. }
            | Commands::TxblastStatusLocal { .. } => None,
            Commands::Genesis { directory, .. }
            | Commands::GenesisPublic { directory, .. }
            | Commands::LocalizeFleet { directory, .. }
            | Commands::Status { directory, .. }
            | Commands::Txblast { directory, .. } => Some(directory),
        }
    }
}

fn run_config_command(command: ConfigCommand) -> Result<()> {
    use anyhow::Context;
    match command {
        ConfigCommand::GetMinerAddress { path } => {
            let contents =
                std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
            if let Some(address) = zebra_config::read_miner_address(&contents)? {
                println!("{address}");
            }
        }
        ConfigCommand::SetMinerAddress { address, path } => {
            if path.is_empty() {
                anyhow::bail!("--path must be supplied at least once");
            }
            for p in &path {
                let contents =
                    std::fs::read_to_string(p).with_context(|| format!("reading {p}"))?;
                let updated = zebra_config::set_miner_address(&contents, &address)?;
                std::fs::write(p, updated).with_context(|| format!("writing {p}"))?;
            }
        }
        ConfigCommand::GetGenesisHash { path } => {
            let contents =
                std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
            if let Some(hash) = zebra_config::read_genesis_hash(&contents)? {
                println!("{hash}");
            }
        }
        ConfigCommand::StripGenesisBlockPath { path } => {
            let contents =
                std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
            let stripped = zebra_config::strip_genesis_block_path(&contents)?;
            std::fs::write(&path, stripped).with_context(|| format!("writing {path}"))?;
        }
        ConfigCommand::RenderBootstrap { path, out } => {
            let contents =
                std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
            let bootstrap = zebra_config::bootstrap_config_for_isolated_rpc(&contents)?;
            let dest = out.unwrap_or_else(|| {
                let p = std::path::Path::new(&path);
                let parent = p.parent().unwrap_or_else(|| std::path::Path::new("."));
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("zebrad");
                parent
                    .join(format!("{stem}.bootstrap.toml"))
                    .to_string_lossy()
                    .into_owned()
            });
            std::fs::write(&dest, bootstrap).with_context(|| format!("writing {dest}"))?;
        }
    }
    Ok(())
}

async fn run_txblast_command(command: TxblastCommand, directory: &str) -> Result<()> {
    match command {
        TxblastCommand::Wallet { command } => match command {
            TxblastWalletCommand::Init {
                directory: state_directory,
                network,
                birthday_height,
                rpc_endpoint,
                lanes_per_node,
                lane_value_zats,
                fanout_width,
                require_mainnet_confirmation,
                force,
            } => {
                let network = network.as_deref().map(str::parse).transpose()?;
                commands::txblast_public::wallet_init(
                    directory,
                    commands::txblast_public::WalletInitArgs {
                        directory: state_directory,
                        network,
                        birthday_height,
                        rpc_endpoint,
                        lanes_per_node,
                        lane_value_zats,
                        fanout_width,
                        require_mainnet_confirmation,
                        force,
                    },
                )
                .await?;
            }
        },
        TxblastCommand::Deposit { command } => match command {
            TxblastDepositCommand::Address {
                directory: state_directory,
                json,
            } => {
                commands::txblast_public::deposit_address(
                    directory,
                    commands::txblast_public::DepositAddressArgs {
                        directory: state_directory,
                        json,
                    },
                )?;
            }
            TxblastDepositCommand::Import {
                directory: state_directory,
                txid,
                vout,
                amount_zats,
                address,
            } => {
                commands::txblast_public::deposit_import(
                    directory,
                    commands::txblast_public::DepositImportArgs {
                        directory: state_directory,
                        txid,
                        vout,
                        amount_zats,
                        address,
                    },
                )?;
            }
            TxblastDepositCommand::Status {
                directory: state_directory,
                rpc_endpoint,
                confirmations,
                json,
            } => {
                commands::txblast_public::deposit_status(
                    directory,
                    commands::txblast_public::DepositStatusArgs {
                        directory: state_directory,
                        rpc_endpoint,
                        confirmations,
                        json,
                    },
                )
                .await?;
            }
        },
        TxblastCommand::Plan {
            directory: state_directory,
            target_block_bytes,
            block_spacing_secs,
            duration_secs,
            nodes,
            measured_tx_bytes,
            max_mempool_bytes,
            safety_margin,
            rpc_endpoint,
            allow_underfunded_plan,
            json,
        } => {
            commands::txblast_public::plan(
                directory,
                commands::txblast_public::PlanArgs {
                    directory: state_directory,
                    target_block_bytes,
                    block_spacing_secs,
                    duration_secs,
                    nodes,
                    measured_tx_bytes,
                    max_mempool_bytes,
                    safety_margin,
                    rpc_endpoint,
                    allow_underfunded_plan,
                    json,
                },
            )
            .await?;
        }
        TxblastCommand::Prepare {
            directory: state_directory,
            plan,
            dry_run,
        } => {
            commands::txblast_public::prepare(
                directory,
                commands::txblast_public::GuardedLifecycleArgs {
                    directory: state_directory,
                    plan,
                    dry_run,
                },
            )
            .await?;
        }
        TxblastCommand::Run {
            directory: state_directory,
            plan,
            target_block_bytes,
            max_global_bytes_per_sec,
            max_node_bytes_per_sec,
            max_pending_txs,
            max_pending_bytes,
            max_mempool_bytes,
            feedback_window_blocks,
            trace_dir,
            mainnet_i_understand_fees,
        } => {
            commands::txblast_public::run_public(
                directory,
                commands::txblast_public::PublicRunArgs {
                    directory: state_directory,
                    plan,
                    target_block_bytes,
                    max_global_bytes_per_sec,
                    max_node_bytes_per_sec,
                    max_pending_txs,
                    max_pending_bytes,
                    max_mempool_bytes,
                    feedback_window_blocks,
                    trace_dir,
                    mainnet_i_understand_fees,
                },
            )
            .await?;
        }
        TxblastCommand::Stop {
            directory: state_directory,
            plan,
            dry_run,
        } => {
            commands::txblast_public::stop(
                directory,
                commands::txblast_public::GuardedLifecycleArgs {
                    directory: state_directory,
                    plan,
                    dry_run,
                },
            )?;
        }
        TxblastCommand::Status {
            directory: state_directory,
            plan,
            dry_run,
        } => {
            commands::txblast_public::status(
                directory,
                commands::txblast_public::GuardedLifecycleArgs {
                    directory: state_directory,
                    plan,
                    dry_run,
                },
            )?;
        }
        TxblastCommand::Withdraw {
            directory: state_directory,
            to,
            amount,
            dry_run,
            mainnet_i_understand_finality,
        } => {
            commands::txblast_public::withdraw(
                directory,
                commands::txblast_public::WithdrawArgs {
                    directory: state_directory,
                    to,
                    amount,
                    dry_run,
                    mainnet_i_understand_finality,
                },
            )
            .await?;
        }
        TxblastCommand::Recover { command } => match command {
            TxblastRecoverCommand::Inventory {
                directory: state_directory,
                from_height,
                json,
            } => {
                commands::txblast_public::recover_inventory(
                    directory,
                    commands::txblast_public::RecoverInventoryArgs {
                        directory: state_directory,
                        from_height,
                        json,
                    },
                )
                .await?;
            }
            TxblastRecoverCommand::Sweep {
                directory: state_directory,
                to,
                from_height,
                dry_run,
                mainnet_i_understand_recovery,
            } => {
                commands::txblast_public::recover_sweep(
                    directory,
                    commands::txblast_public::RecoverSweepArgs {
                        directory: state_directory,
                        to,
                        from_height,
                        dry_run,
                        mainnet_i_understand_recovery,
                    },
                )
                .await?;
            }
        },
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Load .env files with override so they always win over shell env vars.
    // Priority (lowest -> highest): CWD, ancestor of experiment dir, experiment dir.
    let _ = dotenvy::dotenv_override();
    if let Some(dir) = cli.command.directory() {
        let anchor = std::path::Path::new(dir);
        if let Some(start) = anchor
            .canonicalize()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        {
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
        // Experiment directory .env wins over everything.
        let env_path = std::path::Path::new(dir).join(".env");
        let _ = dotenvy::from_path_override(&env_path);
    }

    match cli.command {
        Commands::Init => {
            commands::init::run()?;
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
            pow_fleet_discount,
            pow_sol_per_sec,
            directory,
        } => {
            let pow_calibration = commands::genesis::PowCalibrationCli {
                adjust_fraction: pow_adjust,
                fleet_discount: pow_fleet_discount,
                sol_per_sec: pow_sol_per_sec,
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
        Commands::GenesisPublic {
            zebrad_binary,
            kresko_binary,
            build_dir,
            scripts_dir,
            directory,
        } => {
            commands::genesis_public::run(
                &zebrad_binary,
                kresko_binary.as_deref(),
                &build_dir,
                &scripts_dir,
                &directory,
            )?;
        }
        Commands::LocalizeFleet {
            miner_nodes,
            directory,
        } => {
            commands::localize_fleet::run(&directory, miner_nodes)?;
        }
        Commands::SeedLocal {
            rpc_endpoint,
            genesis,
            premine,
        } => {
            commands::seed_local::run(&rpc_endpoint, &genesis, &premine).await?;
        }
        Commands::JoinBundle {
            run_dir,
            zebra_repo,
            zebra_release_tag,
            kresko_repo,
            kresko_release_tag,
            out,
        } => {
            commands::join_bundle::run(
                &run_dir,
                &zebra_repo,
                &zebra_release_tag,
                &kresko_repo,
                &kresko_release_tag,
                &out,
            )?;
        }
        Commands::Txblast { command, directory } => {
            run_txblast_command(command, &directory).await?;
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
        Commands::Config { command } => {
            run_config_command(command)?;
        }
        Commands::Mine {
            rpc_endpoint,
            zebrad_config,
            submit_stale_solutions,
            template_refresh_seconds,
            mine_provisional_empty_templates,
        } => {
            commands::mine::run_with(
                &rpc_endpoint,
                std::path::Path::new(&zebrad_config),
                commands::mine::MinerOptions {
                    submit_stale_solutions,
                    template_refresh_interval: (template_refresh_seconds > 0)
                        .then(|| std::time::Duration::from_secs(template_refresh_seconds)),
                    mine_provisional_empty_templates,
                    ..Default::default()
                },
            )
            .await?;
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
                "  mining_candidates={} ({:.3}/s)",
                result.mining_candidates, result.mining_candidates_per_sec,
            );
            println!(
                "  matrix input: --sol-per-sec {}={:.9}",
                result.equihash_params, result.mining_candidates_per_sec,
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
            network,
            trace_enable: _,
            skip_funding,
            trace_dir,
            funded_key_path,
            wallet_birthday_height,
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
                network.as_deref().map(str::parse).transpose()?,
                skip_funding,
                trace_dir.as_deref(),
                funded_key_path.as_deref(),
                expected_runtime_funding_txid.as_deref(),
                wallet_birthday_height,
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
    }

    Ok(())
}
