# Regenerate NU7 Join Bundle

Use this from the Kresko `giga-refactor` worktree:

```bash
cd /home/evan/src/zcash/kresko.giga-refactor
```

This runbook assumes the NU7 Vultr run directory is:

```bash
RUN_DIR=/home/evan/src/zcash/kresko.giga-refactor/.kresko/runs/nu7-pow-vultr-4/nu7-pow-vultr-4-20260508-1952
OUT_DIR=/tmp/nu7-join-bundle
BUNDLE_TGZ=/tmp/nu7-join-bundle.tar.gz
```

## 1. Check The Run Payload

The run must already have the generated local-genesis payload:

```bash
test -f "$RUN_DIR/config.json"
test -f "$RUN_DIR/payload/local_genesis/genesis.hex"
test -f "$RUN_DIR/payload/local_genesis/premine_blocks.hex"
test -f "$RUN_DIR/payload/local_genesis/checkpoints.txt"
```

Do not package `payload/local_genesis/funded_keys.json` or any per-node
`funded_key.json` files. The join bundle must only contain public chain data,
config, and scripts.

## 2. Verify Downloadable Source Refs

The join machine builds from public Git URLs. Make sure the refs match the
network binaries you deployed.

For the current NU7 run, use `valargroup/zebra`; the same branch on
`evan-forbes/zebra` was stale during testing, and `ZcashFoundation/zebra` did
not expose this branch.

The `evan/nu7/testnet` ref now also carries the `zebra-jsonl-trace` crate, so a
single Zebra checkout supplies `zebra-chain`, `zebrad`, and `zebra-jsonl-trace`.

```bash
git -C /home/evan/src/zcash/nu7-testnet rev-parse HEAD
git ls-remote --heads https://github.com/valargroup/zebra.git evan/nu7/testnet
git ls-remote --heads https://github.com/evan-forbes/kresko.git giga-refactor
```

The Zebra `ls-remote` SHA should match the local/deployed Zebra SHA.

## 3. Build Kresko Locally

```bash
CXXFLAGS='-include cstdint' cargo build --release --bin kresko
```

## 4. Generate And Package The Bundle

```bash
rm -rf "$OUT_DIR" "$BUNDLE_TGZ"

target/release/kresko join-bundle \
  --run-dir "$RUN_DIR" \
  --zebra-git-url https://github.com/valargroup/zebra.git \
  --zebra-ref evan/nu7/testnet \
  --kresko-git-url https://github.com/evan-forbes/kresko.git \
  --kresko-ref giga-refactor \
  --out "$OUT_DIR"

bash "$OUT_DIR/join-nu7-testnet.sh" --dry-run
tar -C "$OUT_DIR" -czf "$BUNDLE_TGZ" .
sha256sum "$BUNDLE_TGZ"
```

Expected bundle contents:

```text
join-manifest.json
join-nu7-testnet.sh
zebrad.join.toml
local_genesis/checkpoints.txt
local_genesis/genesis.hex
local_genesis/premine_blocks.hex
```

Check that no funded keys were packaged:

```bash
grep -R -n -E 'secret_key_hex|funded_key|tmDZy4Hm|tmWKTk|tmJXZk|tmLtDN' "$OUT_DIR" || true
```

This should print nothing.

## 5. Publish The Bundle

Upload `$BUNDLE_TGZ` to an HTTPS-accessible location such as S3, Spaces, or any
static file host.

Record:

```bash
sha256sum "$BUNDLE_TGZ"
jq '{genesis_hash, seeded_tip_hash, zebra_git_url, zebra_ref, kresko_git_url, kresko_ref, bootstrap_peers}' "$OUT_DIR/join-manifest.json"
```

## 6. Join From A Fresh Ubuntu Host

Observer-only:

```bash
bash scripts/join-nu7-testnet.sh \
  --bundle-url https://example.com/nu7-join-bundle.tar.gz
```

Mining:

```bash
bash scripts/join-nu7-testnet.sh \
  --bundle-url https://example.com/nu7-join-bundle.tar.gz \
  --mine
```

Mining with a spendable supplied recipient:

```bash
bash scripts/join-nu7-testnet.sh \
  --bundle-url https://example.com/nu7-join-bundle.tar.gz \
  --mine \
  --miner-address t2...
```

If `--mine` is used without `--miner-address`, the generated join script creates
a random local testnet P2SH recipient. That is fine for proving mining works,
but it does not save a spend key. Use `--miner-address` when rewards need to be
spendable.

## 7. Runtime Locations On The Join Host

```text
/opt/nu7-testnet                Zebra checkout, bundle, state, helper scripts
/opt/nu7-join-src/kresko        Kresko source checkout used for --mine
/opt/nu7-join-src/nu7-testnet   Symlink to the Zebra source checkout
/opt/nu7-join-src/zebra         zebra-jsonl-trace source checkout
/root/.config/zebrad.toml       Runtime Zebra config
/var/log/nu7-testnet            Join, bootstrap, zebrad, and mining logs
tmux session nu7-zebrad         Running Zebra node
tmux session nu7-mine           Running Kresko miner, only with --mine
```

## 8. Verify The Joined Node

On the join host:

```bash
tmux ls

curl -sS -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]}' \
  http://127.0.0.1:18232 | jq

curl -sS -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":2,"method":"getblockhash","params":[0]}' \
  http://127.0.0.1:18232 | jq -r '.result'

tail -n 40 /var/log/nu7-testnet/zebrad.log
tail -n 40 /var/log/nu7-testnet/mine.log
```

The `getblockhash 0` result must equal `join-manifest.json.genesis_hash`.

For mining mode, `mine.log` should show `Block submitted` lines and `zebrad.log`
should show accepted submitted blocks.

## 9. Known Pitfalls

- Do not use a Zebra Git URL/ref that differs from the deployed network binary.
  A stale branch can build successfully but fail to sync with the running NU7
  network.
- Do not reset the whole experiment when testing a join bundle. Reset or clear
  only the intended join-test node.
- `payload/local_genesis/funded_keys.json` exists in the run payload, but the
  join bundle must not include it.
- The join script builds Zebra from source. There is intentionally no prebuilt
  Zebra binary path in the bundle.
- Mining mode also builds Kresko from source and recreates Kresko's expected
  sibling source layout because `giga-refactor` has local path dependencies.
