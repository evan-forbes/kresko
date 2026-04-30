#!/usr/bin/env bash
#
# Mainnet observer-node runbook.
#
# This is documentation shaped like a shell script. The uncommented lines are
# the operator-level commands we expect to run. The detailed validation should
# live in Kresko commands and in the payload node_init.sh, not in ad hoc grep,
# curl, jq, or ssh snippets here.

set -euo pipefail

###############################################################################
# Operator inputs
###############################################################################

KRESKO_REPO="${KRESKO_REPO:-/home/evan/src/zcash/kresko}"
EXPERIMENT="${EXPERIMENT:-mainnet-watch}"
CHAIN_ID="${CHAIN_ID:-mainnet-watch}"
PROVIDER="${PROVIDER:-digitalocean}"
NODE_COUNT="${NODE_COUNT:-2}"

###############################################################################
# Mainnet init flow
###############################################################################

cd "${KRESKO_REPO}"

kresko init \
  -c "${CHAIN_ID}" \
  -e "${EXPERIMENT}" \
  -p "${PROVIDER}" \
  -N mainnet

cd "${EXPERIMENT}"

MINER_COUNT="${NODE_COUNT}" scripts/bootstrap.sh

scripts/init.sh

###############################################################################
# What Kresko now owns
###############################################################################
#
# `kresko init -N mainnet` should generate a public-network init flow:
#
# - build/select Ubuntu-compatible binaries
# - provision nodes with `kresko up`
# - sync public IPs with `kresko sync-ips --overwrite`
# - generate payload with `kresko genesis-public`
# - deploy with `kresko deploy`
# - validate with `kresko status` / `kresko check`
#
# `kresko genesis-public` packages a public-network `node_init.sh` into
# `payload/scripts/node_init.sh` and `payload/node_init.sh`. `kresko deploy`
# should prefer the payload copy for public networks, so the normal deploy
# mechanism still works without asking the operator to run remote commands.
#
# The public node init script should:
#
# - preserve mainnet state by default with KRESKO_FRESH_STATE=0
# - refuse local-genesis-only target-spacing env vars
# - install the payload binaries
# - copy the per-node zebrad.toml
# - validate Mainnet/Testnet, P2P port, seeders, and external_addr before start
# - start zebrad only as an observer, with no mining/bootstrap/funding path
#
###############################################################################
# What still belongs in Kresko, not this script
###############################################################################
#
# These are the next Kresko features that would remove remaining manual checks:
#
# - `kresko public doctor`: validate public payload vars, per-node configs,
#   seeders, external_addr, RPC/P2P ports, and script selection before deploy.
# - `kresko public wait-sync`: wait until all selected public nodes are synced
#   near tip before txblast/manual data collection.
# - provider firewall management: allow inbound P2P 8233 and block public
#   inbound RPC 8232 for mainnet.
# - provider sizing checks: require >=500 GB usable disk for mainnet, and avoid
#   selecting RAM-heavy SKUs only because they happen to include enough disk.
# - Linode mainnet storage handling: either attach a large enough volume or
#   reject Linode mainnet unless the selected plan has enough local disk.
# - deploy preflight: fail fast if a public network would use the local-genesis
#   `scripts/node_init.sh` instead of the payload public script.
#
###############################################################################
# What local testnet init has that mainnet should not
###############################################################################
#
# Remove these from the mainnet flow:
#
# - `kresko genesis`
# - local genesis block / premine artifact generation
# - target block spacing overrides and target-spacing guards
# - miner address auto-bootstrap
# - `kresko progress`
# - `kresko start-miners`
# - bounded PoW / bounded generate runs
# - local txblast smoke/sample runs
# - runtime key funding from local genesis treasury
#
# Keep these:
#
# - `kresko add`
# - `kresko up`
# - `kresko sync-ips`
# - `kresko genesis-public`
# - `kresko deploy`
# - `kresko status`
# - `kresko check`
# - `kresko collect` when preserving logs/traces matters
