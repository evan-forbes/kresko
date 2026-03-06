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

echo "=== Installing dependencies ==="
apt_retry update -y -o Dpkg::Options::="--force-confdef" -o Dpkg::Options::="--force-confold"
apt_retry install -y build-essential curl jq chrony tmux btop nethogs

echo "=== Disabling firewall for ephemeral instances ==="
ufw --force disable || true

echo "=== Configuring time sync ==="
systemctl enable chrony
systemctl start chrony

echo "=== Configuring BBR congestion control ==="
modprobe tcp_bbr || true
sysctl -w net.core.default_qdisc=fq
sysctl -w net.ipv4.tcp_congestion_control=bbr
echo "net.core.default_qdisc=fq" >> /etc/sysctl.conf
echo "net.ipv4.tcp_congestion_control=bbr" >> /etc/sysctl.conf

echo "=== Extracting payload ==="
tar -xzf /root/$ARCHIVE_NAME -C /root/
source /root/payload/vars.sh

cd $HOME
hostname=$(hostname)
parsed_hostname=$(echo $hostname | awk -F'-' '{print $1 "-" $2}')

echo "=== Installing binaries ==="
cp payload/build/zebrad /usr/local/bin/zebrad
chmod +x /usr/local/bin/zebrad

if [ -f payload/build/kresko ]; then
    cp payload/build/kresko /usr/local/bin/kresko
    chmod +x /usr/local/bin/kresko
fi

echo "=== Setting up zebra config ==="
mkdir -p /root/.cache/zebra
mkdir -p /root/.config
cp payload/$parsed_hostname/zebrad.toml /root/.config/zebrad.toml
if [ -f "payload/$parsed_hostname/funded_key.json" ]; then
    cp "payload/$parsed_hostname/funded_key.json" /root/.config/funded_key.json
fi
# The deployed zebrad binary can lag behind payload config generation.
# Remove this optional key to keep old and new zebrad versions compatible.
sed -i -E '/^[[:space:]]*genesis_block_path[[:space:]]*=.*$/d' /root/.config/zebrad.toml

current_miner_address=$(awk -F= '/^[[:space:]]*miner_address[[:space:]]*=/{gsub(/["[:space:]]/, "", $2); print tolower($2); exit}' /root/.config/zebrad.toml)
if [ -z "$current_miner_address" ] || [ "$current_miner_address" = "auto" ] || [ "$current_miner_address" = "__auto__" ] || [ "$current_miner_address" = "__auto_miner_address__" ]; then
    bootstrap_miner_address="t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v"
    echo "=== Auto-generating miner address via zebrad RPC ==="
    sed -i -E "s|^[[:space:]]*miner_address[[:space:]]*=.*$|miner_address = \"$bootstrap_miner_address\"|" /root/.config/zebrad.toml

    BOOTSTRAP_LOG="/root/logs.bootstrap"
    zebrad -c /root/.config/zebrad.toml start >"$BOOTSTRAP_LOG" 2>&1 &
    bootstrap_pid=$!

    generated_miner_address=""
    last_rpc_response=""
    for attempt in $(seq 1 90); do
        last_rpc_response=$(curl -sS --max-time 2 \
            -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","id":"kresko","method":"getnewaddress","params":[]}' \
            http://127.0.0.1:18232 2>&1 || true)
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
            echo "=== No RPC response captured from 127.0.0.1:18232 ==="
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

GENESIS_BLOCK_FILE="/root/payload/local_genesis/genesis.hex"
PREMINE_BLOCKS_FILE="/root/payload/local_genesis/premine_blocks.hex"
if [ -f "$GENESIS_BLOCK_FILE" ] || [ -f "$PREMINE_BLOCKS_FILE" ]; then
    echo "=== Seeding local chain state from payload artifacts ==="
    BOOTSTRAP_LOG="/root/logs.bootstrap"
    zebrad -c /root/.config/zebrad.toml start >"$BOOTSTRAP_LOG" 2>&1 &
    bootstrap_pid=$!

    rpc_ready=0
    for attempt in $(seq 1 90); do
        rpc_response=$(curl -sS --max-time 2 \
            -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockchaininfo","params":[]}' \
            http://127.0.0.1:18232 2>&1 || true)
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
            http://127.0.0.1:18232 2>&1 || true)
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
        echo "=== Premine blocks queued: $total_blocks ==="
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
                    http://127.0.0.1:18232 2>&1 || true)
                if ! rpc_has_no_error "$submit_response"; then
                    echo "=== submitblock RPC error while loading premine blocks ===" >&2
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
                    echo "=== submitblock returned 'rejected' for premine block $((submitted+1)), retry $retry/10 ===" >&2
                    sleep 2
                    continue
                fi

                echo "=== submitblock rejected premine block: $submit_result ===" >&2
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
                    echo "=== Premine load progress: $submitted/$total_blocks blocks ==="
                fi
            elif [ "$submitted" -eq 1 ] || [ $((submitted % 10)) -eq 0 ]; then
                echo "=== Premine load progress: $submitted blocks ==="
            fi
        done < "$PREMINE_BLOCKS_FILE"
        echo "=== Loaded $submitted premine blocks ==="
    fi

    expected_tip_height="$total_blocks"
    chain_seeded=0
    for attempt in $(seq 1 120); do
        current_genesis_response=$(curl -sS --max-time 2 \
            -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockhash","params":[0]}' \
            http://127.0.0.1:18232 2>&1 || true)
        if rpc_has_result_and_no_error "$current_genesis_response"; then
            current_genesis_hash=$(printf '%s' "$current_genesis_response" | jq -r '.result // empty' 2>/dev/null | tr '[:upper:]' '[:lower:]')
        else
            current_genesis_hash=""
        fi

        current_height_response=$(curl -sS --max-time 2 \
            -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockchaininfo","params":[]}' \
            http://127.0.0.1:18232 2>&1 || true)
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

echo "=== Node: $parsed_hostname ==="
echo "=== Starting zebrad ==="

LOG_FILE="/root/logs"
zebrad -c /root/.config/zebrad.toml start 2>&1 | tee -a "$LOG_FILE"
zebrad_exit=${PIPESTATUS[0]}

echo "=== zebrad exited with code $zebrad_exit ==="
exec bash
