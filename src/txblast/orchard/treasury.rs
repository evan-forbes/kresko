use std::collections::HashMap;

use anyhow::Result;

use crate::txblast::rpc::{AddressUtxo, ZebraRpcClient};

use super::{TreasuryInventory, TreasuryUtxo};

pub(crate) const COINBASE_MATURITY: u32 = 100;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TreasuryRefresh {
    pub(crate) earliest_maturity_height: Option<u32>,
    pub(crate) funding_tx_visible: bool,
    pub(crate) funding_tx_confirmed: bool,
    pub(crate) spendable_funding_utxo_count: usize,
    pub(crate) spendable_funding_balance_zats: u64,
}

pub(crate) async fn refresh_treasury_inventory(
    client: &ZebraRpcClient,
    address: &str,
    current_height: u32,
    min_value: u64,
    expected_funding_txid: Option<&str>,
    inventory: &mut TreasuryInventory,
    coinbase_cache: &mut HashMap<String, bool>,
) -> Result<TreasuryRefresh> {
    let utxos = client.get_address_utxos(address).await?;
    let mut earliest_maturity_height = None;
    let mut discovered = Vec::new();
    let funding_tx = if let Some(txid) = expected_funding_txid {
        client.try_get_raw_transaction_verbose(txid).await?
    } else {
        None
    };
    let funding_tx_visible = funding_tx.is_some();
    let funding_tx_confirmed = funding_tx
        .as_ref()
        .and_then(|tx| tx.confirmations)
        .is_some_and(|value| value > 0)
        && funding_tx
            .as_ref()
            .and_then(|tx| tx.blockhash.as_ref())
            .is_some();
    let mut spendable_funding_utxo_count = 0usize;
    let mut spendable_funding_balance_zats = 0u64;

    for utxo in utxos {
        let is_coinbase = is_coinbase_transaction(client, &utxo.txid, coinbase_cache).await?;
        if is_coinbase {
            let maturity_height = utxo.height.saturating_add(COINBASE_MATURITY);
            if current_height < maturity_height {
                earliest_maturity_height = earliest_maturity_height
                    .map(|earliest: u32| earliest.min(maturity_height))
                    .or(Some(maturity_height));
                continue;
            }
        }

        if expected_funding_txid.is_some_and(|txid| !is_coinbase && utxo.txid == txid) {
            spendable_funding_utxo_count += 1;
            spendable_funding_balance_zats =
                spendable_funding_balance_zats.saturating_add(utxo.satoshis);
        }

        if utxo.satoshis < min_value {
            continue;
        }

        discovered.push(to_treasury_utxo(utxo));
    }

    inventory.refresh_discovered(discovered);

    Ok(TreasuryRefresh {
        earliest_maturity_height,
        funding_tx_visible,
        funding_tx_confirmed,
        spendable_funding_utxo_count,
        spendable_funding_balance_zats,
    })
}

fn to_treasury_utxo(utxo: AddressUtxo) -> TreasuryUtxo {
    TreasuryUtxo {
        outpoint_id: format!("{}:{}", utxo.txid, utxo.output_index),
        txid: utxo.txid,
        output_index: utxo.output_index,
        script: utxo.script,
        satoshis: utxo.satoshis,
        height: utxo.height,
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
