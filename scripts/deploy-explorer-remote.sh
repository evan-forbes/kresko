#!/usr/bin/env bash
set -Eeuo pipefail

# Deploy the devdotbo/zcash-explorer on a standalone host, pointing it at a
# REMOTE Zebra full node's JSON-RPC (rather than the co-located host.docker
# .internal the kresko fleet layer assumes). Used to stand the explorer up on
# 159.223.167.52 against a live Vultr NU7 full node.
#
# Source delivery uses the S3-only rule: the operator uploads the explorer
# source tarball to S3 and the host curls a short-lived presigned URL. The
# secret .env (with SECRET_KEY_BASE) is written over the SSH session's stdin so
# it never touches S3.
#
# Usage:
#   scripts/deploy-explorer-remote.sh \
#     --host 159.223.167.52 \
#     --rpc-host 155.138.237.238 \
#     --source-url '<PRESIGNED_TARBALL_URL>'
#
# Optional: --rpc-port 18232 --public-port 20001 --remote-root /root/zcash-explorer
#           --key ~/.ssh/id_ed25519

HOST=""
RPC_HOST=""
SOURCE_URL=""
RPC_PORT=18232
PUBLIC_PORT=20001
REMOTE_ROOT=/root/zcash-explorer
SERVICE=explorer-testnet
SSH_KEY="${KRESKO_SSH_KEY_PATH:-$HOME/.ssh/id_ed25519}"

usage() { sed -n '3,27p' "$0"; }

while [ "$#" -gt 0 ]; do
    case "$1" in
        --host)        HOST="${2:?}"; shift 2;;
        --rpc-host)    RPC_HOST="${2:?}"; shift 2;;
        --source-url)      SOURCE_URL="${2:?}"; shift 2;;
        --source-url-file) SOURCE_URL="$(tr -d '\r\n' < "${2:?}")"; shift 2;;
        --rpc-port)    RPC_PORT="${2:?}"; shift 2;;
        --public-port) PUBLIC_PORT="${2:?}"; shift 2;;
        --remote-root) REMOTE_ROOT="${2:?}"; shift 2;;
        --key)         SSH_KEY="${2:?}"; shift 2;;
        -h|--help) usage; exit 0;;
        *) echo "unknown arg: $1" >&2; usage >&2; exit 2;;
    esac
done

[ -n "$HOST" ]       || { echo "missing --host" >&2; exit 2; }
[ -n "$RPC_HOST" ]   || { echo "missing --rpc-host" >&2; exit 2; }
[ -n "$SOURCE_URL" ] || { echo "missing --source-url" >&2; exit 2; }

SSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 -i "$SSH_KEY")
ssh_run() { ssh "${SSH_OPTS[@]}" "root@${HOST}" "bash -lc $(printf '%q' "$1")"; }

# 0. Pre-flight: confirm the operator can reach the target node's RPC over the
#    network from here (the explorer host needs the same reachability).
echo "=== pre-flight: probing ${RPC_HOST}:${RPC_PORT} RPC from operator ==="
if curl -fsS --max-time 5 -H 'Content-Type: application/json' \
     --data '{"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]}' \
     "http://${RPC_HOST}:${RPC_PORT}" | jq -e '.result.blocks' >/dev/null; then
    echo "RPC reachable."
else
    echo "WARNING: could not reach ${RPC_HOST}:${RPC_PORT} from operator; the explorer host may not either." >&2
fi

# 1. Install docker + curl if needed (+ swap guard for low-RAM build hosts).
echo "=== ensuring docker/compose (+swap) on ${HOST} ==="
ssh_run "set -e
export DEBIAN_FRONTEND=noninteractive
mkdir -p $(printf '%q' "$REMOTE_ROOT")
# Building the Phoenix/Elixir image can OOM under ~3GB RAM; add swap if scarce.
mem_mb=\$(free -m | awk '/Mem:/{print \$2}')
swap_mb=\$(free -m | awk '/Swap:/{print \$2}')
if [ \"\${mem_mb:-0}\" -lt 3000 ] && [ \"\${swap_mb:-0}\" -lt 1024 ] && [ ! -f /swapfile ]; then
  echo 'adding 3G swapfile for the build'
  fallocate -l 3G /swapfile || dd if=/dev/zero of=/swapfile bs=1M count=3072
  chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
fi
if ! command -v docker >/dev/null 2>&1 || ! docker compose version >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1; then
  apt-get update && apt-get install -y docker.io docker-compose-v2 curl
fi
systemctl enable --now docker >/dev/null 2>&1 || service docker start >/dev/null 2>&1 || true
docker compose version"

# 2. Fetch the source tarball from the presigned URL (preserving any existing .env).
echo "=== fetching explorer source on ${HOST} ==="
ssh_run "set -e
mkdir -p $(printf '%q' "$REMOTE_ROOT")
curl -fsSL $(printf '%q' "$SOURCE_URL") -o /tmp/zcash-explorer.tar.gz
find $(printf '%q' "$REMOTE_ROOT") -mindepth 1 -maxdepth 1 ! -name .env -exec rm -rf {} +
tar -xzf /tmp/zcash-explorer.tar.gz -C $(printf '%q' "$REMOTE_ROOT")
rm -f /tmp/zcash-explorer.tar.gz"

# 3. Write the secret .env over stdin (never via S3). SECRET_KEY_BASE generated locally.
echo "=== writing .env (ZCASHD_HOSTNAME=${RPC_HOST}) ==="
SECRET="$(openssl rand -hex 64 2>/dev/null || head -c 64 /dev/urandom | base64 | tr -d '\n=/+')"
ENV_CONTENT="$(cat <<ENV
ZCASHD_HOSTNAME=${RPC_HOST}
ZCASHD_PORT=${RPC_PORT}
ZCASHD_USERNAME=zcashrpc
ZCASHD_PASSWORD=changeme
ZCASH_NETWORK=testnet
LIGHTWALLETD_ENABLED=false
EXPLORER_SCHEME=http
EXPLORER_HOSTNAME=${HOST}
EXPLORER_PORT=${PUBLIC_PORT}
PORT=4000
TESTNET_EXPLORER_HOSTNAME=${HOST}
TESTNET_SECRET_KEY_BASE=${SECRET}
VK_CPUS=0.3
VK_MEM=1024M
VK_RUNNER_IMAGE=nighthawkapps/vkrunner
FAUCET_ENABLED=false
ENV
)"
printf '%s\n' "$ENV_CONTENT" | ssh "${SSH_OPTS[@]}" "root@${HOST}" \
    "mkdir -p $(printf '%q' "$REMOTE_ROOT") && cat > $(printf '%q' "$REMOTE_ROOT")/.env"

# 4. Build + start the testnet explorer service.
echo "=== docker compose up -d --build ${SERVICE} (this builds the Phoenix image; can take a few min) ==="
ssh_run "cd $(printf '%q' "$REMOTE_ROOT") && docker compose up -d --build $(printf '%q' "$SERVICE")"

# 5. Verify: container -> RPC reachability, then public HTTP.
echo "=== verifying ==="
ssh_run "cd $(printf '%q' "$REMOTE_ROOT") && docker compose ps $(printf '%q' "$SERVICE")"
ssh_run "cd $(printf '%q' "$REMOTE_ROOT") && docker compose exec -T $(printf '%q' "$SERVICE") sh -lc 'command -v nc >/dev/null 2>&1 && nc -z -w 3 $(printf '%q' "$RPC_HOST") $(printf '%q' "$RPC_PORT") && echo RPC_REACHABLE || echo rpc-check-skipped'"
ssh_run "code=000; for _ in \$(seq 1 18); do code=\$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 http://127.0.0.1:${PUBLIC_PORT}/ || true); case \$code in 200|302) break;; esac; sleep 5; done; echo \"local HTTP status: \$code\""

echo "=== done. Public URL: http://${HOST}:${PUBLIC_PORT} (RPC backend ${RPC_HOST}:${RPC_PORT}) ==="
