use std::collections::HashMap;

use anyhow::Result;

use crate::txblast::rpc::{AddressUtxo, ZebraRpcClient};

use super::{TreasuryInventory, TreasuryUtxo};

pub(crate) const COINBASE_MATURITY: u32 = 100;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TreasuryRefresh {
    pub(crate) earliest_maturity_height: Option<u32>,
}

pub(crate) async fn refresh_treasury_inventory(
    client: &ZebraRpcClient,
    address: &str,
    current_height: u32,
    min_value: u64,
    inventory: &mut TreasuryInventory,
    coinbase_cache: &mut HashMap<String, bool>,
) -> Result<TreasuryRefresh> {
    let utxos = client.get_address_utxos(address).await?;
    let mut earliest_maturity_height = None;
    let mut discovered = Vec::new();

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

        if utxo.satoshis < min_value {
            continue;
        }

        discovered.push(to_treasury_utxo(utxo));
    }

    inventory.refresh_discovered(discovered);

    Ok(TreasuryRefresh {
        earliest_maturity_height,
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
