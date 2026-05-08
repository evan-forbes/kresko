#!/bin/bash
set -o pipefail

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

rpc_has_no_error() {
    local response="$1"
    printf '%s' "$response" | jq -e '.error == null' >/dev/null 2>&1
}

rpc_has_result_and_no_error() {
    local response="$1"
    printf '%s' "$response" | jq -e '.error == null and .result != null' >/dev/null 2>&1
}

install_binary_atomic() {
    local src="$1"
    local dest="$2"
    local tmp="${dest}.new"

    install -m 0755 "$src" "$tmp"
    mv -f "$tmp" "$dest"
}

echo "=== Installing dependencies ==="
apt_retry update -y -o Dpkg::Options::="--force-confdef" -o Dpkg::Options::="--force-confold"
apt_retry install -y build-essential curl jq chrony tmux btop nethogs

echo "=== Disabling firewall for ephemeral instances ==="
ufw --force disable || true

echo "=== Configuring time sync ==="
systemctl enable chrony
systemctl start chrony

KRESKO_SYSCTL_FILE="/etc/sysctl.d/60-kresko-network-defaults.conf"
echo "=== Configuring default TCP and qdisc sysctls ==="
cat >"$KRESKO_SYSCTL_FILE" <<'EOF'
# Kresko node transport defaults.
net.ipv4.tcp_slow_start_after_idle=0
net.ipv4.tcp_congestion_control=cubic
net.core.default_qdisc=fq_codel
EOF
sysctl --load "$KRESKO_SYSCTL_FILE"

echo "=== Extracting payload ==="
rm -rf /root/payload
tar -xzf /root/$ARCHIVE_NAME -C /root/
source /root/payload/vars.sh
KRESKO_RPC_PORT="${KRESKO_RPC_PORT:-18232}"
KRESKO_P2P_PORT="${KRESKO_P2P_PORT:-18233}"
KRESKO_NETWORK_KIND="${KRESKO_NETWORK_KIND:-local-genesis}"
KRESKO_MINING_MODE="${KRESKO_MINING_MODE:-generate}"
KRESKO_FRESH_STATE="${KRESKO_FRESH_STATE:-1}"
export KRESKO_RPC_URL="http://127.0.0.1:${KRESKO_RPC_PORT}"
export KRESKO_RPC_PORT

cd $HOME
hostname=$(hostname)
parsed_hostname=$(echo $hostname | awk -F'-' '{print $1 "-" $2}')

echo "=== Installing binaries ==="
install_binary_atomic payload/build/zebrad /usr/local/bin/zebrad
install_binary_atomic payload/build/kresko /usr/local/bin/kresko

echo "=== Setting up zebra config ==="
if [ "$KRESKO_FRESH_STATE" = "1" ]; then
    echo "=== Resetting zebra state cache ==="
    rm -rf /root/.cache/zebra
else
    echo "=== Preserving zebra state cache (KRESKO_FRESH_STATE=$KRESKO_FRESH_STATE) ==="
fi
mkdir -p /root/.cache/zebra
mkdir -p /root/.config
cp payload/$parsed_hostname/zebrad.toml /root/.config/zebrad.toml
if [ -f "payload/$parsed_hostname/funded_key.json" ]; then
    cp "payload/$parsed_hostname/funded_key.json" /root/.config/funded_key.json
fi
# The deployed zebrad binary can lag behind payload config generation.
# Remove this optional key to keep old and new zebrad versions compatible.
sed -i -E '/^[[:space:]]*genesis_block_path[[:space:]]*=.*$/d' /root/.config/zebrad.toml
if [[ -n "${KRESKO_TARGET_BLOCK_TIME_SECS:-}" ]]; then
    actual_target_spacing="$(awk -F= '/^[[:space:]]*post_blossom_pow_target_spacing[[:space:]]*=/{gsub(/[[:space:]]/, "", $2); print $2; exit}' /root/.config/zebrad.toml)"
    if [[ -z "$actual_target_spacing" ]]; then
        echo "ERROR: zebrad.toml is missing post_blossom_pow_target_spacing; expected ${KRESKO_TARGET_BLOCK_TIME_SECS}s"
        exit 1
    fi
    if [[ "$actual_target_spacing" != "$KRESKO_TARGET_BLOCK_TIME_SECS" ]]; then
        echo "ERROR: zebrad.toml target spacing is ${actual_target_spacing}s; expected ${KRESKO_TARGET_BLOCK_TIME_SECS}s"
        exit 1
    fi
    echo "Verified zebrad target spacing: ${actual_target_spacing}s"
fi

BOOTSTRAP_CONFIG="/root/.config/zebrad.bootstrap.toml"
prepare_bootstrap_config() {
    awk '
        function starts_multiline_array(line) {
            return line ~ /=.*\[/ && line !~ /\]/
        }
        skip_array_tail {
            if ($0 ~ /^[[:space:]]*\]/) {
                skip_array_tail = 0
            }
            next
        }
        $0 ~ /^\[network\]$/ {
            in_network = 1
            print
            next
        }
        $0 ~ /^\[/ && $0 !~ /^\[network\]$/ {
            in_network = 0
        }
        in_network && $0 ~ /^[[:space:]]*listen_addr[[:space:]]*=/ {
            print "listen_addr = \"127.0.0.1:0\""
            next
        }
        in_network && $0 ~ /^[[:space:]]*initial_testnet_peers[[:space:]]*=/ {
            print "initial_testnet_peers = []"
            if (starts_multiline_array($0)) {
                skip_array_tail = 1
            }
            next
        }
        in_network && $0 ~ /^[[:space:]]*initial_mainnet_peers[[:space:]]*=/ {
            print "initial_mainnet_peers = []"
            if (starts_multiline_array($0)) {
                skip_array_tail = 1
            }
            next
        }
        { print }
    ' /root/.config/zebrad.toml > "$BOOTSTRAP_CONFIG"
    mkdir -p /root/.cache/zebra/network
    rm -f /root/.cache/zebra/network/*.peers
}

prepare_bootstrap_config

if [ "$KRESKO_MINING_MODE" != "observe" ]; then
current_miner_address=$(awk -F= '/^[[:space:]]*miner_address[[:space:]]*=/{gsub(/["[:space:]]/, "", $2); print tolower($2); exit}' /root/.config/zebrad.toml)
if [ -z "$current_miner_address" ] || [ "$current_miner_address" = "auto" ] || [ "$current_miner_address" = "__auto__" ] || [ "$current_miner_address" = "__auto_miner_address__" ]; then
    bootstrap_miner_address="t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v"
    echo "=== Auto-generating miner address via zebrad RPC ==="
    sed -i -E "s|^[[:space:]]*miner_address[[:space:]]*=.*$|miner_address = \"$bootstrap_miner_address\"|" /root/.config/zebrad.toml
    prepare_bootstrap_config

    BOOTSTRAP_LOG="/root/logs.bootstrap"
    zebrad -c "$BOOTSTRAP_CONFIG" start >"$BOOTSTRAP_LOG" 2>&1 &
    bootstrap_pid=$!

    generated_miner_address=""
    last_rpc_response=""
    for attempt in $(seq 1 90); do
        last_rpc_response=$(curl -sS --max-time 2 \
            -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","id":"kresko","method":"getnewaddress","params":[]}' \
            "$KRESKO_RPC_URL" 2>&1 || true)
        generated_miner_address=$(printf '%s' "$last_rpc_response" | jq -r '.result // empty' 2>/dev/null || true)

        if [ -n "$generated_miner_address" ]; then
            break
        fi

        if ! kill -0 "$bootstrap_pid" 2>/dev/null; then
            break
        fi

        sleep 2
    done

    if [ -n "$generated_miner_address" ]; then
        sed -i -E "s|^[[:space:]]*miner_address[[:space:]]*=.*$|miner_address = \"$generated_miner_address\"|" /root/.config/zebrad.toml
        echo "=== Auto miner address generated: $generated_miner_address ==="
    else
        echo "=== Failed to auto-generate miner address; aborting startup ==="
        if ! kill -0 "$bootstrap_pid" 2>/dev/null; then
            wait "$bootstrap_pid" 2>/dev/null
            bootstrap_exit=$?
            echo "=== Bootstrap zebrad exited early with code $bootstrap_exit ==="
        fi
        if [ -n "$last_rpc_response" ]; then
            echo "=== Last RPC response ==="
            echo "$last_rpc_response"
        else
            echo "=== No RPC response captured from $KRESKO_RPC_URL ==="
        fi
        if [ -f "$BOOTSTRAP_LOG" ]; then
            echo "=== Tail of bootstrap log ($BOOTSTRAP_LOG) ==="
            tail -n 120 "$BOOTSTRAP_LOG" || true
        fi
        if kill -0 "$bootstrap_pid" 2>/dev/null; then
            kill -INT "$bootstrap_pid" 2>/dev/null || true
            sleep 2
            kill -TERM "$bootstrap_pid" 2>/dev/null || true
        fi
        wait "$bootstrap_pid" 2>/dev/null || true
        exit 1
    fi

    if kill -0 "$bootstrap_pid" 2>/dev/null; then
        kill -INT "$bootstrap_pid" 2>/dev/null || true
        sleep 2
        kill -TERM "$bootstrap_pid" 2>/dev/null || true
    fi
    wait "$bootstrap_pid" 2>/dev/null || true
fi
fi

GENESIS_BLOCK_FILE="/root/payload/local_genesis/genesis.hex"
PREMINE_BLOCKS_FILE="/root/payload/local_genesis/premine_blocks.hex"
if [ -f "$GENESIS_BLOCK_FILE" ] || [ -f "$PREMINE_BLOCKS_FILE" ]; then
    echo "=== Seeding local chain state from payload artifacts ==="
    echo "=== Seeding in isolated bootstrap mode (P2P disabled until final startup) ==="
    prepare_bootstrap_config
    BOOTSTRAP_LOG="/root/logs.bootstrap"
    zebrad -c "$BOOTSTRAP_CONFIG" start >"$BOOTSTRAP_LOG" 2>&1 &
    bootstrap_pid=$!

    rpc_ready=0
    for attempt in $(seq 1 90); do
        rpc_response=$(curl -sS --max-time 2 \
            -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockchaininfo","params":[]}' \
            "$KRESKO_RPC_URL" 2>&1 || true)
        if rpc_has_result_and_no_error "$rpc_response"; then
            rpc_ready=1
            break
        fi
        if ! kill -0 "$bootstrap_pid" 2>/dev/null; then
            break
        fi
        sleep 2
    done

    if [ "$rpc_ready" -ne 1 ]; then
        echo "=== Failed to reach RPC while seeding local chain state ===" >&2
        if [ -f "$BOOTSTRAP_LOG" ]; then
            tail -n 120 "$BOOTSTRAP_LOG" || true
        fi
        if kill -0 "$bootstrap_pid" 2>/dev/null; then
            kill -INT "$bootstrap_pid" 2>/dev/null || true
            sleep 2
            kill -TERM "$bootstrap_pid" 2>/dev/null || true
        fi
        wait "$bootstrap_pid" 2>/dev/null || true
        exit 1
    fi

    expected_genesis_hash=$(awk -F= '/^[[:space:]]*genesis_hash[[:space:]]*=/{gsub(/["[:space:]]/, "", $2); print tolower($2); exit}' /root/.config/zebrad.toml)

    if [ -f "$GENESIS_BLOCK_FILE" ]; then
        genesis_hex=$(tr -d '[:space:]' < "$GENESIS_BLOCK_FILE")
        if [ -z "$genesis_hex" ]; then
            echo "=== Genesis file is empty: $GENESIS_BLOCK_FILE ===" >&2
            if kill -0 "$bootstrap_pid" 2>/dev/null; then
                kill -INT "$bootstrap_pid" 2>/dev/null || true
                sleep 2
                kill -TERM "$bootstrap_pid" 2>/dev/null || true
            fi
            wait "$bootstrap_pid" 2>/dev/null || true
            exit 1
        fi

        genesis_submit_response=$(curl -sS --max-time 10 \
            -H "Content-Type: application/json" \
            --data "{\"jsonrpc\":\"2.0\",\"id\":\"kresko\",\"method\":\"submitblock\",\"params\":[\"$genesis_hex\"]}" \
            "$KRESKO_RPC_URL" 2>&1 || true)
        if ! rpc_has_no_error "$genesis_submit_response"; then
            echo "=== submitblock RPC error while loading genesis block ===" >&2
            echo "$genesis_submit_response" >&2
            if kill -0 "$bootstrap_pid" 2>/dev/null; then
                kill -INT "$bootstrap_pid" 2>/dev/null || true
                sleep 2
                kill -TERM "$bootstrap_pid" 2>/dev/null || true
            fi
            wait "$bootstrap_pid" 2>/dev/null || true
            exit 1
        fi
        genesis_submit_result=$(printf '%s' "$genesis_submit_response" | jq -r '.result // empty' 2>/dev/null || true)
        if [ -n "$genesis_submit_result" ]; then
            case "$genesis_submit_result" in
                duplicate*|inconclusive)
                    ;;
                *)
                    echo "=== submitblock rejected genesis block: $genesis_submit_result ===" >&2
                    if kill -0 "$bootstrap_pid" 2>/dev/null; then
                        kill -INT "$bootstrap_pid" 2>/dev/null || true
                        sleep 2
                        kill -TERM "$bootstrap_pid" 2>/dev/null || true
                    fi
                    wait "$bootstrap_pid" 2>/dev/null || true
                    exit 1
                    ;;
            esac
        fi
    fi

    total_blocks=0
    if [ -f "$PREMINE_BLOCKS_FILE" ]; then
        total_blocks=$(grep -cve '^[[:space:]]*$' "$PREMINE_BLOCKS_FILE")
        echo "=== Seed blocks queued: $total_blocks ==="
    fi
    submitted=0
    if [ -f "$PREMINE_BLOCKS_FILE" ]; then
        while IFS= read -r block_hex || [ -n "$block_hex" ]; do
            if [ -z "$block_hex" ]; then
                continue
            fi

            block_accepted=0
            for retry in $(seq 1 10); do
                submit_response=$(curl -sS --max-time 10 \
                    -H "Content-Type: application/json" \
                    --data "{\"jsonrpc\":\"2.0\",\"id\":\"kresko\",\"method\":\"submitblock\",\"params\":[\"$block_hex\"]}" \
                    "$KRESKO_RPC_URL" 2>&1 || true)
                if ! rpc_has_no_error "$submit_response"; then
                    echo "=== submitblock RPC error while loading seed blocks ===" >&2
                    echo "$submit_response" >&2
                    if kill -0 "$bootstrap_pid" 2>/dev/null; then
                        kill -INT "$bootstrap_pid" 2>/dev/null || true
                        sleep 2
                        kill -TERM "$bootstrap_pid" 2>/dev/null || true
                    fi
                    wait "$bootstrap_pid" 2>/dev/null || true
                    exit 1
                fi
                submit_result=$(printf '%s' "$submit_response" | jq -r '.result // empty' 2>/dev/null || true)

                if [ -z "$submit_result" ] || [[ "$submit_result" == duplicate* ]] || [ "$submit_result" = "inconclusive" ]; then
                    block_accepted=1
                    break
                fi

                if [ "$submit_result" = "rejected" ] && [ "$retry" -lt 10 ]; then
                    echo "=== submitblock returned 'rejected' for seed block $((submitted+1)), retry $retry/10 ===" >&2
                    sleep 2
                    continue
                fi

                echo "=== submitblock rejected seed block: $submit_result ===" >&2
                if kill -0 "$bootstrap_pid" 2>/dev/null; then
                    kill -INT "$bootstrap_pid" 2>/dev/null || true
                    sleep 2
                    kill -TERM "$bootstrap_pid" 2>/dev/null || true
                fi
                wait "$bootstrap_pid" 2>/dev/null || true
                exit 1
            done

            submitted=$((submitted + 1))
            if [ "$total_blocks" -gt 0 ]; then
                if [ "$submitted" -eq 1 ] || [ $((submitted % 10)) -eq 0 ] || [ "$submitted" -eq "$total_blocks" ]; then
                    echo "=== Seed load progress: $submitted/$total_blocks blocks ==="
                fi
            elif [ "$submitted" -eq 1 ] || [ $((submitted % 10)) -eq 0 ]; then
                echo "=== Seed load progress: $submitted blocks ==="
            fi
        done < "$PREMINE_BLOCKS_FILE"
        echo "=== Loaded $submitted seed blocks ==="
    fi

    expected_tip_height="$total_blocks"
    chain_seeded=0
    for attempt in $(seq 1 120); do
        current_genesis_response=$(curl -sS --max-time 2 \
            -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockhash","params":[0]}' \
            "$KRESKO_RPC_URL" 2>&1 || true)
        if rpc_has_result_and_no_error "$current_genesis_response"; then
            current_genesis_hash=$(printf '%s' "$current_genesis_response" | jq -r '.result // empty' 2>/dev/null | tr '[:upper:]' '[:lower:]')
        else
            current_genesis_hash=""
        fi

        current_height_response=$(curl -sS --max-time 2 \
            -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockchaininfo","params":[]}' \
            "$KRESKO_RPC_URL" 2>&1 || true)
        if rpc_has_result_and_no_error "$current_height_response"; then
            current_height=$(printf '%s' "$current_height_response" | jq -r '.result.blocks // -1' 2>/dev/null || echo -1)
        else
            current_height=-1
        fi

        if [ -n "$current_genesis_hash" ] && [ -n "$expected_genesis_hash" ] && \
           [ "$current_genesis_hash" = "$expected_genesis_hash" ] && \
           [ "$current_height" -ge "$expected_tip_height" ] 2>/dev/null; then
            chain_seeded=1
            break
        fi

        if ! kill -0 "$bootstrap_pid" 2>/dev/null; then
            break
        fi
        sleep 1
    done

    if [ "$chain_seeded" -ne 1 ]; then
        echo "=== Timed out waiting for seeded chain state to commit ===" >&2
        echo "=== Expected genesis hash: $expected_genesis_hash, expected minimum height: $expected_tip_height ===" >&2
        if [ -f "$BOOTSTRAP_LOG" ]; then
            tail -n 120 "$BOOTSTRAP_LOG" || true
        fi
        if kill -0 "$bootstrap_pid" 2>/dev/null; then
            kill -INT "$bootstrap_pid" 2>/dev/null || true
            sleep 2
            kill -TERM "$bootstrap_pid" 2>/dev/null || true
        fi
        wait "$bootstrap_pid" 2>/dev/null || true
        exit 1
    fi

    if kill -0 "$bootstrap_pid" 2>/dev/null; then
        kill -INT "$bootstrap_pid" 2>/dev/null || true
        sleep 2
        kill -TERM "$bootstrap_pid" 2>/dev/null || true
    fi
    wait "$bootstrap_pid" 2>/dev/null || true
fi

rm -f "$BOOTSTRAP_CONFIG"

echo "=== Node: $parsed_hostname ==="

# In PoW mode, schedule the miner to start after zebrad's RPC is ready.
# The miner runs in a separate tmux session so it can be managed independently.
if [ "${KRESKO_MINING_MODE:-generate}" = "pow" ] && command -v kresko >/dev/null 2>&1; then
    echo "=== Scheduling PoW miner (will start after RPC is ready) ==="
    : > /root/kresko-mine.log
    cat > /root/kresko-mine-wait.sh <<'MINER_SCRIPT'
#!/bin/bash
# Wait for zebrad RPC to become available before starting the miner.
for attempt in $(seq 1 120); do
    rpc_response=$(curl -sS --max-time 2 \
        -H "Content-Type: application/json" \
        --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockchaininfo","params":[]}' \
        "$KRESKO_RPC_URL" 2>&1 || true)
    if printf '%s' "$rpc_response" | jq -e '.error == null and .result != null' >/dev/null 2>&1; then
        echo "=== RPC ready, starting kresko mine ==="
        exec kresko mine --rpc-endpoint "http://localhost:${KRESKO_RPC_PORT}"
    fi
    sleep 2
done
echo "=== RPC not ready after 240s, miner not started ==="
MINER_SCRIPT
    chmod +x /root/kresko-mine-wait.sh
    tmux set-environment -g KRESKO_RPC_URL "$KRESKO_RPC_URL"
    tmux set-environment -g KRESKO_RPC_PORT "$KRESKO_RPC_PORT"
    tmux new-session -d -s mine "bash -lc 'bash /root/kresko-mine-wait.sh 2>&1 | tee -a /root/kresko-mine.log; exec bash -i'"
fi

echo "=== Starting zebrad ==="

mkdir -p /root/logs
LOG_FILE="/root/logs/zebrad.log"
zebrad -c /root/.config/zebrad.toml start 2>&1 | tee -a "$LOG_FILE"
zebrad_exit=${PIPESTATUS[0]}

echo "=== zebrad exited with code $zebrad_exit ==="
exec bash
