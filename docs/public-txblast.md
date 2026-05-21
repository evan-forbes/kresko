# Public-Network Txblast

Public-network txblast is a staged, shielded lifecycle for public testnet and
mainnet experiments. It is separate from local-genesis txblast: local-genesis
can rely on premine-funded transparent keys, while public networks must treat
transparent funds only as the initial deposit and final withdrawal boundary.

## Lifecycle

1. Create a wallet:

   ```bash
   kresko txblast wallet init --network public-testnet --rpc-endpoint http://<node-ip>:8232
   ```

   Mainnet requires `--require-mainnet-confirmation`.
   When `--birthday-height` is omitted, wallet init records the current RPC
   chain height as the wallet birthday if RPC is available.

2. Send one or more deposits to the printed control transparent address.

3. Check deposit status:

   ```bash
   kresko txblast deposit status --confirmations 3
   ```

   When indexed RPC is available, status auto-imports confirmed control-address
   UTXOs for planning. `prepare` still verifies spendable control UTXOs through
   node RPC before spending them. If indexed RPC is unavailable, manually import
   the known deposit with `kresko txblast deposit import --txid <txid> --vout
   <n> --amount-zats <zats>`.

4. Create a plan:

   ```bash
   kresko txblast plan --nodes all --target-block-bytes 1000000
   ```

5. Prepare shielded inventory:

   ```bash
   kresko txblast prepare --plan <plan-id>
   ```

   `prepare` is intentionally resumable and may need multiple invocations:

   - confirmed control-address UTXOs are shielded into the control Orchard
     inventory;
   - after those shielding transactions confirm, control Orchard notes are
     fanned out to per-node hot Orchard lane inventory;
   - after fanout confirms and scans back, hot keys are installed on the remote
     nodes and `.kresko/txblast/prepared.latest.json` is written.

6. Run:

   ```bash
   kresko txblast run --plan <plan-id>
   ```

   Mainnet requires `--mainnet-i-understand-fees`.

7. Withdraw or recover:

   ```bash
   kresko txblast withdraw --to <transparent-address> --amount all
   kresko txblast recover inventory --from-height <height>
   kresko txblast recover sweep --to <transparent-address> --from-height <height>
   ```

   Mainnet withdrawal and recovery sweep require their explicit mainnet guard
   flags.

## Shielding Model

The durable funding path stays Orchard-shielded after the initial transparent
deposit is shielded. Runtime agents spend and replenish Orchard lane inventory.
Withdrawal and recovery sweep spend Orchard notes directly to the requested
transparent address.

`prepare` supports multiple deposits and repeated top-up fanouts. It scans
existing hot Orchard lane inventory first, then fans out only the per-node lane
count or value deficit for the selected plan.

## State Files

Public txblast state lives under `.kresko/txblast/`:

- `wallet.json`: public wallet metadata and planning hints.
- `recovery.json`: private recovery bundle; keep this file secret.
- `state.json`: confirmed deposits, scanned control/hot inventory summaries,
  submitted/confirmed shield, fanout, and sweep transaction ids, plus pending
  transaction resume state.
- `prepared-<plan-id>.json` and `prepared.latest.json`: finalized prepare
  records used by `run`.

The note summaries in `state.json` are not sufficient to spend by themselves.
Spending commands rescan from the wallet birthday or supplied recovery height
using `recovery.json`.

## Operational Notes

- Do not edit `state.json` while transactions are pending unless you are doing
  an emergency recovery.
- If `prepare` reports pending public txblast transactions, wait for
  confirmation and rerun the same command.
- `withdraw --amount all` sweeps the maximum net amount after per-note fees.
- Explicit withdrawals are preplanned before any transaction is submitted; if
  available shielded notes cannot fund the full requested amount plus fees, no
  withdrawal transaction is sent.
- Use a birthday or recovery `--from-height` before the first deposit height to
  avoid missing Orchard notes.
