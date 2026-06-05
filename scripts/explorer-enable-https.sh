#!/usr/bin/env bash
set -Eeuo pipefail

# Front the remote zcash-explorer with Caddy so it serves on a domain over
# HTTPS with no port. Installs Caddy on the explorer host (auto Let's Encrypt
# TLS), reverse-proxies to the explorer container, and updates the explorer's
# Phoenix url env (EXPLORER_SCHEME/HOSTNAME/PORT + TESTNET_EXPLORER_HOSTNAME)
# so links and the LiveView websocket work over the domain. SECRET_KEY_BASE is
# preserved (not regenerated), so existing sessions keep working.
#
# Usage:
#   scripts/explorer-enable-https.sh --host 159.223.167.52 --domain explorer.example.com
#
# Optional: --backend-port 20001 --remote-root /root/zcash-explorer
#           --service explorer-testnet --email you@example.com --key ~/.ssh/id_ed25519

HOST=""
DOMAIN=""
BACKEND_PORT=20001
REMOTE_ROOT=/root/zcash-explorer
SERVICE=explorer-testnet
EMAIL=""
SSH_KEY="${KRESKO_SSH_KEY_PATH:-$HOME/.ssh/id_ed25519}"

usage() { sed -n '3,18p' "$0"; }

while [ "$#" -gt 0 ]; do
    case "$1" in
        --host)         HOST="${2:?}"; shift 2;;
        --domain)       DOMAIN="${2:?}"; shift 2;;
        --backend-port) BACKEND_PORT="${2:?}"; shift 2;;
        --remote-root)  REMOTE_ROOT="${2:?}"; shift 2;;
        --service)      SERVICE="${2:?}"; shift 2;;
        --email)        EMAIL="${2:?}"; shift 2;;
        --key)          SSH_KEY="${2:?}"; shift 2;;
        -h|--help) usage; exit 0;;
        *) echo "unknown arg: $1" >&2; usage >&2; exit 2;;
    esac
done

[ -n "$HOST" ]   || { echo "missing --host" >&2; exit 2; }
[ -n "$DOMAIN" ] || { echo "missing --domain" >&2; exit 2; }

SSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 -i "$SSH_KEY")
ssh_run() { ssh "${SSH_OPTS[@]}" "root@${HOST}" "bash -lc $(printf '%q' "$1")"; }

# 0. Pre-flight: confirm the domain's A record actually points at this host, or
#    Caddy's ACME challenge will fail.
echo "=== pre-flight: does ${DOMAIN} resolve to ${HOST}? ==="
resolved="$(getent hosts "$DOMAIN" | awk '{print $1}' | head -1 || true)"
if [ "$resolved" = "$HOST" ]; then
    echo "DNS OK: ${DOMAIN} -> ${resolved}"
else
    echo "WARNING: ${DOMAIN} resolves to '${resolved:-<nothing>}', not ${HOST}." >&2
    echo "         Caddy's Let's Encrypt challenge needs ${DOMAIN} -> ${HOST}. Continuing anyway." >&2
fi

# 1. Install Caddy from its official apt repo.
echo "=== installing Caddy on ${HOST} ==="
ssh_run "set -e
export DEBIAN_FRONTEND=noninteractive
if ! command -v caddy >/dev/null 2>&1; then
  apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl gnupg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update && apt-get install -y caddy
fi
caddy version"

# 2. Open 80/443 if a firewall is active (don't enable one if it isn't).
echo "=== ensuring ports 80/443 reachable ==="
ssh_run "if command -v ufw >/dev/null 2>&1 && ufw status | grep -q 'Status: active'; then
  ufw allow 80/tcp; ufw allow 443/tcp; echo 'ufw: allowed 80,443';
else echo 'no active ufw; skipping'; fi"

# 3. Write the Caddyfile and reload.
echo "=== writing Caddyfile (${DOMAIN} -> 127.0.0.1:${BACKEND_PORT}) ==="
GLOBAL=""
[ -n "$EMAIL" ] && GLOBAL="{
	email ${EMAIL}
}
"
CADDYFILE="${GLOBAL}${DOMAIN} {
	reverse_proxy 127.0.0.1:${BACKEND_PORT}
}
"
printf '%s' "$CADDYFILE" | ssh "${SSH_OPTS[@]}" "root@${HOST}" "cat > /etc/caddy/Caddyfile && (systemctl reload caddy 2>/dev/null || systemctl restart caddy) && systemctl enable caddy >/dev/null 2>&1 || true && echo 'caddy reloaded'"

# 4. Update the explorer .env so Phoenix uses the domain over https. Upsert each
#    key, preserving everything else (notably TESTNET_SECRET_KEY_BASE).
echo "=== updating explorer .env (scheme=https host=${DOMAIN}) ==="
ssh_run "set -e
cd $(printf '%q' "$REMOTE_ROOT")
upsert() { local k=\"\$1\" v=\"\$2\"; if grep -qE \"^\${k}=\" .env; then sed -i -E \"s|^\${k}=.*|\${k}=\${v}|\" .env; else printf '%s=%s\n' \"\$k\" \"\$v\" >> .env; fi; }
upsert EXPLORER_SCHEME https
upsert EXPLORER_HOSTNAME $(printf '%q' "$DOMAIN")
upsert EXPLORER_PORT 443
upsert TESTNET_EXPLORER_HOSTNAME $(printf '%q' "$DOMAIN")
echo '--- effective explorer url env ---'
grep -E '^(EXPLORER_SCHEME|EXPLORER_HOSTNAME|EXPLORER_PORT|TESTNET_EXPLORER_HOSTNAME)=' .env"

# 5. Restart the explorer container to pick up the new env.
echo "=== restarting explorer container ==="
ssh_run "cd $(printf '%q' "$REMOTE_ROOT") && docker compose up -d $(printf '%q' "$SERVICE")"

# 6. Verify HTTPS on the domain (allow time for the cert to be issued on first run).
echo "=== verifying https://${DOMAIN} (waiting for cert issuance) ==="
ssh_run "code=000; for _ in \$(seq 1 24); do code=\$(curl -sS -o /dev/null -w '%{http_code}' --max-time 8 https://${DOMAIN}/ || true); case \$code in 200|302) break;; esac; sleep 5; done; echo \"https status from node: \$code\""

echo "=== done. Explorer should be live at https://${DOMAIN} (http auto-redirects) ==="
