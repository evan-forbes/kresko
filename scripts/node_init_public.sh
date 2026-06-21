#!/bin/bash
set -euo pipefail

ARCHIVE_NAME="payload.tar.gz"
export DEBIAN_FRONTEND=noninteractive

apt_retry() {
    local max_attempts=10
    local attempt=1
    while true; do
        if apt-get -o DPkg::Lock::Timeout=60 "$@"; then
            return 0
        fi
        if [ "$attempt" -ge "$max_attempts" ]; then
            echo "apt-get failed after ${max_attempts} attempts: apt-get $*" >&2
            return 1
        fi
        echo "apt-get retry ${attempt}/${max_attempts} in 10s: apt-get $*" >&2
        attempt=$((attempt + 1))
        sleep 10
    done
}

install_binary_atomic() {
    local src="$1"
    local dest="$2"
    local tmp="${dest}.new"

    install -m 0755 "$src" "$tmp"
    mv -f "$tmp" "$dest"
}

require_line() {
    local pattern="$1"
    local file="$2"
    local description="$3"

    if ! grep -Eq "$pattern" "$file"; then
        echo "ERROR: missing or invalid ${description} in ${file}" >&2
        exit 1
    fi
}

echo "=== Installing public-node dependencies ==="
apt_retry update -y -o Dpkg::Options::="--force-confdef" -o Dpkg::Options::="--force-confold"
apt_retry install -y aria2 curl jq chrony tmux btop nethogs zstd

echo "=== Configuring time sync ==="
systemctl enable chrony
systemctl start chrony

KRESKO_SYSCTL_FILE="/etc/sysctl.d/60-kresko-network-defaults.conf"
echo "=== Configuring default TCP and qdisc sysctls ==="
cat >"$KRESKO_SYSCTL_FILE" <<'EOF'
# Kresko public-node transport defaults.
net.ipv4.tcp_slow_start_after_idle=0
net.ipv4.tcp_congestion_control=cubic
net.core.default_qdisc=fq_codel
EOF
sysctl --load "$KRESKO_SYSCTL_FILE"

echo "=== Locating payload ==="
if [ -f /root/kresko/payload/vars.sh ]; then
    payload_root="/root/kresko/payload"
elif [ -f "/root/${ARCHIVE_NAME}" ]; then
    rm -rf /root/payload
    tar -xzf "/root/${ARCHIVE_NAME}" -C /root/
    payload_root="/root/payload"
else
    echo "ERROR: no synced payload directory or /root/${ARCHIVE_NAME} archive found" >&2
    exit 1
fi
source "${payload_root}/vars.sh"

KRESKO_NETWORK_KIND="${KRESKO_NETWORK_KIND:-}"
KRESKO_RPC_PORT="${KRESKO_RPC_PORT:-8232}"
KRESKO_P2P_PORT="${KRESKO_P2P_PORT:-8233}"
KRESKO_FRESH_STATE="${KRESKO_FRESH_STATE:-0}"
# Optional state-snapshot bootstrap (default off). When set, the node hydrates
# zebrad's state DB from this URL instead of syncing the whole chain over P2P.
# Public-network only (this script already gates on mainnet/public-testnet).
# The operator is responsible for pointing at a snapshot for the *right*
# network — a mainnet snapshot on a testnet node will not verify.
KRESKO_STATE_SNAPSHOT_URL="${KRESKO_STATE_SNAPSHOT_URL:-}"
KRESKO_STATE_SNAPSHOT_URLS="${KRESKO_STATE_SNAPSHOT_URLS:-}"
KRESKO_STATE_SNAPSHOT_SHA256="${KRESKO_STATE_SNAPSHOT_SHA256:-}"
KRESKO_STATE_SNAPSHOT_ARCHIVE="${KRESKO_STATE_SNAPSHOT_ARCHIVE:-kresko-state-snapshot.tar.gz}"

case "$KRESKO_NETWORK_KIND" in
    mainnet|public-testnet)
        ;;
    *)
        echo "ERROR: public node init only supports mainnet/public-testnet; got '${KRESKO_NETWORK_KIND}'" >&2
        exit 1
        ;;
esac

if [ -n "${KRESKO_TARGET_BLOCK_TIME_SECS:-}" ]; then
    echo "ERROR: public networks must not carry KRESKO_TARGET_BLOCK_TIME_SECS; use public consensus parameters" >&2
    exit 1
fi

# Zebra's config parser reads all ZEBRA_* env vars and rejects unknown ones.
# Save tracing env vars and unset them so config parsing succeeds.
declare -a _SAVED_ZEBRA_TRACE_NAMES=()
declare -a _SAVED_ZEBRA_TRACE_VALUES=()
for _kresko_var_name in ${!ZEBRA@}; do
    case "$_kresko_var_name" in
        ZEBRA_*TRACE*|ZEBRA_*TRACING*)
            _SAVED_ZEBRA_TRACE_NAMES+=("$_kresko_var_name")
            _SAVED_ZEBRA_TRACE_VALUES+=("${!_kresko_var_name}")
            unset "$_kresko_var_name"
            ;;
    esac
done

cd "$HOME"
hostname=$(hostname)
parsed_hostname=$(echo "$hostname" | awk -F'-' '{print $1 "-" $2}')
node_payload_dir="${payload_root}/${parsed_hostname}"

if [ ! -d "$node_payload_dir" ]; then
    echo "ERROR: no payload directory for host '${parsed_hostname}' at ${node_payload_dir}" >&2
    exit 1
fi

echo "=== Installing binaries ==="
install_binary_atomic "${payload_root}/build/zebrad" /usr/local/bin/zebrad
install_binary_atomic "${payload_root}/build/kresko" /usr/local/bin/kresko

echo "=== Setting up zebra config ==="
if [ "$KRESKO_FRESH_STATE" = "1" ]; then
    echo "=== Resetting zebra state cache ==="
    rm -rf /root/.cache/zebra
else
    echo "=== Preserving zebra state cache (KRESKO_FRESH_STATE=$KRESKO_FRESH_STATE) ==="
fi
mkdir -p /root/.cache/zebra

if [ -n "$KRESKO_STATE_SNAPSHOT_URLS" ] || [ -n "$KRESKO_STATE_SNAPSHOT_URL" ]; then
    if [ -d /root/.cache/zebra/state ] && [ -n "$(ls -A /root/.cache/zebra/state 2>/dev/null)" ]; then
        echo "=== State cache already present; skipping snapshot ==="
    else
        snapshot_archive="/tmp/${KRESKO_STATE_SNAPSHOT_ARCHIVE}"
        snapshot_urls="${KRESKO_STATE_SNAPSHOT_URLS:-$KRESKO_STATE_SNAPSHOT_URL}"
        echo "=== Hydrating zebra state from snapshot: ${snapshot_urls} ==="
        aria2_args=(-x16 -s16 --continue=true --allow-overwrite=true -d /tmp -o "$KRESKO_STATE_SNAPSHOT_ARCHIVE")
        if [ -n "$KRESKO_STATE_SNAPSHOT_SHA256" ]; then
            aria2_args+=("--checksum=sha-256=${KRESKO_STATE_SNAPSHOT_SHA256}")
        fi
        if aria2c "${aria2_args[@]}" ${snapshot_urls}; then
            case "$snapshot_archive" in
                *.tar.zst)
                    zstd -dc "$snapshot_archive" | tar -x -C /root/.cache/zebra
                    ;;
                *.tar.gz|*.tgz)
                    tar -xzf "$snapshot_archive" -C /root/.cache/zebra
                    ;;
                *.tar)
                    tar -xf "$snapshot_archive" -C /root/.cache/zebra
                    ;;
                *)
                    echo "ERROR: unsupported snapshot archive extension: ${snapshot_archive}" >&2
                    exit 1
                    ;;
            esac
            rm -f "$snapshot_archive"
            echo "=== Snapshot extracted; zebrad will resume from the snapshot height ==="
        else
            status=$?
            rm -f "$snapshot_archive"
            echo "=== Snapshot hydration failed with exit ${status}; falling back to P2P block sync ==="
        fi
    fi
fi

mkdir -p /root/.config
cp "${node_payload_dir}/zebrad.toml" /root/.config/zebrad.toml

# The deployed zebrad binary can lag behind payload config generation.
# Remove this optional key TOML-aware so an unrelated key with a similar
# name can never be deleted by accident.
kresko config strip-genesis-block-path /root/.config/zebrad.toml

if [ "$KRESKO_NETWORK_KIND" = "mainnet" ]; then
    require_line '^[[:space:]]*network[[:space:]]*=[[:space:]]*"Mainnet"' /root/.config/zebrad.toml "mainnet network"
    require_line '^[[:space:]]*listen_addr[[:space:]]*=[[:space:]]*"0\.0\.0\.0:8233"' /root/.config/zebrad.toml "mainnet P2P listen address"
    require_line '^[[:space:]]*initial_mainnet_peers[[:space:]]*=' /root/.config/zebrad.toml "mainnet seed peers"
else
    require_line '^[[:space:]]*network[[:space:]]*=[[:space:]]*"Testnet"' /root/.config/zebrad.toml "testnet network"
    require_line '^[[:space:]]*listen_addr[[:space:]]*=[[:space:]]*"0\.0\.0\.0:18233"' /root/.config/zebrad.toml "testnet P2P listen address"
    require_line '^[[:space:]]*initial_testnet_peers[[:space:]]*=' /root/.config/zebrad.toml "testnet seed peers"
fi
require_line '^[[:space:]]*external_addr[[:space:]]*=' /root/.config/zebrad.toml "external address"
require_line '^[[:space:]]*v2_p2p[[:space:]]*=[[:space:]]*true' /root/.config/zebrad.toml "Zakura P2P enablement"
require_line '^[[:space:]]*legacy_p2p[[:space:]]*=[[:space:]]*(true|false)' /root/.config/zebrad.toml "legacy Zebra P2P setting"
require_line '^[[:space:]]*zakura_node_secret_key[[:space:]]*=' /root/.config/zebrad.toml "stable Zakura node identity"
require_line '^[[:space:]]*listen_addr[[:space:]]*=[[:space:]]*"0\.0\.0\.0:8234"' /root/.config/zebrad.toml "Zakura P2P listen address"
require_line '^[[:space:]]*bootstrap_peers[[:space:]]*=' /root/.config/zebrad.toml "Zakura bootstrap peers"

echo "=== Public node: ${parsed_hostname} (${KRESKO_NETWORK_KIND}) ==="
echo "=== RPC port: ${KRESKO_RPC_PORT}; P2P port: ${KRESKO_P2P_PORT} ==="
echo "=== Ensure provider firewall allows inbound P2P and blocks public RPC ==="

echo "=== Starting zebrad (systemd) ==="
mkdir -p /root/logs /root/traces
LOG_FILE="/root/logs/zebrad.log"

# Re-apply the tracing env vars we held back during config parsing as
# systemd Environment= lines (so `download-traces` keeps working), and
# make sure any trace dirs/files exist. zebrad otherwise runs with a clean
# env under systemd.
_kresko_unit_env=""
for _kresko_idx in "${!_SAVED_ZEBRA_TRACE_NAMES[@]}"; do
    _kresko_var_name="${_SAVED_ZEBRA_TRACE_NAMES[$_kresko_idx]}"
    _kresko_var_value="${_SAVED_ZEBRA_TRACE_VALUES[$_kresko_idx]}"
    case "$_kresko_var_name" in
        ZEBRA_*TRACE*_DIR|ZEBRA_*TRACING*_DIR)
            if [ -n "$_kresko_var_value" ]; then
                mkdir -p "$_kresko_var_value"
            fi
            ;;
        ZEBRA_*TRACE*_FILE|ZEBRA_*TRACING*_FILE)
            if [ -n "$_kresko_var_value" ]; then
                mkdir -p "$(dirname "$_kresko_var_value")"
            fi
            ;;
    esac
    _kresko_unit_env+="Environment=\"${_kresko_var_name}=${_kresko_var_value}\""$'\n'
done

# Run zebrad as a supervised systemd service rather than a bare process:
#  - Restart=on-failure survives crashes; the unit is enabled so it also
#    survives reboots.
#  - LimitNOFILE raises the open-file ceiling. zebra only lifts its own
#    soft limit to a built-in target and splits it 50/50 between rocksdb
#    SST files and peer sockets; at the OS default of 1024 a synced mainnet
#    node exhausts descriptors and panics the state service with EMFILE.
# The old `export ZEBRA_NODE_ID` is intentionally gone: this zebrad rejects
# that field, and the clean systemd env avoids it entirely.
cat > /etc/systemd/system/zebrad.service <<UNIT
[Unit]
Description=Zebra (zakura) ${KRESKO_NETWORK_KIND} node - ${parsed_hostname}
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=/root
ExecStart=/usr/local/bin/zebrad -c /root/.config/zebrad.toml start
Restart=on-failure
RestartSec=10
LimitNOFILE=1048576
${_kresko_unit_env}StandardOutput=append:${LOG_FILE}
StandardError=append:${LOG_FILE}

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable zebrad >/dev/null 2>&1 || true

# Fast-fail: if zebrad can't stay up (bad config, missing binary, port
# clash), surface the log tail and fail the deploy instead of leaving a
# dead/looping unit behind.
set +e
systemctl restart zebrad
for _ in $(seq 1 8); do
    sleep 2
    if [ "$(systemctl is-active zebrad)" = "failed" ] \
        || [ "$(systemctl show -p NRestarts --value zebrad)" != "0" ]; then
        echo "=== zebrad failed to stay up under systemd ===" >&2
        systemctl status zebrad --no-pager -l >&2 2>/dev/null || true
        echo "=== Tail of ${LOG_FILE} ===" >&2
        tail -n 200 "${LOG_FILE}" >&2 || true
        exit 1
    fi
done
set -e

echo "=== zebrad is running under systemd (is-active: $(systemctl is-active zebrad)) ==="
echo "=== follow logs: tail -f ${LOG_FILE}  (or: journalctl -u zebrad -f) ==="
