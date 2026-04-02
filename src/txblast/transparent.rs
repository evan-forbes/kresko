use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use secp256k1::{Message, PublicKey, Secp256k1, SecretKey};
use zebra_chain::{
    amount::{Amount, NonNegative},
    block::Height,
    parameters::NetworkUpgrade,
    serialization::ZcashSerialize,
    transaction::{Hash, HashType, LockTime, Transaction},
    transparent,
};

use crate::config::LocalGenesisFundedKey;

use super::rpc::{AddressUtxo, ZebraRpcClient};

const BASE_FEE_ZATS: u64 = 10_000;
const MAX_UNCONFIRMED_ANCESTOR_DEPTH: u32 = 20;
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(crate) struct FundedKey {
    pub name: String,
    pub address: transparent::Address,
    pub secret_key: SecretKey,
    pub public_key: PublicKey,
}

#[derive(Clone)]
pub(crate) struct SpendableUtxo {
    pub(crate) outpoint: transparent::OutPoint,
    pub(crate) output: transparent::Output,
    pub(crate) lineage_depth: u32,
}

#[derive(Default)]
pub(crate) struct RefreshStats {
    pub(crate) coinbase_utxos: usize,
}

pub(crate) fn load_funded_key(explicit_path: Option<&str>) -> Result<(FundedKey, PathBuf)> {
    let path = resolve_funded_key_path(explicit_path)?;
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read funded key file {}", path.display()))?;

    let raw: LocalGenesisFundedKey =
        serde_json::from_str(&data).context("failed to parse funded key json")?;

    let key_bytes =
        hex::decode(&raw.secret_key_hex).context("funded key secret_key_hex is not valid hex")?;
    if key_bytes.len() != 32 {
        anyhow::bail!(
            "funded key secret_key_hex must decode to 32 bytes, got {}",
            key_bytes.len()
        );
    }

    let secret_key =
        SecretKey::from_slice(&key_bytes).context("funded key secret_key_hex is invalid")?;
    let secp = Secp256k1::new();
    let public_key = PublicKey::from_secret_key(&secp, &secret_key);

    if !raw.public_key_hex.is_empty() {
        let expected = hex::decode(&raw.public_key_hex)
            .context("funded key public_key_hex is not valid hex")?;
        if expected != public_key.serialize() {
            anyhow::bail!(
                "funded key file is inconsistent: public_key_hex does not match secret_key_hex"
            );
        }
    }

    let address =
        transparent::Address::from_str(&raw.address).context("funded key address is invalid")?;

    Ok((
        FundedKey {
            name: raw.name,
            address,
            secret_key,
            public_key,
        },
        path,
    ))
}

fn resolve_funded_key_path(explicit_path: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = explicit_path {
        return Ok(PathBuf::from(path));
    }

    if let Ok(path) = std::env::var("KRESKO_FUNDED_KEY_PATH") {
        if !path.trim().is_empty() {
            return Ok(PathBuf::from(path));
        }
    }

    let config_path = PathBuf::from("/root/.config/funded_key.json");
    if config_path.exists() {
        return Ok(config_path);
    }

    if let Some(parsed_hostname) = detect_parsed_hostname() {
        let payload_path =
            PathBuf::from(format!("/root/payload/{parsed_hostname}/funded_key.json"));
        if payload_path.exists() {
            return Ok(payload_path);
        }
    }

    anyhow::bail!(
        "could not locate funded key file. pass --funded-key-path, or set KRESKO_FUNDED_KEY_PATH"
    )
}

fn detect_parsed_hostname() -> Option<String> {
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_string())
        })?;

    let parts: Vec<&str> = hostname.split('-').collect();
    if parts.len() >= 2 {
        Some(format!("{}-{}", parts[0], parts[1]))
    } else {
        Some(hostname)
    }
}

pub async fn run(client: &ZebraRpcClient, key: &FundedKey, rate: u64, amount: f64) -> Result<()> {
    if rate == 0 {
        anyhow::bail!("--rate must be greater than 0")
    }

    let amount_zats = amount_to_zatoshis(amount)?;
    let min_required = amount_zats + BASE_FEE_ZATS;

    let mut utxos = VecDeque::new();
    let mut coinbase_tx_cache = HashMap::new();
    let initial_refresh =
        refresh_chain_utxos(client, key, &mut utxos, &mut coinbase_tx_cache).await?;
    if utxos.is_empty() {
        if initial_refresh.coinbase_utxos > 0 {
            anyhow::bail!(
                "found {} transparent UTXOs for {}, but none are spendable by transparent txblast because they are coinbase outputs. on testnet/mainnet, coinbase spends must have only shielded outputs. fund this key with non-coinbase transparent UTXOs before running txblast",
                initial_refresh.coinbase_utxos,
                key.address
            );
        }

        anyhow::bail!(
            "no spendable transparent UTXOs found for {}. make sure premine blocks were loaded",
            key.address
        );
    }

    let interval = Duration::from_secs_f64(1.0 / rate as f64);
    let mut ticker = tokio::time::interval(interval);
    let mut tx_count: u64 = 0;
    let mut err_count: u64 = 0;
    let mut last_refresh = tokio::time::Instant::now();

    loop {
        ticker.tick().await;

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            if let Err(e) =
                refresh_chain_utxos(client, key, &mut utxos, &mut coinbase_tx_cache).await
            {
                eprintln!("[warn] chain UTXO refresh failed: {e}");
            }
            last_refresh = tokio::time::Instant::now();
        }

        let Some(utxo) =
            take_spendable_utxo(&mut utxos, min_required, MAX_UNCONFIRMED_ANCESTOR_DEPTH)
        else {
            if let Err(e) =
                refresh_chain_utxos(client, key, &mut utxos, &mut coinbase_tx_cache).await
            {
                eprintln!("[warn] chain UTXO refresh failed: {e}");
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };

        match build_sign_and_send(client, key, &utxo, amount_zats).await {
            Ok((txid, new_outputs)) => {
                tx_count += 1;
                for (index, output) in new_outputs.into_iter().enumerate() {
                    let outpoint = transparent::OutPoint {
                        hash: txid,
                        index: index as u32,
                    };
                    utxos.push_back(SpendableUtxo {
                        outpoint,
                        output,
                        lineage_depth: utxo.lineage_depth.saturating_add(1),
                    });
                }

                if tx_count % 100 == 0 {
                    println!(
                        "[{tx_count}] sent tx: {} (queue_utxos={}, errors={err_count})",
                        txid,
                        utxos.len()
                    );
                }
            }
            Err(e) => {
                err_count += 1;
                eprintln!("[{}] tx failed: {e}", tx_count + err_count);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

pub(crate) fn take_spendable_utxo(
    utxos: &mut VecDeque<SpendableUtxo>,
    min_required: u64,
    max_depth: u32,
) -> Option<SpendableUtxo> {
    let len = utxos.len();
    for _ in 0..len {
        let utxo = utxos.pop_front()?;
        let value = u64::from(utxo.output.value);
        if value >= min_required && utxo.lineage_depth <= max_depth {
            return Some(utxo);
        }
        utxos.push_back(utxo);
    }
    None
}

pub(crate) async fn refresh_chain_utxos(
    client: &ZebraRpcClient,
    key: &FundedKey,
    utxos: &mut VecDeque<SpendableUtxo>,
    coinbase_tx_cache: &mut HashMap<Hash, bool>,
) -> Result<RefreshStats> {
    let chain_utxos = client.get_address_utxos(&key.address.to_string()).await?;
    let mut chain_queue = VecDeque::new();
    let mut stats = RefreshStats::default();

    for utxo in chain_utxos {
        let txid = Hash::from_str(&utxo.txid)
            .with_context(|| format!("invalid txid in getaddressutxos: {}", utxo.txid))?;

        if is_coinbase_transaction(client, &txid, coinbase_tx_cache).await? {
            stats.coinbase_utxos += 1;
            continue;
        }

        if let Some(spendable) = rpc_utxo_to_spendable(utxo, txid)? {
            chain_queue.push_back(spendable);
        }
    }

    if !chain_queue.is_empty() {
        *utxos = chain_queue;
    }

    Ok(stats)
}

async fn is_coinbase_transaction(
    client: &ZebraRpcClient,
    txid: &Hash,
    coinbase_tx_cache: &mut HashMap<Hash, bool>,
) -> Result<bool> {
    if let Some(is_coinbase) = coinbase_tx_cache.get(txid) {
        return Ok(*is_coinbase);
    }

    let tx = client
        .get_raw_transaction_verbose(&txid.to_string())
        .await?;
    let is_coinbase = tx.vin.first().is_some_and(|vin| vin.coinbase.is_some());
    coinbase_tx_cache.insert(txid.clone(), is_coinbase);

    Ok(is_coinbase)
}

fn rpc_utxo_to_spendable(utxo: AddressUtxo, txid: Hash) -> Result<Option<SpendableUtxo>> {
    let script_bytes = hex::decode(&utxo.script)
        .with_context(|| format!("invalid script hex in getaddressutxos: {}", utxo.script))?;

    let value = Amount::<NonNegative>::try_from(utxo.satoshis)
        .context("invalid satoshi amount in getaddressutxos")?;
    let output = transparent::Output::new(value, transparent::Script::new(&script_bytes));

    if output.is_dust() {
        return Ok(None);
    }

    Ok(Some(SpendableUtxo {
        outpoint: transparent::OutPoint {
            hash: txid,
            index: utxo.output_index,
        },
        output,
        lineage_depth: 0,
    }))
}

async fn build_sign_and_send(
    client: &ZebraRpcClient,
    key: &FundedKey,
    utxo: &SpendableUtxo,
    amount_zats: u64,
) -> Result<(zebra_chain::transaction::Hash, Vec<transparent::Output>)> {
    let input_value = u64::from(utxo.output.value);
    if input_value <= BASE_FEE_ZATS {
        anyhow::bail!("input value {} is not enough to pay fee", input_value);
    }

    let mut primary_value = amount_zats;
    if primary_value + BASE_FEE_ZATS > input_value {
        primary_value = input_value - BASE_FEE_ZATS;
    }

    let primary_amount = Amount::<NonNegative>::try_from(primary_value)
        .context("primary output amount does not fit in Amount")?;
    let mut outputs = vec![transparent::Output::new(
        primary_amount,
        key.address.script(),
    )];

    let change_value = input_value.saturating_sub(primary_value + BASE_FEE_ZATS);
    if change_value > 0 {
        let change_amount =
            Amount::<NonNegative>::try_from(change_value).context("invalid change amount")?;
        let change_output = transparent::Output::new(change_amount, key.address.script());
        if !change_output.is_dust() {
            outputs.push(change_output);
        }
    }

    if outputs.iter().all(transparent::Output::is_dust) {
        anyhow::bail!("all outputs are dust")
    }

    let mut tx = Transaction::V5 {
        network_upgrade: NetworkUpgrade::Nu6_1,
        lock_time: LockTime::unlocked(),
        expiry_height: Height(0),
        inputs: vec![transparent::Input::PrevOut {
            outpoint: utxo.outpoint,
            unlock_script: transparent::Script::new(&[]),
            sequence: u32::MAX,
        }],
        outputs: outputs.clone(),
        sapling_shielded_data: None,
        orchard_shielded_data: None,
    };

    let sighash = tx.sighash(
        NetworkUpgrade::Nu6_1,
        HashType::ALL,
        Arc::new(vec![utxo.output.clone()]),
        Some((0, utxo.output.lock_script.as_raw_bytes().to_vec())),
    )?;

    let secp = Secp256k1::new();
    let msg = Message::from_digest(sighash.0);
    let sig = secp.sign_ecdsa(&msg, &key.secret_key);
    let mut sig_bytes = sig.serialize_der().to_vec();
    sig_bytes.push(HashType::ALL.bits() as u8);

    let script_sig = build_p2pkh_script_sig(&sig_bytes, &key.public_key.serialize())?;

    match &mut tx {
        Transaction::V5 { inputs, .. } => match &mut inputs[0] {
            transparent::Input::PrevOut { unlock_script, .. } => {
                *unlock_script = script_sig;
            }
            transparent::Input::Coinbase { .. } => unreachable!("constructed prevout input"),
        },
        _ => unreachable!("constructed v5 tx"),
    }

    let tx_hex = hex::encode(tx.zcash_serialize_to_vec()?);
    let txid_hex = client.send_raw_transaction(&tx_hex).await?;
    let txid = zebra_chain::transaction::Hash::from_str(&txid_hex)
        .with_context(|| format!("invalid txid returned by sendrawtransaction: {txid_hex}"))?;

    Ok((txid, outputs))
}

fn build_p2pkh_script_sig(
    signature_with_hash_type: &[u8],
    pubkey: &[u8],
) -> Result<transparent::Script> {
    let mut script = Vec::new();
    push_small_data(&mut script, signature_with_hash_type)?;
    push_small_data(&mut script, pubkey)?;
    Ok(transparent::Script::new(&script))
}

fn push_small_data(script: &mut Vec<u8>, data: &[u8]) -> Result<()> {
    if data.len() > 75 {
        anyhow::bail!("pushdata too long for small push opcode: {}", data.len());
    }

    script.push(data.len() as u8);
    script.extend_from_slice(data);
    Ok(())
}

pub(crate) fn amount_to_zatoshis(amount: f64) -> Result<u64> {
    if !amount.is_finite() || amount <= 0.0 {
        anyhow::bail!("amount must be a positive finite number of ZEC");
    }

    let zats_f64 = amount * 100_000_000.0;
    let zats_rounded = zats_f64.round();

    if zats_rounded <= 0.0 {
        anyhow::bail!("amount is too small: {amount}");
    }

    if (zats_f64 - zats_rounded).abs() > 0.000_000_1 {
        anyhow::bail!("amount must have at most 8 decimal places: {amount}");
    }

    let zats = zats_rounded as u64;
    if zats <= BASE_FEE_ZATS {
        anyhow::bail!(
            "amount {} zats is too small for txblast with {} zats fee",
            zats,
            BASE_FEE_ZATS
        );
    }

    Ok(zats)
}

#[cfg(test)]
mod tests {
    use super::{amount_to_zatoshis, build_p2pkh_script_sig};

    #[test]
    fn converts_amount_to_zatoshis() {
        assert_eq!(amount_to_zatoshis(0.001).expect("valid amount"), 100_000);
    }

    #[test]
    fn rejects_more_than_8_decimals() {
        let err = amount_to_zatoshis(0.001_000_000_1).expect_err("should fail");
        assert!(err.to_string().contains("8 decimal"));
    }

    #[test]
    fn builds_small_push_script_sig() {
        let sig = vec![1u8; 72];
        let pubkey = vec![2u8; 33];
        let script = build_p2pkh_script_sig(&sig, &pubkey).expect("script should build");
        let raw = script.as_raw_bytes();
        assert_eq!(raw[0], 72);
        assert_eq!(raw[73], 33);
        assert_eq!(raw.len(), 1 + 72 + 1 + 33);
    }
}
