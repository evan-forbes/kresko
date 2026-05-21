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

The join flow now downloads **prebuilt binaries** from GitHub releases instead
of building Zebra or Kresko from source. The bundle is data-only (genesis seed
blocks, the runtime `zebrad.join.toml`, and a manifest); the join script
(`scripts/join-nu7-testnet.sh`) reads the release coordinates from the manifest
and `curl`s the matching `zebrad` and `kresko` binaries.

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
config, and the manifest.

## 2. Verify The Release Tags

The join machine downloads released binaries. Make sure the release tags match
the network binaries you deployed:

- Zebra (zebrad): <https://github.com/valargroup/zebra/releases> — currently
  `nu7-testnet-v0.1.0` (asset `zebra-<tag>-x86_64-unknown-linux-gnu.tar.gz`).
- Kresko (miner): <https://github.com/valargroup/kresko/releases> — currently
  `v0.1.0` (asset `kresko-<tag>-x86_64-linux-gnu`).

```bash
gh release view nu7-testnet-v0.1.0 --repo valargroup/zebra  --json assets --jq '.assets[].name'
gh release view v0.1.0             --repo valargroup/kresko --json assets --jq '.assets[].name'
```

The deployed network must be running the same binaries these tags publish.

## 3. Build Kresko Locally

```bash
CXXFLAGS='-include cstdint' cargo build --release --bin kresko
```

## 4. Generate And Package The Bundle

The release coordinates default to `valargroup/zebra @ nu7-testnet-v0.1.0` and
`valargroup/kresko @ v0.1.0`; override them only if you cut new releases.

```bash
rm -rf "$OUT_DIR" "$BUNDLE_TGZ"

target/release/kresko join-bundle \
  --run-dir "$RUN_DIR" \
  --zebra-repo valargroup/zebra \
  --zebra-release-tag nu7-testnet-v0.1.0 \
  --kresko-repo valargroup/kresko \
  --kresko-release-tag v0.1.0 \
  --out "$OUT_DIR"

tar -C "$OUT_DIR" -czf "$BUNDLE_TGZ" .

# Validate the packaged bundle with the join script (no root needed).
bash scripts/join-nu7-testnet.sh --bundle-url "$BUNDLE_TGZ" --dry-run
sha256sum "$BUNDLE_TGZ"
```

Expected bundle contents (data-only — no script):

```text
join-manifest.json
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
jq '{genesis_hash, seeded_tip_hash, zebra_release_repo, zebra_release_tag, kresko_release_repo, kresko_release_tag, bootstrap_peers}' "$OUT_DIR/join-manifest.json"
```

## 6. Join From A Fresh Ubuntu Host

The join script is attached to every Kresko release. Download it and point it at
the published bundle. It fetches the prebuilt `zebrad`/`kresko` binaries named in
the bundle manifest, verifies their checksums, seeds the chain, and starts
zebrad.

Observer-only:

```bash
curl -fsSLO https://github.com/valargroup/kresko/releases/download/v0.1.0/join-nu7-testnet.sh
bash join-nu7-testnet.sh \
  --bundle-url https://example.com/nu7-join-bundle.tar.gz
```

Mining:

```bash
bash join-nu7-testnet.sh \
  --bundle-url https://example.com/nu7-join-bundle.tar.gz \
  --mine
```

Mining with a spendable supplied recipient:

```bash
bash join-nu7-testnet.sh \
  --bundle-url https://example.com/nu7-join-bundle.tar.gz \
  --mine \
  --miner-address t2...
```

If `--mine` is used without `--miner-address`, the join script creates a random
local testnet P2SH recipient. That is fine for proving mining works, but it does
not save a spend key. Use `--miner-address` when rewards need to be spendable.

## 7. Runtime Locations On The Join Host

```text
/usr/local/bin/zebrad           Prebuilt zebrad downloaded from the zebra release
/usr/local/bin/kresko           Prebuilt kresko downloaded from the kresko release (--mine only)
/opt/nu7-testnet/bundle         Extracted join bundle (manifest, config, seed blocks)
/opt/nu7-testnet/state          Zebra state cache
/root/.config/zebrad.toml       Runtime Zebra config
/var/log/nu7-testnet            Bootstrap, zebrad, and mining logs
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

- Do not use release tags that differ from the deployed network binaries. A
  mismatched build can start but fail to sync with the running NU7 network.
- Do not reset the whole experiment when testing a join bundle. Reset or clear
  only the intended join-test node.
- `payload/local_genesis/funded_keys.json` exists in the run payload, but the
  join bundle must not include it.
- The released binaries target `x86_64` Linux (glibc). For other architectures,
  cut a matching release first or build from source manually.
