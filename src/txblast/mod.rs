pub mod rpc;
pub mod shielded;
pub mod transparent;

use anyhow::Result;

use crate::config::TxType;

/// Run the transaction blaster locally (called on remote nodes).
pub async fn run_local(
    rpc_endpoint: &str,
    tx_type: TxType,
    rate: u64,
    amount: f64,
    funded_key_path: Option<&str>,
) -> Result<()> {
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
        TxType::Shielded => shielded::run(&client, &funded_key, rate, amount).await,
        TxType::Both => {
            let client2 = rpc::ZebraRpcClient::new(rpc_endpoint);
            let key2 = funded_key.clone();
            let t_rate = std::cmp::max(rate / 2, 1);
            let s_rate = std::cmp::max(rate / 2, 1);
            tokio::try_join!(
                transparent::run(&client, &funded_key, t_rate, amount),
                shielded::run(&client2, &key2, s_rate, amount),
            )?;
            Ok(())
        }
    }
}
