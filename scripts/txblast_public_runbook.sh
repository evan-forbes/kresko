#!/usr/bin/env bash
#
# Public-network txblast runbook.
#
# This file is documentation first. It is intentionally written as a shell-shaped
# runbook so the eventual commands are easy to copy, review, and turn into an
# executable workflow after public fanout/fanin/recovery are implemented.

set -euo pipefail
#
# Current implementation status:
#
# - Implemented and safe to run:
#   - kresko txblast wallet init
#   - kresko txblast deposit address
#   - kresko txblast deposit import
#   - kresko txblast deposit status
#   - kresko txblast plan
#
# - Guarded / not yet fund-moving:
#   - kresko txblast prepare
#   - kresko txblast run
#   - kresko txblast stop
#   - kresko txblast status
#   - kresko txblast withdraw
#   - kresko txblast recover inventory
#   - kresko txblast recover sweep
#
# "Guarded" means the CLI exists, parses arguments, validates wallet/plan/network
# state, and refuses to perform the unsafe part. Commands that would broadcast
# real transactions currently require --dry-run or return a clear error before
# moving funds. This lets us document and test the lifecycle without pretending
# scanner-backed fanout, fan-in, or emergency sweep are finished.
#
# Assumptions:
#
# - Run from the experiment directory, or set EXPERIMENT_DIR below.
# - The experiment was provisioned and deployed via the Python orchestration
#   layer (`harness`) for either `public-testnet` or `mainnet`.
# - Nodes have already been created/deployed/synced when you reach prepare/run.
# - For public testnet, first practice the full lifecycle with small funds.
# - Do not enable mainnet runs until public-testnet recovery drills pass.

###############################################################################
# 0. Operator configuration
###############################################################################

# Directory containing config.json.
EXPERIMENT_DIR="${EXPERIMENT_DIR:-.}"

# Use public-testnet first. For mainnet, the wallet init command below needs
# --require-mainnet-confirmation, and later spending/recovery commands need their
# explicit mainnet confirmation flags.
NETWORK="${NETWORK:-public-testnet}"

# External address where funds should ultimately return after the experiment.
# This must match NETWORK: testnet t-address for public-testnet, mainnet t-address
# for mainnet.
WITHDRAW_TO="${WITHDRAW_TO:-CHANGE_ME_TRANSPARENT_ADDRESS}"

# Deposit tracking can use getaddressutxos when the RPC endpoint has address
# index support. If address-index RPC is unavailable, use txblast deposit import.
RPC_ENDPOINT="${RPC_ENDPOINT:-http://localhost:18232}"

# One fleet-wide block fill target. This is not per node.
TARGET_BLOCK_BYTES="${TARGET_BLOCK_BYTES:-1000000}"

# Zcash public networks target 75 second post-Blossom blocks.
BLOCK_SPACING_SECS="${BLOCK_SPACING_SECS:-75}"

# Replace with a measured lane-advance serialized transaction size once the
# public tx builder path can produce a sample. The current placeholder is
# deliberately conservative enough for planning, not final tuning.
MEASURED_TX_BYTES="${MEASURED_TX_BYTES:-3000}"

# The node selector is evaluated against config.json miners.
NODES="${NODES:-all}"

mainnet_wallet_init_args=()
mainnet_run_args=()
mainnet_withdraw_args=()
mainnet_recover_args=()
if [[ "${NETWORK}" == "mainnet" ]]; then
  mainnet_wallet_init_args=(--require-mainnet-confirmation)
  mainnet_run_args=(--mainnet-i-understand-fees)
  mainnet_withdraw_args=(--mainnet-i-understand-finality)
  mainnet_recover_args=(--mainnet-i-understand-recovery)
fi

###############################################################################
# 1. Create local wallet and recovery bundle
###############################################################################

# Public testnet:
#
kresko txblast wallet init \
  -d "${EXPERIMENT_DIR}" \
  --network "${NETWORK}" \
  --rpc-endpoint "${RPC_ENDPOINT}" \
  --lanes-per-node 100 \
  --lane-value-zats 30000 \
  --fanout-width 1 \
  "${mainnet_wallet_init_args[@]}"
#
# Mainnet requires an explicit acknowledgement:
#
#   kresko txblast wallet init \
#     -d "${EXPERIMENT_DIR}" \
#     --network mainnet \
#     --rpc-endpoint "${RPC_ENDPOINT}" \
#     --lanes-per-node 100 \
#     --lane-value-zats 30000 \
#     --fanout-width 1 \
#     --require-mainnet-confirmation
#
# Outputs:
#
# - ${EXPERIMENT_DIR}/.kresko/txblast/wallet.json
# - ${EXPERIMENT_DIR}/.kresko/txblast/recovery.json
#
# recovery.json is chmod 0600 and contains the control key plus per-node hot
# keys. It is the file needed if miner VMs disappear.

###############################################################################
# 2. Get the deposit address
###############################################################################

# Print the transparent control deposit address:
#
kresko txblast deposit address -d "${EXPERIMENT_DIR}"
#
# JSON form:
#
#   kresko txblast deposit address -d "${EXPERIMENT_DIR}" --json
#
# Send funds from an external wallet to this address. For public testnet, use
# small faucet/test funds. For mainnet, do not send funds until the prepare,
# withdraw, and recovery paths are implemented and tested on public testnet.

###############################################################################
# 3. Track deposits
###############################################################################

kresko txblast deposit status \
  -d "${EXPERIMENT_DIR}" \
  --rpc-endpoint "${RPC_ENDPOINT}" \
  --confirmations 3
#
# When address-index RPC is available, deposit status auto-imports confirmed
# control-address UTXOs for planning. Plan also auto-syncs before checking
# funding.
#
# If getaddressutxos is not available, manually import the known deposit:
#
#   kresko txblast deposit import \
#     -d "${EXPERIMENT_DIR}" \
#     --txid CHANGE_ME_DEPOSIT_TXID \
#     --vout CHANGE_ME_VOUT \
#     --amount-zats CHANGE_ME_AMOUNT_ZATS
#
# Optional safety assertion:
#
#   kresko txblast deposit import \
#     -d "${EXPERIMENT_DIR}" \
#     --txid CHANGE_ME_DEPOSIT_TXID \
#     --vout CHANGE_ME_VOUT \
#     --amount-zats CHANGE_ME_AMOUNT_ZATS \
#     --address "$(kresko txblast deposit address -d "${EXPERIMENT_DIR}")"
#
# Imported deposits are only planning inputs until verified through RPC and
# confirmations. They are not treated as trusted spendable funds by themselves.

###############################################################################
# 4. Build the fleet-wide funding and byte-budget plan
###############################################################################

# This records an immutable plan in wallet.json. The target is global across the
# fleet, so 1,000,000 bytes/block split across 20 nodes is still 1,000,000 total
# bytes/block, not 20,000,000.
#
kresko txblast plan \
  -d "${EXPERIMENT_DIR}" \
  --target-block-bytes "${TARGET_BLOCK_BYTES}" \
  --block-spacing-secs "${BLOCK_SPACING_SECS}" \
  --duration-secs 900 \
  --nodes "${NODES}" \
  --measured-tx-bytes "${MEASURED_TX_BYTES}" \
  --safety-margin 0.20
#
# JSON form:
#
#   kresko txblast plan \
#     -d "${EXPERIMENT_DIR}" \
#     --target-block-bytes "${TARGET_BLOCK_BYTES}" \
#     --block-spacing-secs "${BLOCK_SPACING_SECS}" \
#     --duration-secs 900 \
#     --nodes "${NODES}" \
#     --measured-tx-bytes "${MEASURED_TX_BYTES}" \
#     --safety-margin 0.20 \
#     --json
#
# If deposits are not imported yet, this can still be recorded for review:
#
#   kresko txblast plan \
#     -d "${EXPERIMENT_DIR}" \
#     --target-block-bytes "${TARGET_BLOCK_BYTES}" \
#     --block-spacing-secs "${BLOCK_SPACING_SECS}" \
#     --duration-secs 900 \
#     --nodes "${NODES}" \
#     --measured-tx-bytes "${MEASURED_TX_BYTES}" \
#     --safety-margin 0.20 \
#     --allow-underfunded-plan

###############################################################################
# 5. Prepare funds into hot keys and lane inventory
###############################################################################

# Current status: guarded. Today this validates wallet/plan state in --dry-run,
# but it will not broadcast fanout or shielding transactions.
#
kresko txblast prepare \
  -d "${EXPERIMENT_DIR}" \
  --dry-run
#
# Future behavior:
#
# - Verify nodes are synced and on the expected public network.
# - Select confirmed deposit UTXOs.
# - Coordinator signs control-wallet spends.
# - Deploy one hot key file per node.
# - Create lane notes per node.
# - Record lane inventory in durable state.
#
# Future real command shape:
#
#   kresko txblast prepare \
#     -d "${EXPERIMENT_DIR}" \
#     --plan CHANGE_ME_PLAN_ID

###############################################################################
# 6. Run byte-budgeted txblast
###############################################################################

# Current status: guarded. Today this prints the selected plan and byte budget,
# but it does not start remote agents because prepare has not created durable
# hot-key lane inventory yet.
#
kresko txblast run \
  -d "${EXPERIMENT_DIR}" \
  --max-pending-txs 50 \
  --max-pending-bytes 250000 \
  --max-mempool-bytes 1500000 \
  --feedback-window-blocks 10 \
  "${mainnet_run_args[@]}"
#
# Mainnet will require:
#
#   kresko txblast run \
#     -d "${EXPERIMENT_DIR}" \
#     --mainnet-i-understand-fees \
#     --max-pending-txs 50 \
#     --max-pending-bytes 250000 \
#     --max-mempool-bytes 1500000
#
# Future behavior:
#
# - Split the global byte budget across active agents.
# - Spend tokens by actual serialized tx bytes.
# - Pause/reduce when observed mempool bytes exceed guardrails.
# - Slowly adjust based on observed block sizes.
# - Keep lane funds isolated during the run.

###############################################################################
# 7. Monitor and stop
###############################################################################

# Current public status is split:
#
# - Deposit/wallet status is implemented:
#
kresko txblast deposit status \
  -d "${EXPERIMENT_DIR}" \
  --rpc-endpoint "${RPC_ENDPOINT}" \
  --confirmations 3
#
# - Unified public workload status is guarded:
#
kresko txblast status \
  -d "${EXPERIMENT_DIR}" \
  --dry-run
#
# - Public stop is guarded until the public runner control channel exists:
#
kresko txblast stop \
  -d "${EXPERIMENT_DIR}" \
  --dry-run

###############################################################################
# 8. Withdraw funds back to an external wallet
###############################################################################

# Current status: guarded. Address validation and dry-run are implemented, but
# no public fan-in/sweep transaction is broadcast.
#
kresko txblast withdraw \
  -d "${EXPERIMENT_DIR}" \
  --to "${WITHDRAW_TO}" \
  --amount all \
  --dry-run \
  "${mainnet_withdraw_args[@]}"
#
# Mainnet future command shape:
#
#   kresko txblast withdraw \
#     -d "${EXPERIMENT_DIR}" \
#     --to "${WITHDRAW_TO}" \
#     --amount all \
#     --mainnet-i-understand-finality
#
# Future behavior:
#
# - Stop agents.
# - Wait for pending spends to confirm or disappear from mempools.
# - Rescan each hot key from wallet birthday / checkpoint.
# - Consolidate lane notes.
# - Send funds to the external address.
# - Record withdrawal txids and residual fee/dust accounting.

###############################################################################
# 9. Emergency recovery
###############################################################################

# Inventory currently verifies the recovery bundle is present and reports the
# control/hot-key count, but it does not scan chain notes yet:
#
kresko txblast recover inventory \
  -d "${EXPERIMENT_DIR}" \
  --from-height 0 \
  --json
#
# Sweep is guarded. Dry-run validates the destination address and reports the
# intended scan start:
#
kresko txblast recover sweep \
  -d "${EXPERIMENT_DIR}" \
  --to "${WITHDRAW_TO}" \
  --from-height 0 \
  --dry-run \
  "${mainnet_recover_args[@]}"
#
# Mainnet future command shape:
#
#   kresko txblast recover sweep \
#     -d "${EXPERIMENT_DIR}" \
#     --to "${WITHDRAW_TO}" \
#     --from-height CHANGE_ME_BIRTHDAY_HEIGHT \
#     --mainnet-i-understand-recovery
#
# Future behavior:
#
# - Use only recovery.json and a synced public-network RPC endpoint.
# - Scan Orchard bundles for control and hot keys.
# - Track nullifiers to distinguish spent and unspent notes.
# - Track transparent UTXOs.
# - Sweep recoverable funds to WITHDRAW_TO.

###############################################################################
# 10. Mainnet enablement checklist
###############################################################################

# Do not run mainnet txblast until all of these are true:
#
# - Public-testnet prepare creates lane inventory and can resume after interruption.
# - Public-testnet run enforces one global byte budget across multiple nodes.
# - Public-testnet withdraw recovers expected non-fee funds.
# - Public-testnet recover inventory works after deleting remote traces.
# - Public-testnet recover sweep works after simulating lost miner VMs.
# - Mainnet transaction builder tests cover mainnet/testnet address mismatch.
# - Operator has reviewed fee burn estimates from txblast plan.
#
# The current guarded commands are deliberately there to keep this checklist from
# being bypassed accidentally.
