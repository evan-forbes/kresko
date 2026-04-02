use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use orchard::keys::{FullViewingKey, Scope, SpendingKey};
use zcash_primitives::transaction::builder::{BuildConfig, Builder};
use zcash_primitives::transaction::fees::zip317;
use zcash_protocol::consensus::{self, BlockHeight, NetworkType, NetworkUpgrade};
use zcash_protocol::memo::MemoBytes;
use zcash_protocol::value::Zatoshis;
use zcash_transparent::bundle::{OutPoint, TxOut};
use zcash_transparent::builder::TransparentSigningSet;
use zebra_chain::serialization::ZcashSerialize;

use super::rpc::ZebraRpcClient;
use super::transparent::{self, FundedKey};

const BASE_FEE_ZATS: u64 = 20_000; // ZIP-317 minimum for 1 input + 2 Orchard actions
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Minimal consensus parameters — all upgrades active at height 1.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct KreskoTestnet;

impl consensus::Parameters for KreskoTestnet {
    fn network_type(&self) -> NetworkType {
        NetworkType::Test
    }

    fn activation_height(&self, _nu: NetworkUpgrade) -> Option<BlockHeight> {
        // All upgrades active from height 1 on kresko local testnets.
        Some(BlockHeight::from_u32(1))
    }
}

// ---------------------------------------------------------------------------
// Dummy Sapling provers — never called, builder type signature requires them.
// ---------------------------------------------------------------------------

struct NoSaplingSpendProver;

impl sapling_crypto::prover::SpendProver for NoSaplingSpendProver {
    type Proof = sapling_crypto::bundle::GrothProofBytes;

    fn prepare_circuit(
        _: sapling_crypto::ProofGenerationKey,
        _: sapling_crypto::Diversifier,
        _: sapling_crypto::Rseed,
        _: sapling_crypto::value::NoteValue,
        _: jubjub::Fr,
        _: sapling_crypto::value::ValueCommitTrapdoor,
        _: bls12_381::Scalar,
        _: sapling_crypto::MerklePath,
    ) -> Option<sapling_crypto::circuit::Spend> {
        unreachable!("no Sapling spends in shielded txblast")
    }

    fn create_proof<R: rand_core_06::RngCore>(
        &self,
        _: sapling_crypto::circuit::Spend,
        _: &mut R,
    ) -> Self::Proof {
        unreachable!("no Sapling spends in shielded txblast")
    }

    fn encode_proof(proof: Self::Proof) -> sapling_crypto::bundle::GrothProofBytes {
        proof
    }
}

struct NoSaplingOutputProver;

impl sapling_crypto::prover::OutputProver for NoSaplingOutputProver {
    type Proof = sapling_crypto::bundle::GrothProofBytes;

    fn prepare_circuit(
        _: &sapling_crypto::keys::EphemeralSecretKey,
        _: sapling_crypto::PaymentAddress,
        _: jubjub::Fr,
        _: sapling_crypto::value::NoteValue,
        _: sapling_crypto::value::ValueCommitTrapdoor,
    ) -> sapling_crypto::circuit::Output {
        unreachable!("no Sapling outputs in shielded txblast")
    }

    fn create_proof<R: rand_core_06::RngCore>(
        &self,
        _: sapling_crypto::circuit::Output,
        _: &mut R,
    ) -> Self::Proof {
        unreachable!("no Sapling outputs in shielded txblast")
    }

    fn encode_proof(proof: Self::Proof) -> sapling_crypto::bundle::GrothProofBytes {
        proof
    }
}

// ---------------------------------------------------------------------------
// Orchard key container.
// ---------------------------------------------------------------------------

struct OrchardKeys {
    address: orchard::Address,
    ovk: orchard::keys::OutgoingViewingKey,
}

fn derive_orchard_keys(secret: &[u8; 32]) -> Result<OrchardKeys> {
    let ct = SpendingKey::from_bytes(*secret);
    if bool::from(ct.is_none()) {
        anyhow::bail!("funded key secret bytes are not a valid Orchard SpendingKey");
    }
    let sk = ct.unwrap();
    let fvk = FullViewingKey::from(&sk);
    let address = fvk.address_at(0u32, Scope::External);
    let ovk = fvk.to_ovk(Scope::External);
    Ok(OrchardKeys { address, ovk })
}

// ---------------------------------------------------------------------------
// Anchor management.
// ---------------------------------------------------------------------------

async fn fetch_orchard_anchor(client: &ZebraRpcClient) -> Result<orchard::Anchor> {
    let height = client.get_block_count().await?;
    if height == 0 {
        return Ok(orchard::Anchor::empty_tree());
    }
    let treestate = client.z_get_treestate(height).await?;
    let root_hex = treestate
        .pointer("/orchard/commitments/finalRoot")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if root_hex.is_empty()
        || root_hex == "0000000000000000000000000000000000000000000000000000000000000000"
    {
        return Ok(orchard::Anchor::empty_tree());
    }
    let root_bytes: [u8; 32] = hex::decode(root_hex)
        .context("orchard finalRoot is not valid hex")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("orchard finalRoot is not 32 bytes"))?;
    let ct = orchard::Anchor::from_bytes(root_bytes);
    if bool::from(ct.is_none()) {
        anyhow::bail!("orchard finalRoot is not a valid anchor");
    }
    Ok(ct.unwrap())
}

// ---------------------------------------------------------------------------
// Type bridging: zebra-chain → zcash_transparent types via serialization.
// ---------------------------------------------------------------------------

fn bridge_outpoint(zc: &zebra_chain::transparent::OutPoint) -> OutPoint {
    OutPoint::new(zc.hash.0, zc.index)
}

fn bridge_txout(zc: &zebra_chain::transparent::Output) -> Result<TxOut> {
    let mut bytes = Vec::new();
    zc.zcash_serialize(&mut bytes)
        .context("failed to serialize transparent output")?;
    let mut cursor = std::io::Cursor::new(&bytes);
    TxOut::read(&mut cursor).map_err(|e| anyhow::anyhow!("bridge TxOut: {e}"))
}

fn funded_key_to_transparent_address(
    key: &FundedKey,
) -> Result<zcash_transparent::address::TransparentAddress> {
    // Extract 20-byte pubkey hash from P2PKH script:
    // OP_DUP OP_HASH160 <20 bytes> OP_EQUALVERIFY OP_CHECKSIG
    let script_bytes = key.address.script().as_raw_bytes().to_vec();
    if script_bytes.len() == 25
        && script_bytes[0] == 0x76
        && script_bytes[1] == 0xa9
        && script_bytes[2] == 0x14
        && script_bytes[23] == 0x88
        && script_bytes[24] == 0xac
    {
        let mut hash = [0u8; 20];
        hash.copy_from_slice(&script_bytes[3..23]);
        Ok(zcash_transparent::address::TransparentAddress::PublicKeyHash(hash))
    } else {
        anyhow::bail!("funded key address is not a standard P2PKH address")
    }
}

// ---------------------------------------------------------------------------
// Shielding transaction builder.
// ---------------------------------------------------------------------------

async fn build_and_send_shielding_tx(
    client: &ZebraRpcClient,
    funded_key: &FundedKey,
    orchard_keys: &OrchardKeys,
    utxo_outpoint: &zebra_chain::transparent::OutPoint,
    utxo_output: &zebra_chain::transparent::Output,
    amount_zats: u64,
    anchor: orchard::Anchor,
    target_height: u32,
) -> Result<String> {
    let input_value = u64::from(utxo_output.value);
    if input_value <= BASE_FEE_ZATS {
        anyhow::bail!("input value {} is not enough to pay fee", input_value);
    }

    let output_value = amount_zats.min(input_value.saturating_sub(BASE_FEE_ZATS));
    if output_value == 0 {
        anyhow::bail!("output value would be zero after fee");
    }

    let build_config = BuildConfig::Standard {
        sapling_anchor: None,
        orchard_anchor: Some(anchor),
    };
    let height = BlockHeight::from_u32(target_height);
    let mut builder = Builder::new(KreskoTestnet, height, build_config);

    // Transparent input.
    let outpoint = bridge_outpoint(utxo_outpoint);
    let coin = bridge_txout(utxo_output)?;
    builder
        .add_transparent_input(funded_key.public_key, outpoint, coin)
        .map_err(|e| anyhow::anyhow!("add_transparent_input: {e}"))?;

    // Orchard output (shielding).
    builder
        .add_orchard_output::<zip317::FeeError>(
            Some(orchard_keys.ovk.clone()),
            orchard_keys.address,
            output_value,
            MemoBytes::empty(),
        )
        .map_err(|e| anyhow::anyhow!("add_orchard_output: {e}"))?;

    // Return change as transparent so we can keep spending it.
    let change_value = input_value
        .saturating_sub(output_value)
        .saturating_sub(BASE_FEE_ZATS);
    if change_value > 546 {
        let change_zats = Zatoshis::from_u64(change_value)
            .map_err(|_| anyhow::anyhow!("change value out of range"))?;
        let change_addr = funded_key_to_transparent_address(funded_key)?;
        builder
            .add_transparent_output(&change_addr, change_zats)
            .map_err(|e| anyhow::anyhow!("add_transparent_output (change): {e}"))?;
    }

    // Sign and prove.
    let mut signing_set = TransparentSigningSet::new();
    signing_set.add_key(funded_key.secret_key);

    let fee_rule = zip317::FeeRule::standard();
    let start = Instant::now();
    let result = builder
        .build(
            &signing_set,
            &[],
            &[],
            rand_core_06::OsRng,
            &NoSaplingSpendProver,
            &NoSaplingOutputProver,
            &fee_rule,
        )
        .map_err(|e| anyhow::anyhow!("transaction build failed: {e}"))?;
    let proving_ms = start.elapsed().as_millis();

    // Serialize and submit.
    let mut tx_bytes = Vec::new();
    result
        .transaction()
        .write(&mut tx_bytes)
        .map_err(|e| anyhow::anyhow!("failed to serialize transaction: {e}"))?;
    let tx_hex = hex::encode(&tx_bytes);
    let txid = client.send_raw_transaction(&tx_hex).await?;

    if proving_ms > 1000 {
        eprintln!("[shielded] Orchard proving took {proving_ms}ms");
    }

    Ok(txid)
}

// ---------------------------------------------------------------------------
// Main run loop — mirrors transparent::run pattern.
// ---------------------------------------------------------------------------

pub async fn run(
    client: &ZebraRpcClient,
    key: &FundedKey,
    rate: u64,
    amount: f64,
) -> Result<()> {
    if rate == 0 {
        anyhow::bail!("--rate must be greater than 0");
    }

    let secret_bytes: [u8; 32] = key.secret_key.secret_bytes();
    let orchard_keys = derive_orchard_keys(&secret_bytes)?;
    println!(
        "[shielded] Orchard address derived from funded key '{}'",
        key.name,
    );

    let amount_zats = transparent::amount_to_zatoshis(amount)?;

    // Load initial UTXO set.
    let mut utxos = VecDeque::new();
    let mut coinbase_tx_cache = std::collections::HashMap::new();
    let initial_refresh =
        transparent::refresh_chain_utxos(client, key, &mut utxos, &mut coinbase_tx_cache).await?;
    if utxos.is_empty() {
        if initial_refresh.coinbase_utxos > 0 {
            anyhow::bail!(
                "found {} UTXOs but all are coinbase — shielded txblast needs non-coinbase transparent UTXOs",
                initial_refresh.coinbase_utxos,
            );
        }
        anyhow::bail!(
            "no spendable transparent UTXOs found for {}. make sure premine blocks were loaded",
            key.address,
        );
    }

    // Fetch initial Orchard anchor.
    let mut anchor = fetch_orchard_anchor(client).await?;
    println!(
        "[shielded] initial state: {} UTXOs, anchor fetched",
        utxos.len()
    );

    let interval = Duration::from_secs_f64(1.0 / rate as f64);
    let mut ticker = tokio::time::interval(interval);
    let mut tx_count: u64 = 0;
    let mut err_count: u64 = 0;
    let mut last_refresh = tokio::time::Instant::now();

    loop {
        ticker.tick().await;

        // Periodic refresh of UTXOs and anchor.
        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            if let Err(e) =
                transparent::refresh_chain_utxos(client, key, &mut utxos, &mut coinbase_tx_cache)
                    .await
            {
                eprintln!("[shielded][warn] UTXO refresh failed: {e}");
            }
            match fetch_orchard_anchor(client).await {
                Ok(a) => anchor = a,
                Err(e) => eprintln!("[shielded][warn] anchor refresh failed: {e}"),
            }
            last_refresh = tokio::time::Instant::now();
        }

        let target_height = client.get_block_count().await.unwrap_or(100) + 10;

        let Some(utxo) =
            transparent::take_spendable_utxo(&mut utxos, amount_zats + BASE_FEE_ZATS, 20)
        else {
            if let Err(e) =
                transparent::refresh_chain_utxos(client, key, &mut utxos, &mut coinbase_tx_cache)
                    .await
            {
                eprintln!("[shielded][warn] UTXO refresh failed: {e}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };

        match build_and_send_shielding_tx(
            client,
            key,
            &orchard_keys,
            &utxo.outpoint,
            &utxo.output,
            amount_zats,
            anchor,
            target_height,
        )
        .await
        {
            Ok(txid) => {
                tx_count += 1;
                if tx_count % 10 == 0 {
                    println!(
                        "[shielded][{tx_count}] sent shielding tx: {txid} (queue={}, errors={err_count})",
                        utxos.len()
                    );
                }
            }
            Err(e) => {
                err_count += 1;
                eprintln!("[shielded][{}] tx failed: {e}", tx_count + err_count);
                utxos.push_back(utxo);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}
