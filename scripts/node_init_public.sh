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
apt_retry install -y curl jq chrony tmux btop nethogs

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

echo "=== Extracting payload ==="
rm -rf /root/payload
tar -xzf "/root/${ARCHIVE_NAME}" -C /root/
source /root/payload/vars.sh

KRESKO_NETWORK_KIND="${KRESKO_NETWORK_KIND:-}"
KRESKO_RPC_PORT="${KRESKO_RPC_PORT:-8232}"
KRESKO_P2P_PORT="${KRESKO_P2P_PORT:-8233}"
KRESKO_FRESH_STATE="${KRESKO_FRESH_STATE:-0}"

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
node_payload_dir="/root/payload/${parsed_hostname}"

if [ ! -d "$node_payload_dir" ]; then
    echo "ERROR: no payload directory for host '${parsed_hostname}' at ${node_payload_dir}" >&2
    exit 1
fi

echo "=== Installing binaries ==="
install_binary_atomic /root/payload/build/zebrad /usr/local/bin/zebrad
install_binary_atomic /root/payload/build/kresko /usr/local/bin/kresko

echo "=== Setting up zebra config ==="
if [ "$KRESKO_FRESH_STATE" = "1" ]; then
    echo "=== Resetting zebra state cache ==="
    rm -rf /root/.cache/zebra
else
    echo "=== Preserving zebra state cache (KRESKO_FRESH_STATE=$KRESKO_FRESH_STATE) ==="
fi
mkdir -p /root/.cache/zebra
mkdir -p /root/.config
cp "${node_payload_dir}/zebrad.toml" /root/.config/zebrad.toml

# The deployed zebrad binary can lag behind payload config generation.
# Remove this optional key to keep old and new zebrad versions compatible.
sed -i -E '/^[[:space:]]*genesis_block_path[[:space:]]*=.*$/d' /root/.config/zebrad.toml

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

echo "=== Public node: ${parsed_hostname} (${KRESKO_NETWORK_KIND}) ==="
echo "=== RPC port: ${KRESKO_RPC_PORT}; P2P port: ${KRESKO_P2P_PORT} ==="
echo "=== Ensure provider firewall allows inbound P2P and blocks public RPC ==="

echo "=== Starting zebrad ==="
for _kresko_idx in "${!_SAVED_ZEBRA_TRACE_NAMES[@]}"; do
    _kresko_var_name="${_SAVED_ZEBRA_TRACE_NAMES[$_kresko_idx]}"
    _kresko_var_value="${_SAVED_ZEBRA_TRACE_VALUES[$_kresko_idx]}"
    export "$_kresko_var_name=$_kresko_var_value"

    case "$_kresko_var_name" in
        ZEBRA_*TRACE*_DIR|ZEBRA_*TRACING*_DIR)
            if [ -n "$_kresko_var_value" ]; then
                mkdir -p "$_kresko_var_value"
            fi
            ;;
        ZEBRA_*TRACE*_FILE|ZEBRA_*TRACING*_FILE)
            if [ -n "$_kresko_var_value" ]; then
                mkdir -p "$(dirname "$_kresko_var_value")/traces"
            fi
            ;;
    esac
done

LOG_FILE="/root/logs"
set +e
zebrad -c /root/.config/zebrad.toml start 2>&1 | tee -a "$LOG_FILE"
zebrad_exit=${PIPESTATUS[0]}
set -e

echo "=== zebrad exited with code $zebrad_exit ==="
exec bash
