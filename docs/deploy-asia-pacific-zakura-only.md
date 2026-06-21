# Deploy asia-pacific-0 as Zakura-only

This runbook redeploys only `mainnet-zebra-snapshot/asia-pacific-0`
(`168.144.173.250`) from a Zebra commit, using S3 payload delivery. It keeps
the node as a pure Zakura scratch-sync test node and does not touch the other
snapshot fleet nodes.

## Inputs

Set these for each deploy:

```bash
COMMIT=1e0b819063ce7b5da5a7ba4da138e424e06c527a
SHORT=${COMMIT:0:8}
TARGET_IP=168.144.173.250
TARGET_NAME=asia-pacific-0
FLEET=mainnet-zebra-snapshot
PAYLOAD_TEMPLATE=/home/evan/.kresko/fleets/mainnet-zebra-snapshot/payload
ZEBRA_SOURCE=/home/evan/src/valar/zebra
KRESKO_ROOT=/home/evan/src/zcash/kresko.giga-refactor
TS=$(date -u +%Y%m%dT%H%M%SZ)
WORKTREE=/tmp/zebra-zakura-fixes-review-${SHORT}
PAYLOAD_WORK=/tmp/${FLEET}-zakura-only-${SHORT}
ARCHIVE=/tmp/${FLEET}-zakura-only-${SHORT}-${TS}.tar.gz
S3_KEY=kresko/${FLEET}/zakura-only-${SHORT}-${TS}.tar.gz
```

The required environment is the normal Kresko/S3 environment:

```bash
export AWS_S3_BUCKET=evan-talis
export AWS_S3_ENDPOINT=https://sfo3.digitaloceanspaces.com
export AWS_DEFAULT_REGION=sfo3
```

## 1. Build from a clean source tree

Do not build from `/home/evan/src/valar/zebra` if it has local changes, and do
not build from any branch worktree with staged or unstaged changes.

```bash
git -C "$ZEBRA_SOURCE" cat-file -t "$COMMIT"
git -C "$ZEBRA_SOURCE" show -s --format='%H%n%ci%n%s' "$COMMIT"
git -C "$ZEBRA_SOURCE" status --short --branch

rm -rf "$WORKTREE"
git clone "$ZEBRA_SOURCE" "$WORKTREE"
git -C "$WORKTREE" checkout --detach "$COMMIT"
git -C "$WORKTREE" status --short --branch

cd "$WORKTREE"
CARGO_TARGET_DIR=/home/evan/src/valar/zebra/target cargo xtask package ubuntu
```

Record the binary metadata:

```bash
"$WORKTREE/target/ubuntu/zebra" --version
ZEBRAD_SHA=$(sha256sum "$WORKTREE/target/ubuntu/zebra" | awk '{print $1}')
echo "$ZEBRAD_SHA"
```

## 2. Assemble the payload

Start from the existing snapshot payload, replace only `build/zebrad`, then
patch only `payload/asia-pacific/zebrad.toml`.

```bash
rm -rf "$PAYLOAD_WORK"
mkdir -p "$PAYLOAD_WORK"
cp -a "$PAYLOAD_TEMPLATE" "$PAYLOAD_WORK/payload"
cp -a "$WORKTREE/target/ubuntu/zebra" "$PAYLOAD_WORK/payload/build/zebrad"
```

Patch `"$PAYLOAD_WORK/payload/asia-pacific/zebrad.toml"` so the target is
Zakura-only:

```toml
[network]
initial_mainnet_peers = []
legacy_p2p = false
v2_p2p = true

[network.zakura]
listen_addr = "0.0.0.0:8234"
# keep bootstrap_peers populated with the other fleet Zakura peers

[network.zakura.block_sync]
replace_legacy_syncer = true
```

Leave the legacy `[network] listen_addr = "0.0.0.0:8233"` present if it already
exists. It is inert when `legacy_p2p = false`; deleting it is unnecessary and
can create config compatibility churn.

Update the payload manifest:

```bash
cat > "$PAYLOAD_WORK/payload/build/manifest.txt" <<EOF
kresko_sha256=$(sha256sum "$PAYLOAD_WORK/payload/build/kresko" | awk '{print $1}')
kresko_source=$KRESKO_ROOT
zebrad_build_command=CARGO_TARGET_DIR=/home/evan/src/valar/zebra/target cargo xtask package ubuntu
zebrad_commit=$COMMIT
zebrad_sha256=$ZEBRAD_SHA
zebrad_source=$WORKTREE
EOF
```

Verify before packaging:

```bash
rg -n 'legacy_p2p|v2_p2p|initial_mainnet_peers|replace_legacy_syncer|listen_addr|bootstrap_peers' \
  "$PAYLOAD_WORK/payload/asia-pacific/zebrad.toml"
sha256sum "$PAYLOAD_WORK/payload/build/zebrad" "$PAYLOAD_WORK/payload/build/kresko"
"$PAYLOAD_WORK/payload/build/zebrad" --version
```

## 3. Package and upload to S3

The archive should contain the contents of `payload/` at its root, not a nested
`payload/` directory.

```bash
tar -C "$PAYLOAD_WORK/payload" -czf "$ARCHIVE" .
tar -tzf "$ARCHIVE" | sed -n '1,40p'

aws s3 cp "$ARCHIVE" "s3://${AWS_S3_BUCKET}/${S3_KEY}" \
  --endpoint-url "$AWS_S3_ENDPOINT"

PAYLOAD_URL=$(aws s3 presign "s3://${AWS_S3_BUCKET}/${S3_KEY}" \
  --expires-in 3600 \
  --endpoint-url "$AWS_S3_ENDPOINT")
echo "$PAYLOAD_URL"
```

## 4. Deploy only asia-pacific-0

Confirm the SSH target first:

```bash
ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new root@"$TARGET_IP" hostname
```

It must print `asia-pacific-0`.

Run the one-node deploy. This resets `/root/.cache/zebra` intentionally.

```bash
cat >/tmp/deploy-asia-pacific-zakura-only.sh <<'REMOTE'
set -euo pipefail

archive="/tmp/kresko-zakura-only.tar.gz"
payload_root="/root/kresko/payload"
config_path="/root/.config/zebrad.toml"

: "${PAYLOAD_URL:?missing PAYLOAD_URL}"
: "${EXPECTED_ZEBRAD_SHA256:?missing EXPECTED_ZEBRAD_SHA256}"

echo "=== host ==="
hostname

echo "=== download payload ==="
mkdir -p /root/kresko /root/logs /root/traces
curl -fL --retry 3 --retry-connrefused --retry-delay 5 --connect-timeout 10 \
    -o "$archive" "$PAYLOAD_URL"

echo "=== extract payload ==="
rm -rf "$payload_root"
mkdir -p "$payload_root"
tar -xzf "$archive" -C "$payload_root"

echo "=== install binaries ==="
install -m 0755 "$payload_root/build/zebrad" /usr/local/bin/zebrad
install -m 0755 "$payload_root/build/kresko" /usr/local/bin/kresko

actual_zebrad_sha256="$(sha256sum /usr/local/bin/zebrad | awk '{print $1}')"
if [ "$actual_zebrad_sha256" != "$EXPECTED_ZEBRAD_SHA256" ]; then
    echo "ERROR: zebrad sha256 mismatch: got $actual_zebrad_sha256 expected $EXPECTED_ZEBRAD_SHA256" >&2
    exit 1
fi

echo "=== install config ==="
mkdir -p /root/.config
cp "$payload_root/asia-pacific/zebrad.toml" "$config_path"
kresko config strip-genesis-block-path "$config_path"

echo "=== preflight config ==="
grep -nE '^[[:space:]]*(initial_mainnet_peers|legacy_p2p|v2_p2p|replace_legacy_syncer)[[:space:]]*=' "$config_path"
grep -nE '^[[:space:]]*listen_addr[[:space:]]*=[[:space:]]*"0\.0\.0\.0:8234"' "$config_path"

echo "=== restart zebrad from scratch ==="
systemctl stop zebrad 2>/dev/null || true
pkill -x zebrad 2>/dev/null || true
rm -rf /root/.cache/zebra
mkdir -p /root/.cache/zebra
systemctl start zebrad

echo "=== post-start ==="
sleep 8
zebrad --version
sha256sum /usr/local/bin/zebrad
du -sh /root/.cache/zebra
systemctl --no-pager --full status zebrad | sed -n '1,18p'
ss -ltnp | grep -E ':(8232|8233|8234)[[:space:]]' || true
ss -lunp | grep -E ':(8232|8233|8234|8235)[[:space:]]' || true
REMOTE

ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new root@"$TARGET_IP" \
  "PAYLOAD_URL='$PAYLOAD_URL' EXPECTED_ZEBRAD_SHA256='$ZEBRAD_SHA' bash -s" \
  </tmp/deploy-asia-pacific-zakura-only.sh
```

## 5. Verify

Run these against only `asia-pacific-0`:

```bash
ssh root@"$TARGET_IP" 'zebrad --version; sha256sum /usr/local/bin/zebrad'

ssh root@"$TARGET_IP" \
  "grep -nE '^[[:space:]]*(initial_mainnet_peers|legacy_p2p|v2_p2p|replace_legacy_syncer)[[:space:]]*=' /root/.config/zebrad.toml"

ssh root@"$TARGET_IP" \
  "ss -ltnp | grep -E ':(8232|8233|8234)[[:space:]]' || true; ss -lunp | grep -E ':(8232|8233|8234|8235)[[:space:]]' || true"

ssh root@"$TARGET_IP" \
  "du -sh /root/.cache/zebra; curl -sS --max-time 5 --data-binary '{\"jsonrpc\":\"1.0\",\"id\":\"verify\",\"method\":\"getblockchaininfo\",\"params\":[]}' -H 'content-type: text/plain;' http://127.0.0.1:8232/"

ssh root@"$TARGET_IP" \
  "grep -E 'legacy P2P disabled|Zakura P2P endpoint ready|Zakura block sync is replacing|Zakura block sync committed|initial sync|invalid_peer_range|panic|error' /root/logs/zebra-tracing.log | tail -n 160"
```

Expected listener shape:

- TCP `8232` present.
- TCP `8233` absent.
- UDP `8234` present. Zakura uses UDP here; do not expect a TCP `8234`
  listener.

Expected log lines:

- `legacy P2P disabled; not opening Zcash protocol listener`
- `Zakura P2P endpoint ready`
- `Zakura block sync is replacing the legacy ChainSync body downloader`

Verify the rest of the fleet remains untouched:

```bash
cd "$KRESKO_ROOT"
PYTHONPATH=. python fleets/mainnet_zebra_snapshot.py status
```

Only `asia-pacific-0` should be at scratch height. The other snapshot nodes
should remain reachable and synced.

## 6. Optional debug bundle

If the node is stuck, collect a package for another agent before changing
anything:

```bash
DEBUG_TS=$(date -u +%Y%m%dT%H%M%SZ)
DEBUG_NAME=asia-pacific-0-zakura-only-${SHORT}-debug-${DEBUG_TS}

ssh root@"$TARGET_IP" "name='$DEBUG_NAME'; root=/root/kresko/debug/\$name; archive=/root/kresko/debug/\$name.tgz; \
mkdir -p \"\$root\"/{rpc,traces,logs}; \
hostname >\"\$root/hostname.txt\"; date -u +%Y-%m-%dT%H:%M:%SZ >\"\$root/collected-at-utc.txt\"; \
zebrad --version >\"\$root/zebrad-version.txt\" 2>&1 || true; \
sha256sum /usr/local/bin/zebrad /usr/local/bin/kresko >\"\$root/binary-sha256.txt\" 2>&1 || true; \
cp /root/.config/zebrad.toml \"\$root/zebrad.toml\" 2>/dev/null || true; \
cp /root/kresko/payload/build/manifest.txt \"\$root/payload-manifest.txt\" 2>/dev/null || true; \
systemctl --no-pager --full status zebrad >\"\$root/systemctl-status-zebrad.txt\" 2>&1 || true; \
systemctl cat zebrad >\"\$root/systemctl-cat-zebrad.txt\" 2>&1 || true; \
journalctl -u zebrad --since '45 minutes ago' --no-pager >\"\$root/journal-zebrad-last-45m.log\" 2>&1 || true; \
ss -ltnp >\"\$root/ss-ltnp.txt\" 2>&1 || true; ss -lunp >\"\$root/ss-lunp.txt\" 2>&1 || true; \
du -sh /root/.cache/zebra /root/logs /root/traces/zakura >\"\$root/du-key-paths.txt\" 2>&1 || true; \
for method in getblockchaininfo getnetworkinfo getpeerinfo getinfo; do curl -sS --max-time 10 --data-binary \"{\\\"jsonrpc\\\":\\\"1.0\\\",\\\"id\\\":\\\"debug\\\",\\\"method\\\":\\\"\$method\\\",\\\"params\\\":[]}\" -H 'content-type: text/plain;' http://127.0.0.1:8232/ >\"\$root/rpc/\$method.json\" 2>\"\$root/rpc/\$method.stderr\" || true; done; \
tail -n 20000 /root/logs/zebra-tracing.log >\"\$root/logs/zebra-tracing-tail-20000.log\" 2>&1 || true; \
tail -n 2000 /root/logs/zebrad.log >\"\$root/logs/zebrad-tail-2000.log\" 2>&1 || true; \
for f in /root/traces/zakura/*.jsonl; do [ -f \"\$f\" ] && cp \"\$f\" \"\$root/traces/\" 2>&1 || true; done; \
tar -czf \"\$archive\" -C \"\$(dirname \"\$root\")\" \"\$(basename \"\$root\")\"; printf '%s\n' \"\$archive\""

mkdir -p "$KRESKO_ROOT/debug/$DEBUG_NAME"
scp root@"$TARGET_IP":"/root/kresko/debug/${DEBUG_NAME}.tgz" "$KRESKO_ROOT/debug/$DEBUG_NAME/"
tar -xzf "$KRESKO_ROOT/debug/$DEBUG_NAME/${DEBUG_NAME}.tgz" -C "$KRESKO_ROOT/debug/$DEBUG_NAME"
```

## Notes

- Do not run `fleet.deploy` without a selector for this workflow; this runbook
  intentionally uses direct SSH to one IP after S3 upload.
- Zakura's native endpoint is UDP. `ss -ltnp` alone will miss it; always check
  `ss -lunp`.
- A deployed pure-Zakura node can still log "syncer task" because some top-level
  Zebra orchestration names are legacy. The important line is `Zakura block sync
  is replacing the legacy ChainSync body downloader`.
- If sync remains at height 0, inspect `traces/header_sync*.jsonl` for
  `header_range_rejected` and `traces/legacy_request*.jsonl` for `BlocksByHash`
  responses before redeploying again.
