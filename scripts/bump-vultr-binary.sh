#!/usr/bin/env bash
set -Eeuo pipefail

# Bump the live Vultr NU7 miners to a new zebrad binary IN PLACE, keeping the
# existing chain state at /root/.cache/zebra.
#
# Distribution follows the S3-only rule: the operator uploads the binary to S3
# and each node curls a short-lived presigned URL. This script never scp/rsyncs.
#
# Per node it: downloads + checksum-verifies the new binary, stops the running
# `zebra` and `mine` tmux sessions (releasing the state DB lock), atomically
# replaces /usr/local/bin/zebrad, restarts zebrad against the existing config +
# state, restarts the miner, then verifies zebrad is alive and RPC answers.
#
# Usage:
#   scripts/bump-vultr-binary.sh --url <PRESIGNED_URL> --sha256 <HEX> [--node IP]...
#
# With no --node flags it targets the four known Vultr miners. The presigned URL
# is passed on the command line (not committed) and expires, so regenerate it
# with `aws s3 presign` if it lapses.

URL=""
SHA256=""
SSH_KEY="${KRESKO_SSH_KEY_PATH:-$HOME/.ssh/id_ed25519}"
RPC_PORT="${KRESKO_RPC_PORT:-18232}"
NODES=()

usage() { sed -n '3,25p' "$0"; }

while [ "$#" -gt 0 ]; do
    case "$1" in
        --url)      URL="${2:?missing url}"; shift 2;;
        --url-file) URL="$(tr -d '\r\n' < "${2:?missing url-file}")"; shift 2;;
        --sha256) SHA256="${2:?missing sha256}"; shift 2;;
        --node)   NODES+=("${2:?missing node}"); shift 2;;
        --key)    SSH_KEY="${2:?missing key}"; shift 2;;
        -h|--help) usage; exit 0;;
        *) echo "unknown arg: $1" >&2; usage >&2; exit 2;;
    esac
done

[ -n "$URL" ]    || { echo "missing --url" >&2; exit 2; }
[ -n "$SHA256" ] || { echo "missing --sha256" >&2; exit 2; }

if [ "${#NODES[@]}" -eq 0 ]; then
    NODES=(155.138.237.238 95.179.198.212 149.248.55.37 104.207.148.61)
fi

SSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 -i "$SSH_KEY")

# The remote routine, parameterized by the presigned URL and expected hash. It
# is intentionally idempotent and self-checking so a re-run is safe.
remote_script() {
    cat <<REMOTE
set -Eeuo pipefail
URL='$URL'
WANT='$SHA256'
RPC_PORT='$RPC_PORT'
RPC_URL="http://127.0.0.1:\${RPC_PORT}"

echo "--- \$(hostname) (\$(uname -m)) ---"
old_hash=\$( [ -f /usr/local/bin/zebrad ] && sha256sum /usr/local/bin/zebrad | awk '{print \$1}' || echo missing )
echo "current zebrad sha256: \$old_hash"
if [ "\$old_hash" = "\$WANT" ]; then
    echo "already running the target binary; will still restart to be sure"
fi

echo "downloading new binary..."
curl -fL --retry 3 "\$URL" -o /usr/local/bin/zebrad.new
got=\$(sha256sum /usr/local/bin/zebrad.new | awk '{print \$1}')
if [ "\$got" != "\$WANT" ]; then
    echo "CHECKSUM MISMATCH: got \$got want \$WANT" >&2
    rm -f /usr/local/bin/zebrad.new
    exit 1
fi
chmod 0755 /usr/local/bin/zebrad.new
echo "version check: \$(/usr/local/bin/zebrad.new --version 2>&1 | head -1)"

echo "stopping zebra/mine tmux sessions and any lingering procs..."
tmux kill-session -t mine 2>/dev/null || true
tmux kill-session -t zebra 2>/dev/null || true
tmux kill-session -t txblast 2>/dev/null || true
pkill -x zebrad 2>/dev/null || true
pkill -x kresko 2>/dev/null || true
# wait for the RocksDB lock under /root/.cache/zebra to be released
for _ in \$(seq 1 15); do pgrep -x zebrad >/dev/null 2>&1 || break; sleep 1; done

echo "swapping binary..."
install -m 0755 /usr/local/bin/zebrad.new /usr/local/bin/zebrad
rm -f /usr/local/bin/zebrad.new

echo "restarting zebrad (keeping state at /root/.cache/zebra)..."
mkdir -p /root/logs
tmux new-session -d -s zebra "bash -lc 'set -a; . /root/.kresko/env 2>/dev/null || true; set +a; zebrad -c /root/.config/zebrad.toml start 2>&1 | tee -a /root/logs/zebrad.log; exec bash -i'"

if [ -x /root/.kresko/miner-wait.sh ]; then
    echo "restarting miner..."
    tmux new-session -d -s mine "bash -lc 'set -a; . /root/.kresko/env 2>/dev/null || true; set +a; bash /root/.kresko/miner-wait.sh 2>&1 | tee -a /root/logs/miner.log; exec bash -i'"
else
    echo "no /root/.kresko/miner-wait.sh found; skipping miner restart" >&2
fi

echo "verifying zebrad stays up + RPC answers..."
sleep 5
if ! tmux has-session -t zebra 2>/dev/null || ! pgrep -x zebrad >/dev/null 2>&1; then
    echo "ZEBRAD DID NOT STAY UP. Tail of /root/logs/zebrad.log:" >&2
    tail -n 40 /root/logs/zebrad.log >&2 || true
    exit 1
fi
height=""
for _ in \$(seq 1 30); do
    resp=\$(curl -sS --max-time 3 -H 'Content-Type: application/json' \
        --data '{"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]}' \
        "\$RPC_URL" 2>/dev/null || true)
    height=\$(printf '%s' "\$resp" | jq -r '.result.blocks // empty' 2>/dev/null || true)
    [ -n "\$height" ] && break
    sleep 2
done
new_hash=\$(sha256sum /usr/local/bin/zebrad | awk '{print \$1}')
if [ -n "\$height" ]; then
    echo "OK: zebrad up, installed sha256=\$new_hash, RPC height=\$height"
else
    echo "WARN: zebrad process is up (sha256=\$new_hash) but RPC not answering yet; check /root/logs/zebrad.log" >&2
fi
REMOTE
}

fail=0
for ip in "${NODES[@]}"; do
    echo "========================================================"
    echo "=== bumping $ip"
    echo "========================================================"
    if ssh "${SSH_OPTS[@]}" "root@${ip}" "bash -s" <<<"$(remote_script)"; then
        echo "=== $ip: done"
    else
        echo "=== $ip: FAILED (see output above)" >&2
        fail=1
    fi
done

echo "========================================================"
if [ "$fail" -eq 0 ]; then
    echo "all nodes bumped successfully"
else
    echo "one or more nodes failed; review output above" >&2
fi
exit "$fail"
