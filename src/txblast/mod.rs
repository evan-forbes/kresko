pub(crate) mod orchard;
pub mod rpc;
pub mod shielded;
pub mod transparent;

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zcash_protocol::consensus::{self, BlockHeight, NetworkType, NetworkUpgrade};

use crate::config::{NetworkKind, OrchardTxblastConfig};

const AUTO_RUNTIME_FUNDING_CONFIRM_TIMEOUT_SECS: u64 = 600;

#[derive(Clone, Debug)]
pub struct OrchardBlastRuntimeConfig {
    pub lane_premine: OrchardTxblastConfig,
    pub network_params: TxblastNetworkParams,
    pub max_in_flight: usize,
    pub target_ready_lanes: usize,
    pub lane_low_watermark: usize,
    pub fanout_max_in_flight: usize,
    pub proving_workers: usize,
    pub progress_interval: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxblastNetworkParams {
    LocalGenesis,
    PublicTestnet,
    Mainnet,
}

impl TxblastNetworkParams {
    pub fn from_network_kind(network_kind: NetworkKind) -> Self {
        match network_kind {
            NetworkKind::LocalGenesis => Self::LocalGenesis,
            NetworkKind::PublicTestnet => Self::PublicTestnet,
            NetworkKind::Mainnet => Self::Mainnet,
        }
    }
}

impl consensus::Parameters for TxblastNetworkParams {
    fn network_type(&self) -> NetworkType {
        match self {
            Self::Mainnet => NetworkType::Main,
            Self::LocalGenesis | Self::PublicTestnet => NetworkType::Test,
        }
    }

    fn activation_height(&self, nu: NetworkUpgrade) -> Option<BlockHeight> {
        match self {
            Self::LocalGenesis => match nu {
                NetworkUpgrade::Nu6_1 => None,
                _ => Some(BlockHeight::from_u32(1)),
            },
            Self::PublicTestnet => consensus::Network::TestNetwork.activation_height(nu),
            Self::Mainnet => consensus::Network::MainNetwork.activation_height(nu),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TxblastTraceConfig {
    pub enabled: bool,
    pub directory: Option<PathBuf>,
}

impl TxblastTraceConfig {
    pub fn from_args(trace_dir: Option<&str>) -> Self {
        let enabled = !env_flag_enabled("KRESKO_TXBLAST_TRACE_DISABLE");
        let directory = trace_dir
            .filter(|dir| !dir.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("KRESKO_TRACE_DIR").map(PathBuf::from))
            .or_else(|| Some(PathBuf::from("/root/.cache/kresko/txblast-traces")));

        Self { enabled, directory }
    }
}

impl OrchardBlastRuntimeConfig {
    pub fn from_parts(
        premine: OrchardTxblastConfig,
        max_in_flight: Option<usize>,
        target_ready_lanes: Option<usize>,
        lane_low_watermark: Option<usize>,
        fanout_max_in_flight: Option<usize>,
        proving_workers: Option<usize>,
        progress_interval_secs: Option<u64>,
    ) -> Result<Self> {
        Self::from_parts_with_network(
            premine,
            TxblastNetworkParams::LocalGenesis,
            max_in_flight,
            target_ready_lanes,
            lane_low_watermark,
            fanout_max_in_flight,
            proving_workers,
            progress_interval_secs,
        )
    }

    pub fn from_parts_with_network(
        premine: OrchardTxblastConfig,
        network_params: TxblastNetworkParams,
        max_in_flight: Option<usize>,
        target_ready_lanes: Option<usize>,
        lane_low_watermark: Option<usize>,
        fanout_max_in_flight: Option<usize>,
        proving_workers: Option<usize>,
        progress_interval_secs: Option<u64>,
    ) -> Result<Self> {
        if premine.lanes_per_miner == 0 {
            anyhow::bail!("orchard lanes per miner must be greater than 0");
        }
        if premine.lane_value_zats == 0 {
            anyhow::bail!("orchard lane value must be greater than 0");
        }
        if premine.fanout_outputs == 0 {
            anyhow::bail!("orchard fanout outputs must be greater than 0");
        }

        let target_ready_lanes = target_ready_lanes.unwrap_or(premine.lanes_per_miner);
        if target_ready_lanes == 0 {
            anyhow::bail!("orchard target ready lanes must be greater than 0");
        }

        let lane_low_watermark = lane_low_watermark
            .unwrap_or(std::cmp::max(1, (target_ready_lanes.saturating_mul(3)) / 4));
        if lane_low_watermark > target_ready_lanes {
            anyhow::bail!("orchard lane low watermark must be <= target ready lanes");
        }

        let max_in_flight =
            max_in_flight.unwrap_or(std::cmp::max(8, std::cmp::min(target_ready_lanes, 256)));
        if max_in_flight == 0 {
            anyhow::bail!("orchard max in flight must be greater than 0");
        }

        let fanout_max_in_flight =
            fanout_max_in_flight.unwrap_or(std::cmp::max(4, std::cmp::min(max_in_flight / 4, 32)));
        if fanout_max_in_flight == 0 {
            anyhow::bail!("orchard fanout max in flight must be greater than 0");
        }

        let proving_workers = proving_workers.unwrap_or(1);
        if proving_workers == 0 {
            anyhow::bail!("orchard proving workers must be greater than 0");
        }

        let progress_interval_secs = progress_interval_secs.unwrap_or(5);
        if progress_interval_secs == 0 {
            anyhow::bail!("orchard progress interval must be greater than 0");
        }

        Ok(Self {
            lane_premine: premine,
            network_params,
            max_in_flight,
            target_ready_lanes,
            lane_low_watermark,
            fanout_max_in_flight,
            proving_workers,
            progress_interval: Duration::from_secs(progress_interval_secs),
        })
    }
}

/// Run the transaction blaster locally (called on remote nodes).
pub async fn run_local(
    rpc_endpoint: &str,
    rate: u64,
    amount: f64,
    orchard_lanes_per_miner: Option<usize>,
    orchard_lane_value_zats: Option<u64>,
    orchard_fanout_source_value_zats: Option<u64>,
    orchard_fanout_outputs: Option<usize>,
    orchard_max_in_flight: Option<usize>,
    orchard_target_ready_lanes: Option<usize>,
    orchard_lane_low_watermark: Option<usize>,
    orchard_fanout_max_in_flight: Option<usize>,
    orchard_proving_workers: Option<usize>,
    orchard_progress_interval_secs: Option<u64>,
    network: Option<NetworkKind>,
    skip_funding: bool,
    trace_dir: Option<&str>,
    funded_key_path: Option<&str>,
    expected_runtime_funding_txid: Option<&str>,
    wallet_birthday_height: Option<u32>,
) -> Result<()> {
    let orchard_premine = OrchardTxblastConfig {
        lanes_per_miner: orchard_lanes_per_miner
            .unwrap_or_else(|| OrchardTxblastConfig::default().lanes_per_miner),
        lane_value_zats: orchard_lane_value_zats
            .unwrap_or_else(|| OrchardTxblastConfig::default().lane_value_zats),
        fanout_source_value_zats: orchard_fanout_source_value_zats
            .unwrap_or_else(|| OrchardTxblastConfig::default().fanout_source_value_zats),
        fanout_outputs: orchard_fanout_outputs
            .unwrap_or_else(|| OrchardTxblastConfig::default().fanout_outputs),
    };
    let network_params =
        TxblastNetworkParams::from_network_kind(network.unwrap_or(NetworkKind::LocalGenesis));
    let orchard_runtime = OrchardBlastRuntimeConfig::from_parts_with_network(
        orchard_premine,
        network_params,
        orchard_max_in_flight,
        orchard_target_ready_lanes,
        orchard_lane_low_watermark,
        orchard_fanout_max_in_flight,
        orchard_proving_workers,
        orchard_progress_interval_secs,
    )?;
    let trace_config = TxblastTraceConfig::from_args(trace_dir);

    println!("Starting txblast (endpoint={rpc_endpoint}, rate={rate}/s, amount={amount})");

    let client = rpc::ZebraRpcClient::new(rpc_endpoint);

    // Verify connection
    let info = client.get_blockchain_info().await?;
    println!(
        "Connected to zebrad: chain={}, blocks={}",
        info["chain"].as_str().unwrap_or("unknown"),
        info["blocks"].as_u64().unwrap_or(0),
    );

    if skip_funding {
        println!("Skipping cached runtime funding verification before txblast start.");
    } else if network_params != TxblastNetworkParams::LocalGenesis {
        println!(
            "Skipping cached local-genesis runtime funding verification on {:?}.",
            network_params
        );
    } else {
        maybe_prepare_cached_runtime_funding(
            rpc_endpoint,
            &orchard_runtime,
            expected_runtime_funding_txid,
        )
        .await?;
    }

    let (funded_key, key_path) = transparent::load_funded_key(funded_key_path)?;
    println!(
        "Loaded funded key '{}' (address={}) from {}",
        funded_key.name,
        funded_key.address,
        key_path.display()
    );

    shielded::run(
        &client,
        &funded_key,
        rate,
        amount,
        &orchard_runtime,
        &trace_config,
        expected_runtime_funding_txid,
        wallet_birthday_height,
    )
    .await
}

async fn maybe_prepare_cached_runtime_funding(
    rpc_endpoint: &str,
    orchard_runtime: &OrchardBlastRuntimeConfig,
    expected_runtime_funding_txid: Option<&str>,
) -> Result<()> {
    let Some(local_genesis_dir) = cached_bootstrap_local_genesis_dir() else {
        return Ok(());
    };

    let minimum_recipient_zats = orchard::min_treasury_reseed_value(orchard_runtime);
    println!(
        "Cached bootstrap detected at {}; verifying runtime funding before shielded txblast (minimum per recipient: {} zats)",
        local_genesis_dir, minimum_recipient_zats,
    );
    crate::commands::fund_runtime_keys::ensure_local_runtime_funding(
        rpc_endpoint,
        &local_genesis_dir,
        minimum_recipient_zats,
        AUTO_RUNTIME_FUNDING_CONFIRM_TIMEOUT_SECS,
        expected_runtime_funding_txid,
    )
    .await
}

fn cached_bootstrap_local_genesis_dir() -> Option<String> {
    let from_env = std::env::var("KRESKO_LOCAL_GENESIS_DIR")
        .ok()
        .filter(|dir| !dir.trim().is_empty());
    let candidate = from_env.or_else(|| {
        let default = "/root/payload/local_genesis";
        Path::new(default).exists().then(|| default.to_owned())
    })?;
    let candidate_path = Path::new(&candidate);
    if candidate_path.join("treasury_key.json").exists()
        && candidate_path.join("funded_keys.json").exists()
    {
        Some(candidate)
    } else {
        None
    }
}

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_protocol::consensus::Parameters;

    #[test]
    fn txblast_network_params_use_public_activation_heights() {
        assert_eq!(
            TxblastNetworkParams::Mainnet.network_type(),
            NetworkType::Main
        );
        assert_eq!(
            TxblastNetworkParams::Mainnet.activation_height(NetworkUpgrade::Nu6_1),
            Some(BlockHeight::from_u32(3_146_400))
        );
        assert_eq!(
            TxblastNetworkParams::PublicTestnet.activation_height(NetworkUpgrade::Nu6_1),
            Some(BlockHeight::from_u32(3_536_500))
        );
    }

    #[test]
    fn local_genesis_params_keep_all_stable_upgrades_at_height_one() {
        assert_eq!(
            TxblastNetworkParams::LocalGenesis.activation_height(NetworkUpgrade::Nu6),
            Some(BlockHeight::from_u32(1))
        );
        assert_eq!(
            TxblastNetworkParams::LocalGenesis.activation_height(NetworkUpgrade::Nu6_1),
            None
        );
    }
}
