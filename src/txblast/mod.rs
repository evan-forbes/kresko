pub(crate) mod orchard;
pub mod rpc;
pub mod shielded;
pub mod transparent;

use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::{OrchardTxblastConfig, TxType};

#[derive(Clone, Debug)]
pub struct OrchardBlastRuntimeConfig {
    pub lane_premine: OrchardTxblastConfig,
    pub max_in_flight: usize,
    pub target_ready_lanes: usize,
    pub lane_low_watermark: usize,
    pub fanout_max_in_flight: usize,
    pub progress_interval: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct TxblastTraceConfig {
    pub enabled: bool,
    pub directory: Option<PathBuf>,
}

impl TxblastTraceConfig {
    pub fn from_args(trace_enable: bool, trace_dir: Option<&str>) -> Self {
        let enabled =
            trace_enable || trace_dir.is_some() || env_flag_enabled("KRESKO_TXBLAST_TRACE_ENABLE");
        let directory = trace_dir
            .filter(|dir| !dir.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("KRESKO_TRACE_DIR").map(PathBuf::from));

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

        let progress_interval_secs = progress_interval_secs.unwrap_or(5);
        if progress_interval_secs == 0 {
            anyhow::bail!("orchard progress interval must be greater than 0");
        }

        Ok(Self {
            lane_premine: premine,
            max_in_flight,
            target_ready_lanes,
            lane_low_watermark,
            fanout_max_in_flight,
            progress_interval: Duration::from_secs(progress_interval_secs),
        })
    }
}

/// Run the transaction blaster locally (called on remote nodes).
pub async fn run_local(
    rpc_endpoint: &str,
    tx_type: TxType,
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
    orchard_progress_interval_secs: Option<u64>,
    trace_enable: bool,
    trace_dir: Option<&str>,
    funded_key_path: Option<&str>,
    expected_runtime_funding_txid: Option<&str>,
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
    let orchard_runtime = OrchardBlastRuntimeConfig::from_parts(
        orchard_premine,
        orchard_max_in_flight,
        orchard_target_ready_lanes,
        orchard_lane_low_watermark,
        orchard_fanout_max_in_flight,
        orchard_progress_interval_secs,
    )?;
    let trace_config = TxblastTraceConfig::from_args(trace_enable, trace_dir);

    println!(
        "Starting txblast (endpoint={rpc_endpoint}, type={tx_type}, rate={rate}/s, amount={amount})"
    );

    let client = rpc::ZebraRpcClient::new(rpc_endpoint);

    // Verify connection
    let info = client.get_blockchain_info().await?;
    println!(
        "Connected to zebrad: chain={}, blocks={}",
        info["chain"].as_str().unwrap_or("unknown"),
        info["blocks"].as_u64().unwrap_or(0),
    );

    let (funded_key, key_path) = transparent::load_funded_key(funded_key_path)?;
    println!(
        "Loaded funded key '{}' (address={}) from {}",
        funded_key.name,
        funded_key.address,
        key_path.display()
    );

    match tx_type {
        TxType::Transparent => transparent::run(&client, &funded_key, rate, amount).await,
        TxType::Shielded => {
            shielded::run(
                &client,
                &funded_key,
                rate,
                amount,
                &orchard_runtime,
                &trace_config,
                expected_runtime_funding_txid,
            )
            .await
        }
        TxType::Both => {
            let client2 = rpc::ZebraRpcClient::new(rpc_endpoint);
            let key2 = funded_key.clone();
            let t_rate = std::cmp::max(rate / 2, 1);
            let s_rate = std::cmp::max(rate / 2, 1);
            tokio::try_join!(
                transparent::run(&client, &funded_key, t_rate, amount),
                shielded::run(
                    &client2,
                    &key2,
                    s_rate,
                    amount,
                    &orchard_runtime,
                    &trace_config,
                    expected_runtime_funding_txid,
                ),
            )?;
            Ok(())
        }
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
