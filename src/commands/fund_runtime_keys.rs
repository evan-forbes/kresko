use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use zebra_chain::{
    amount::{Amount, NonNegative},
    transparent,
};

use crate::config::{
    Config, LocalGenesisBootstrapMode, LocalGenesisFundedKey, resolve_value, shellexpand,
};
use crate::ssh;
use crate::txblast::OrchardBlastRuntimeConfig;
use crate::txblast::orchard::min_treasury_reseed_value;
use crate::txblast::rpc::ZebraRpcClient;
use crate::txblast::transparent::{
    BASE_FEE_ZATS, FundedKey, build_sign_and_send_outputs, load_funded_key, rpc_utxo_to_spendable,
};

const COINBASE_MATURITY: u32 = 100;
const DEFAULT_CONFIRM_TIMEOUT_SECS: u64 = 600;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub async fn run(directory: &str) -> Result<()> {
    let dir = Path::new(directory);
    let config = Config::load(dir)?;
    let local_genesis = config
        .local_genesis
        .as_ref()
        .context("missing local_genesis config; run 'kresko genesis' first")?;

    if local_genesis.bootstrap_mode != LocalGenesisBootstrapMode::Cached {
        println!("Runtime funding skipped: bootstrap mode is generated.");
        return Ok(());
    }

    let runtime = OrchardBlastRuntimeConfig::from_parts(
        config.orchard_txblast.clone(),
        None,
        None,
        None,
        None,
        None,
    )?;
    let minimum_recipient_zats = min_treasury_reseed_value(&runtime);

    let key = resolve_value(None, "KRESKO_SSH_KEY_PATH", &config.ssh_key_path);
    let key = shellexpand(&key);
    let operator = config
        .miners
        .iter()
        .find(|inst| inst.public_ip != "TBD")
        .context("no active miners with assigned IPs")?;

    println!(
        "Funding runtime keys from cached treasury via {} (minimum per recipient: {} zats)...",
        operator.name, minimum_recipient_zats
    );

    let remote_command = format!(
        "bash -lc 'source /root/payload/vars.sh && kresko fund-runtime-keys-local \
            --rpc-endpoint http://localhost:18232 \
            --local-genesis-dir /root/payload/local_genesis \
            --minimum-recipient-zats {minimum_recipient_zats} \
            --confirm-timeout-secs {DEFAULT_CONFIRM_TIMEOUT_SECS}'"
    );
    let output = ssh::ssh_exec_timeout(
        &operator.public_ip,
        &key,
        &remote_command,
        Duration::from_secs(DEFAULT_CONFIRM_TIMEOUT_SECS + 60),
    )
    .await?;

    if !output.trim().is_empty() {
        print!("{output}");
    }

    Ok(())
}

pub async fn run_local(
    rpc_endpoint: &str,
    local_genesis_dir: &str,
    minimum_recipient_zats: u64,
    confirm_timeout_secs: u64,
) -> Result<()> {
    if minimum_recipient_zats == 0 {
        anyhow::bail!("--minimum-recipient-zats must be greater than 0");
    }

    let local_genesis_dir = PathBuf::from(local_genesis_dir);
    let treasury_key_path = local_genesis_dir.join("treasury_key.json");
    let funded_keys_path = local_genesis_dir.join("funded_keys.json");

    let (treasury_key, _) = load_funded_key(Some(
        treasury_key_path
            .to_str()
            .context("treasury key path is not valid UTF-8")?,
    ))?;
    let runtime_keys: Vec<LocalGenesisFundedKey> = serde_json::from_slice(
        &std::fs::read(&funded_keys_path)
            .with_context(|| format!("failed to read {}", funded_keys_path.display()))?,
    )
    .with_context(|| format!("failed to parse {}", funded_keys_path.display()))?;

    if runtime_keys.is_empty() {
        anyhow::bail!(
            "no runtime funded keys found in {}",
            funded_keys_path.display()
        );
    }

    let client = ZebraRpcClient::new(rpc_endpoint);
    let mut coinbase_cache = HashMap::new();
    let mut missing_keys = Vec::new();

    for key in runtime_keys {
        if runtime_key_already_funded(&client, &key, &mut coinbase_cache).await? {
            println!(
                "[fund-runtime-keys] {} already has confirmed non-coinbase funds; skipping",
                key.name
            );
            continue;
        }
        missing_keys.push(key);
    }

    if missing_keys.is_empty() {
        println!("[fund-runtime-keys] all runtime keys are already funded");
        return Ok(());
    }

    let current_height = client.get_block_count().await?;
    let treasury_utxo =
        select_spendable_treasury_utxo(&client, &treasury_key, current_height, &mut coinbase_cache)
            .await?;

    let input_value = u64::from(treasury_utxo.output.value);
    if input_value <= BASE_FEE_ZATS {
        anyhow::bail!("treasury UTXO value {input_value} is not enough to pay the funding fee");
    }

    let spendable_after_fee = input_value - BASE_FEE_ZATS;
    let recipient_count = missing_keys.len() as u64;
    let per_recipient_zats = spendable_after_fee / recipient_count;

    if per_recipient_zats < minimum_recipient_zats {
        anyhow::bail!(
            "cached treasury cannot fund {} runtime keys: {} zats available after fee, {} zats each, minimum required is {}",
            missing_keys.len(),
            spendable_after_fee,
            per_recipient_zats,
            minimum_recipient_zats,
        );
    }

    let mut outputs = Vec::with_capacity(missing_keys.len() + 1);
    for key in &missing_keys {
        outputs.push(output_for_address(&key.address, per_recipient_zats)?);
    }

    let distributed = per_recipient_zats.saturating_mul(recipient_count);
    let change_value = spendable_after_fee.saturating_sub(distributed);
    if change_value > 0 {
        outputs.push(output_for_address(
            &treasury_key.address.to_string(),
            change_value,
        )?);
    }

    let (txid, _) =
        build_sign_and_send_outputs(&client, &treasury_key, &treasury_utxo, outputs).await?;
    println!(
        "[fund-runtime-keys] submitted funding transaction {} to {} runtime keys ({} zats each)",
        txid,
        missing_keys.len(),
        per_recipient_zats
    );

    wait_for_runtime_funding(
        &client,
        &missing_keys,
        &txid.to_string(),
        Duration::from_secs(confirm_timeout_secs),
    )
    .await?;

    println!("[fund-runtime-keys] runtime keys confirmed");
    Ok(())
}

async fn runtime_key_already_funded(
    client: &ZebraRpcClient,
    key: &LocalGenesisFundedKey,
    coinbase_cache: &mut HashMap<String, bool>,
) -> Result<bool> {
    let utxos = client.get_address_utxos(&key.address).await?;
    for utxo in utxos {
        if !is_coinbase_transaction(client, &utxo.txid, coinbase_cache).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn select_spendable_treasury_utxo(
    client: &ZebraRpcClient,
    treasury_key: &FundedKey,
    current_height: u32,
    coinbase_cache: &mut HashMap<String, bool>,
) -> Result<crate::txblast::transparent::SpendableUtxo> {
    let utxos = client
        .get_address_utxos(&treasury_key.address.to_string())
        .await?;
    let mut best = None;

    for utxo in utxos {
        let is_coinbase = is_coinbase_transaction(client, &utxo.txid, coinbase_cache).await?;
        if is_coinbase && current_height < utxo.height.saturating_add(COINBASE_MATURITY) {
            continue;
        }

        let txid = zebra_chain::transaction::Hash::from_str(&utxo.txid)
            .with_context(|| format!("invalid txid in getaddressutxos: {}", utxo.txid))?;
        let Some(spendable) = rpc_utxo_to_spendable(utxo, txid)? else {
            continue;
        };

        let value = u64::from(spendable.output.value);
        if best
            .as_ref()
            .map(|candidate: &crate::txblast::transparent::SpendableUtxo| {
                u64::from(candidate.output.value) < value
            })
            .unwrap_or(true)
        {
            best = Some(spendable);
        }
    }

    best.context("no spendable treasury UTXO found; make sure the cached bootstrap chain was loaded and miners are running")
}

fn output_for_address(address: &str, value_zats: u64) -> Result<transparent::Output> {
    let address = transparent::Address::from_str(address)
        .with_context(|| format!("invalid address: {address}"))?;
    let value = Amount::<NonNegative>::try_from(value_zats)
        .context("output amount does not fit in Amount")?;
    let output = transparent::Output::new(value, address.script());
    if output.is_dust() {
        anyhow::bail!(
            "funding output for {} would be dust ({value_zats} zats)",
            address
        );
    }
    Ok(output)
}

async fn wait_for_runtime_funding(
    client: &ZebraRpcClient,
    runtime_keys: &[LocalGenesisFundedKey],
    expected_txid: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;

    loop {
        let mut confirmed = 0usize;
        for key in runtime_keys {
            let utxos = client.get_address_utxos(&key.address).await?;
            if utxos.iter().any(|utxo| utxo.txid == expected_txid) {
                confirmed += 1;
            }
        }

        if confirmed == runtime_keys.len() {
            return Ok(());
        }

        if Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for funding transaction {} to confirm on all runtime keys (confirmed {}/{})",
                expected_txid,
                confirmed,
                runtime_keys.len(),
            );
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn is_coinbase_transaction(
    client: &ZebraRpcClient,
    txid: &str,
    coinbase_cache: &mut HashMap<String, bool>,
) -> Result<bool> {
    if let Some(is_coinbase) = coinbase_cache.get(txid) {
        return Ok(*is_coinbase);
    }

    let tx = client.get_raw_transaction_verbose(txid).await?;
    let is_coinbase = tx.vin.first().is_some_and(|vin| vin.coinbase.is_some());
    coinbase_cache.insert(txid.to_owned(), is_coinbase);
    Ok(is_coinbase)
}
