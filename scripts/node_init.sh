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

# All logs land under /root/logs so reset, collect, and the early-exit
# tail logic can reason about a single tree.
mkdir -p /root/logs

# Persist kresko env so any future shell — tmux session, attached debugger,
# pyinfra-issued one-shot — sees KRESKO_RPC_URL/PORT without re-deriving
# them. tmux_start_command sources this file before running its payload.
mkdir -p /root/.kresko
cat > /root/.kresko/env <<KRESKO_ENV_EOF
KRESKO_RPC_URL=$KRESKO_RPC_URL
KRESKO_RPC_PORT=$KRESKO_RPC_PORT
KRESKO_NETWORK_KIND=$KRESKO_NETWORK_KIND
KRESKO_MINING_MODE=$KRESKO_MINING_MODE
KRESKO_ENV_EOF
chmod 0644 /root/.kresko/env

# Defense in depth: tmux's global env reaches sessions started against an
# already-running tmux server (e.g. when an operator attaches manually).
if command -v tmux >/dev/null 2>&1; then
    tmux set-environment -g KRESKO_RPC_URL "$KRESKO_RPC_URL" 2>/dev/null || true
    tmux set-environment -g KRESKO_RPC_PORT "$KRESKO_RPC_PORT" 2>/dev/null || true
    tmux set-environment -g KRESKO_NETWORK_KIND "$KRESKO_NETWORK_KIND" 2>/dev/null || true
fi

echo "=== Installing binaries ==="
# Provenance: hash before/after install and compare against the payload
# manifest written by `kresko genesis`. The before-hash tells us whether
# the deploy actually changed the binary (important for redeploy auditing);
# the manifest match tells us the running binary is byte-identical to the
# one the run was built against.
sha256_or_missing() {
    local file="$1"
    if [ -f "$file" ]; then
        sha256sum "$file" | awk '{print $1}'
    else
        echo "missing"
    fi
}

manifest_value() {
    local key="$1"
    local manifest="payload/build/manifest.txt"
    if [ ! -f "$manifest" ]; then
        echo ""
        return
    fi
    awk -F= -v k="$key" '$1 == k { print $2; exit }' "$manifest"
}

verify_provenance() {
    local name="$1"
    local prev_hash="$2"
    local installed_hash="$3"
    local payload_hash="$4"

    if [ -z "$payload_hash" ]; then
        echo "WARN: payload/build/manifest.txt has no ${name}_sha256; skipping provenance check" >&2
    elif [ "$installed_hash" != "$payload_hash" ]; then
        echo "ERROR: installed ${name} sha256 ${installed_hash} does not match payload manifest ${payload_hash}" >&2
        return 1
    fi
    if [ "$prev_hash" = "missing" ]; then
        echo "PROVENANCE: ${name} installed (sha256=${installed_hash}, no previous binary)"
    elif [ "$prev_hash" = "$installed_hash" ]; then
        echo "PROVENANCE: ${name} unchanged (sha256=${installed_hash})"
    else
        echo "PROVENANCE: ${name} CHANGED (was=${prev_hash}, now=${installed_hash})"
    fi
}

zebrad_prev_hash="$(sha256_or_missing /usr/local/bin/zebrad)"
kresko_prev_hash="$(sha256_or_missing /usr/local/bin/kresko)"

install_binary_atomic payload/build/zebrad /usr/local/bin/zebrad
install_binary_atomic payload/build/kresko /usr/local/bin/kresko

zebrad_new_hash="$(sha256_or_missing /usr/local/bin/zebrad)"
kresko_new_hash="$(sha256_or_missing /usr/local/bin/kresko)"

verify_provenance zebrad "$zebrad_prev_hash" "$zebrad_new_hash" "$(manifest_value zebrad_sha256)" || exit 1
verify_provenance kresko "$kresko_prev_hash" "$kresko_new_hash" "$(manifest_value kresko_sha256)" || exit 1

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
# The bootstrap (P2P-disabled) variant is pre-rendered at payload-build
# time by `kresko genesis`, so node_init.sh no longer munges TOML with awk.
BOOTSTRAP_CONFIG="/root/.config/zebrad.bootstrap.toml"
if [ -f "payload/$parsed_hostname/zebrad.bootstrap.toml" ]; then
    cp "payload/$parsed_hostname/zebrad.bootstrap.toml" "$BOOTSTRAP_CONFIG"
else
    # Backward compat: re-render from the deployed config if the payload was
    # built by an older kresko that didn't bake one in.
    kresko config render-bootstrap /root/.config/zebrad.toml --out "$BOOTSTRAP_CONFIG"
fi
mkdir -p /root/.cache/zebra/network
rm -f /root/.cache/zebra/network/*.peers

# Older zebrad releases reject genesis_block_path. Strip TOML-aware so we
# never break a deployed config.
kresko config strip-genesis-block-path /root/.config/zebrad.toml

if [ "$KRESKO_MINING_MODE" != "observe" ]; then
current_miner_address=$(kresko config get-miner-address /root/.config/zebrad.toml | tr '[:upper:]' '[:lower:]')
if [ -z "$current_miner_address" ] || [ "$current_miner_address" = "auto" ] || [ "$current_miner_address" = "__auto__" ] || [ "$current_miner_address" = "__auto_miner_address__" ]; then
    bootstrap_miner_address="t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v"
    echo "=== Auto-generating miner address via zebrad RPC ==="
    # Update both files atomically; bootstrap and the live config must
    # share the same miner_address so RPC bootstrap signs the same way.
    kresko config set-miner-address \
        --address "$bootstrap_miner_address" \
        --path /root/.config/zebrad.toml \
        --path "$BOOTSTRAP_CONFIG"

    BOOTSTRAP_LOG="/root/logs/bootstrap.log"
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
        kresko config set-miner-address \
            --address "$generated_miner_address" \
            --path /root/.config/zebrad.toml \
            --path "$BOOTSTRAP_CONFIG"
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
    # Drop stale peer cache so the bootstrap node can't dial fleet peers
    # via cached entries from a prior run.
    mkdir -p /root/.cache/zebra/network
    rm -f /root/.cache/zebra/network/*.peers
    BOOTSTRAP_LOG="/root/logs/bootstrap.log"
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

    expected_genesis_hash="$(kresko config get-genesis-hash /root/.config/zebrad.toml)"

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
            for retry in $(seq 1 30); do
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

                if [ "$submit_result" = "rejected" ] && [ "$retry" -lt 30 ]; then
                    echo "=== submitblock returned 'rejected' for seed block $((submitted+1)), retry $retry/30 ===" >&2
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
            committed=0
            for attempt in $(seq 1 60); do
                current_height_response=$(curl -sS --max-time 2 \
                    -H "Content-Type: application/json" \
                    --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockchaininfo","params":[]}' \
                    "$KRESKO_RPC_URL" 2>&1 || true)
                if rpc_has_result_and_no_error "$current_height_response"; then
                    current_height=$(printf '%s' "$current_height_response" | jq -r '.result.blocks // -1' 2>/dev/null || echo -1)
                    if [ "$current_height" -ge "$submitted" ] 2>/dev/null; then
                        committed=1
                        break
                    fi
                fi
                if ! kill -0 "$bootstrap_pid" 2>/dev/null; then
                    break
                fi
                sleep 1
            done
            if [ "$committed" -ne 1 ]; then
                echo "=== Timed out waiting for seed block $submitted to commit ===" >&2
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
    : > /root/logs/miner.log
    cat > /root/.kresko/miner-wait.sh <<'MINER_SCRIPT'
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
    chmod +x /root/.kresko/miner-wait.sh
    tmux new-session -d -s mine "bash -lc 'set -a; . /root/.kresko/env; set +a; bash /root/.kresko/miner-wait.sh 2>&1 | tee -a /root/logs/miner.log; exec bash -i'"
fi

echo "=== Starting zebrad ==="

LOG_FILE="/root/logs/zebrad.log"
zebrad -c /root/.config/zebrad.toml start 2>&1 | tee -a "$LOG_FILE" &
zebrad_pid=$!

# Surface a fast-fail: if zebrad dies in the first ~10 seconds the deploy
# should make that obvious instead of letting the wrapper drop into bash
# silently with no zebrad running.
for _ in $(seq 1 5); do
    sleep 2
    if ! kill -0 "$zebrad_pid" 2>/dev/null; then
        wait "$zebrad_pid" 2>/dev/null
        zebrad_exit=$?
        echo "=== zebrad exited within 10s with code $zebrad_exit ===" >&2
        if [ -f "$LOG_FILE" ]; then
            echo "=== Tail of $LOG_FILE ===" >&2
            tail -n 200 "$LOG_FILE" >&2 || true
        fi
        exit "$zebrad_exit"
    fi
done

wait "$zebrad_pid"
zebrad_exit=$?

echo "=== zebrad exited with code $zebrad_exit ==="
exec bash
