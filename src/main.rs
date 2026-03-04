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
    /// Generate .env template for a provider
    InitEnv {
        /// Cloud provider (digitalocean, googlecloud)
        #[arg(long, default_value = "digitalocean")]
        provider: String,
    },

    /// Initialize experiment directory structure and default configs
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
    },

    /// Add nodes to the experiment config
    Add {
        /// Node type (validator)
        #[arg(short = 't', long, default_value = "validator")]
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

        /// Path to txblast binary (optional, defaults to kresko binary)
        #[arg(long)]
        txblast_binary: Option<String>,

        /// Build directory name
        #[arg(long, default_value = "build")]
        build_dir: String,

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

        /// Continue even if some validators fail
        #[arg(long)]
        ignore_failed_validators: bool,

        /// Experiment directory
        #[arg(short = 'd', long, default_value = ".")]
        directory: String,
    },

    /// Query node status (block heights, sync progress)
    Status {
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
        /// Comma-separated validator indices or "all"
        #[arg(long, default_value = "all")]
        validators: String,

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

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    match cli.command {
        Commands::InitEnv { provider } => {
            commands::init_env::run(&provider)?;
        }
        Commands::Init {
            chain_id,
            experiment,
            provider,
            ssh_pub_key_path,
            ssh_key_name,
        } => {
            commands::init::run(
                &chain_id,
                &experiment,
                &provider,
                ssh_pub_key_path,
                ssh_key_name,
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
            txblast_binary,
            build_dir,
            directory,
        } => {
            commands::genesis::run(
                &zebrad_binary,
                txblast_binary.as_deref(),
                &build_dir,
                &directory,
            )?;
        }
        Commands::Deploy {
            ssh_key_path,
            direct_payload_upload,
            workers,
            ignore_failed_validators,
            directory,
        } => {
            commands::deploy::run(
                ssh_key_path.as_deref(),
                direct_payload_upload,
                workers,
                ignore_failed_validators,
                &directory,
            )
            .await?;
        }
        Commands::Status { directory } => {
            commands::status::run(&directory).await?;
        }
        Commands::List { directory } => {
            commands::list::run(&directory).await?;
        }
        Commands::Txblast {
            instances,
            tx_type,
            rate,
            amount,
            directory,
        } => {
            let tx_type: config::TxType = tx_type.parse()?;
            commands::txblast::run(&instances, tx_type, rate, amount, &directory).await?;
        }
        Commands::TxblastLocal {
            rpc_endpoint,
            tx_type,
            rate,
            amount,
        } => {
            let tx_type: config::TxType = tx_type.parse()?;
            txblast::run_local(&rpc_endpoint, tx_type, rate, amount).await?;
        }
        Commands::KillSession {
            session,
            timeout,
            directory,
        } => {
            commands::kill_session::run(&session, timeout, &directory).await?;
        }
        Commands::Download {
            nodes,
            workers,
            no_compress,
            directory,
        } => {
            commands::download::run(&nodes, workers, no_compress, &directory).await?;
        }
        Commands::UploadData { directory } => {
            commands::upload_data::run(&directory).await?;
        }
        Commands::Reset {
            validators,
            workers,
            directory,
        } => {
            commands::reset::run(&validators, workers, &directory).await?;
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
