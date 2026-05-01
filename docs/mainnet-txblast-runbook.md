# Mainnet Txblast Runbook

This runbook describes how to use Kresko public-network txblast to fill a
chosen byte budget of mainnet blocks with Orchard transactions.

Mainnet txblast spends real ZEC and pays real fees. Start with a small target,
watch the chain and node health, and stop immediately if the run is affecting
normal network use more than intended.

## Model

The public-network path is staged:

1. A control wallet receives one or more transparent deposits.
2. `prepare` shields confirmed deposits into control Orchard inventory.
3. If needed, `prepare` splits large control Orchard reservoir notes into enough
   fanout source reservoirs for the selected lane topology.
4. After shielding and any reservoir split confirms, `prepare` fans Orchard
   funds out to per-node hot Orchard lane inventory.
5. `run` starts `txblast-local` on the selected nodes. Runtime traffic spends
   Orchard lane notes and replenishes lanes with shielded fanout.
6. `withdraw` or `recover sweep` spends remaining Orchard inventory to the
   requested transparent address.

The durable funding path stays shielded after the initial deposit boundary.

## Preconditions

- Kresko and Zebra binaries are built from the intended revisions.
- The experiment was initialized with `--network mainnet`.
- Cloud nodes are up, deployed, and fully synced to mainnet.
- You have a secure copy of `.kresko/txblast/recovery.json`.
- The transparent withdrawal address is known before starting.
- You have chosen a conservative block byte target and can afford the lane
  principal plus all fees.

Check that the experiment is a mainnet experiment:

```bash
rg '"network_kind"\s*:\s*"mainnet"' config.json
```

Create and deploy public-network observer nodes if needed:

```bash
kresko init \
  --chain-id mainnet-txblast \
  --experiment mainnet-txblast \
  --provider digitalocean \
  --network mainnet

cd mainnet-txblast
kresko add --node-type miner --count 4 --region random
kresko up -w 16
kresko genesis-public \
  --zebrad-binary /path/to/zebrad \
  --kresko-binary /path/to/kresko
kresko deploy -w 16
kresko status
```

Wait for `kresko status` to show synced mainnet nodes before funding or running.

## Choose The Block Target

`kresko txblast plan` takes `--target-block-bytes`, not a percentage. Pick a
byte target from the block share you want to occupy:

```text
target_block_bytes = reference_block_bytes * target_share
```

For example, with a 2,000,000-byte reference block:

```text
1%  = 20,000 bytes/block
5%  = 100,000 bytes/block
10% = 200,000 bytes/block
```

Use a small value first. The default measured Orchard tx size is 3,000 bytes,
so a 100,000 bytes/block plan is roughly 34 Orchard transactions per block
globally. With four nodes, each node starts at roughly 8 or 9 transactions per
block.

## Create The Txblast Wallet

Create the public txblast wallet in the experiment directory:

```bash
kresko txblast wallet init \
  --network mainnet \
  --rpc-endpoint http://<node-ip>:8232 \
  --lanes-per-node 100 \
  --lane-value-zats 30000 \
  --fanout-width 1 \
  --require-mainnet-confirmation
```

When `--birthday-height` is omitted, wallet init records the current RPC chain
height as the wallet birthday so later Orchard rescans do not start at genesis.

Back up the recovery bundle immediately:

```bash
cp -a .kresko/txblast/recovery.json /secure/offline/location/recovery.json
```

Print the control deposit address:

```bash
kresko txblast deposit address
```

Send one or more deposits to that address. Multiple deposits are supported; the
next prepare cycle will pick up eligible confirmed deposits.

Then wait for confirmations and check deposit status:

```bash
kresko txblast deposit status --confirmations 3
```

When indexed RPC is available, status auto-imports confirmed control-address
UTXOs as planning hints. `plan` also auto-syncs deposits before checking
funding. These imported deposits are not trusted by themselves: `prepare`
verifies spendable control-address UTXOs through node RPC before spending
anything.

If indexed RPC is unavailable, manually import the known deposit:

```bash
kresko txblast deposit import \
  --txid <deposit-txid> \
  --vout <output-index> \
  --amount-zats <amount-zats>
```

## Plan Funding And Rate

Create a plan for the intended block byte budget:

```bash
kresko txblast plan \
  --nodes all \
  --target-block-bytes 100000 \
  --block-spacing-secs 75 \
  --duration-secs 900 \
  --measured-tx-bytes 3000 \
  --safety-margin 0.20
```

Read the printed `required` zats values. If imported deposits are below the
requirement, either deposit more funds and re-run planning, or explicitly record
an underfunded plan:

```bash
kresko txblast plan \
  --nodes all \
  --target-block-bytes 100000 \
  --duration-secs 900 \
  --allow-underfunded-plan
```

Use underfunded plans only for short or intentionally partial runs.

## Prepare Shielded Inventory

Run prepare in stages. The command is idempotent and should be rerun after each
submitted transaction confirms.

```bash
kresko txblast prepare --plan <plan-id>
```

Expected progression:

1. First run: confirmed transparent deposits are shielded into the control
   Orchard inventory. The command prints shielding txids and exits.
2. After shielding confirms: rerun the same command. If the control reservoir
   topology is too coarse for the selected lane count and fanout width,
   `prepare` submits control reservoir split txids and exits.
3. After any reservoir split confirms: rerun the same command. Control Orchard notes are
   fanned out to per-node hot Orchard lane inventory. The command prints fanout
   txids and exits.
4. After fanout confirms: rerun the same command. Kresko scans the hot
   inventory, installs the hot keys on the remote nodes, and writes
   `.kresko/txblast/prepared.latest.json`.

If you add another deposit later, rerun `deposit status`, then rerun
`prepare --plan <plan-id>`. It will shield newly confirmed deposits and top up
only the per-node lane deficits.

## Start The Mainnet Run

Start with explicit pending and byte caps:

```bash
kresko txblast run \
  --plan <plan-id> \
  --target-block-bytes 100000 \
  --max-pending-txs 8 \
  --trace-dir /root/.cache/kresko/txblast-traces \
  --mainnet-i-understand-fees
```

Useful rate overrides:

```bash
# Cap the whole fleet directly.
kresko txblast run \
  --plan <plan-id> \
  --max-global-bytes-per-sec 1000 \
  --max-pending-txs 4 \
  --mainnet-i-understand-fees

# Cap each node directly.
kresko txblast run \
  --plan <plan-id> \
  --max-node-bytes-per-sec 250 \
  --max-pending-txs 4 \
  --mainnet-i-understand-fees
```

`--max-mempool-bytes` and `--feedback-window-blocks` are recorded by the public
runner, but current `txblast-local` does not enforce mempool feedback control.
Use direct byte and pending caps for the actual safety limit.

## Monitor

Watch Kresko and Zebra status:

```bash
kresko status
kresko download -n all -w 16 traces
kresko download -n all -w 16 heights -b 16
```

Remote sessions and logs:

```bash
kresko exec -m all -c 'tmux ls || true' --with-output
kresko exec -m all -c 'tail -n 200 /root/kresko-txblast.log' --with-output
kresko exec -m all -c 'ls -lah /root/.cache/kresko/txblast-traces' --with-output
```

For long runs, periodically check:

- node sync and RPC health;
- pending transaction count;
- observed block sizes;
- remaining shielded inventory;
- mempool behavior from independent mainnet nodes, if available.

## Stop Without Sweeping

Stop remote txblast sessions when the target window is complete:

```bash
kresko kill-session --session txblast
```

`kresko txblast stop` is currently a guarded placeholder for the public runner
control channel. Use `kill-session` for active tmux-managed txblast agents.

After stopping, wait for pending txblast transactions to confirm before
withdrawing or recovery-sweeping.

## Withdraw Remaining Funds

Dry-run first:

```bash
kresko txblast withdraw \
  --to <mainnet-transparent-address> \
  --amount all \
  --dry-run \
  --mainnet-i-understand-finality
```

Sweep all available shielded inventory, net of withdrawal fees:

```bash
kresko txblast withdraw \
  --to <mainnet-transparent-address> \
  --amount all \
  --mainnet-i-understand-finality
```

Withdraw a specific amount in zats:

```bash
kresko txblast withdraw \
  --to <mainnet-transparent-address> \
  --amount 1000000 \
  --mainnet-i-understand-finality
```

Explicit withdrawals are planned before submission. If available shielded notes
cannot fund the requested amount plus fees, no withdrawal transaction is sent.

Multiple withdrawals are supported. Wait for each sweep to confirm before
submitting another sweep from the same inventory.

## Recovery

Use recovery if local state is lost, prepare/run state is confusing, or you need
an emergency sweep from the recovery bundle. Pick a `--from-height` before the
first deposit shielding transaction, or use the wallet birthday if it is safely
early.

Inventory report:

```bash
kresko txblast recover inventory \
  --from-height <height-before-first-deposit> \
  --json
```

Emergency sweep:

```bash
kresko txblast recover sweep \
  --to <mainnet-transparent-address> \
  --from-height <height-before-first-deposit> \
  --dry-run \
  --mainnet-i-understand-recovery

kresko txblast recover sweep \
  --to <mainnet-transparent-address> \
  --from-height <height-before-first-deposit> \
  --mainnet-i-understand-recovery
```

Do not run normal withdrawal and recovery sweep at the same time.

## Repeat Runs And Top-Ups

For another block-fill window:

1. Stop the previous run and wait for pending transactions to confirm.
2. Add another deposit if inventory is low.
3. Optionally import the new deposit as a planning hint.
4. Rerun `kresko txblast plan` if changing target, duration, selected nodes, or
   lane parameters.
5. Rerun `kresko txblast prepare --plan <plan-id>` until it finalizes.
6. Start `kresko txblast run` again with the intended caps.

The state under `.kresko/txblast/` records confirmed deposits, submitted
shielding/fanout/sweep txids, inventory summaries, and pending transaction
resume status. Do not edit it during an active run.

## Cleanup

After all funds are withdrawn and logs are collected:

```bash
kresko download -n all -w 16
kresko kill-session --session txblast
kresko kill-session --session app
kresko down
```

Keep `wallet.json`, `state.json`, `prepared-*.json`, and `recovery.json` until
you have independently verified the withdrawal or recovery sweep outputs.
