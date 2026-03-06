#!/usr/bin/env bash
set -u
set -o pipefail

RPC_URL="${RPC_URL:-http://127.0.0.1:18232}"
CONFIG_PATH="${CONFIG_PATH:-/root/.config/zebrad.toml}"
PEER_IP="${PEER_IP:-}"
PEER_P2P_PORT="${PEER_P2P_PORT:-18233}"
PEER_RPC_PORT="${PEER_RPC_PORT:-18232}"
RPC_TIMEOUT="${RPC_TIMEOUT:-5}"
DO_ADDNODE=0

usage() {
    cat <<'EOF'
Usage: network_diag.sh [options]

Options:
  --rpc URL               Local JSON-RPC URL (default: http://127.0.0.1:18232)
  --config PATH           zebrad config path (default: /root/.config/zebrad.toml)
  --peer-ip IP            Peer node IP for connectivity/addnode checks
  --peer-p2p-port PORT    Peer P2P port (default: 18233)
  --peer-rpc-port PORT    Peer RPC port (default: 18232)
  --rpc-timeout SEC       Curl timeout in seconds (default: 5)
  --addnode               Try addnode("<peer-ip>:<peer-p2p-port>", "add")
  -h, --help              Show this help

Examples:
  ./network_diag.sh
  ./network_diag.sh --peer-ip 165.227.115.123 --addnode
  ./network_diag.sh --rpc http://127.0.0.1:18232 --config /root/.config/zebrad.toml
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --rpc)
            RPC_URL="$2"
            shift 2
            ;;
        --config)
            CONFIG_PATH="$2"
            shift 2
            ;;
        --peer-ip)
            PEER_IP="$2"
            shift 2
            ;;
        --peer-p2p-port)
            PEER_P2P_PORT="$2"
            shift 2
            ;;
        --peer-rpc-port)
            PEER_RPC_PORT="$2"
            shift 2
            ;;
        --rpc-timeout)
            RPC_TIMEOUT="$2"
            shift 2
            ;;
        --addnode)
            DO_ADDNODE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage
            exit 2
            ;;
    esac
done

have_cmd() {
    command -v "$1" >/dev/null 2>&1
}

hr() {
    printf '\n%s\n' "======================================================================"
}

section() {
    hr
    printf '%s\n' "$1"
    hr
}

json_pretty() {
    if have_cmd jq; then
        jq .
    else
        cat
    fi
}

rpc_raw() {
    local url="$1"
    local method="$2"
    local params="${3:-[]}"
    curl -sS --max-time "$RPC_TIMEOUT" \
        -H "Content-Type: application/json" \
        --data "{\"jsonrpc\":\"2.0\",\"id\":\"diag\",\"method\":\"$method\",\"params\":$params}" \
        "$url"
}

rpc_print() {
    local label="$1"
    local url="$2"
    local method="$3"
    local params="${4:-[]}"

    printf '\n[%s] %s %s\n' "$label" "$method" "$params"
    local out
    if ! out="$(rpc_raw "$url" "$method" "$params" 2>&1)"; then
        printf 'RPC ERROR: %s\n' "$out"
        return 1
    fi
    printf '%s\n' "$out" | json_pretty
}

rpc_value() {
    local url="$1"
    local method="$2"
    local params="$3"
    local jq_filter="$4"

    if ! have_cmd jq; then
        return 1
    fi

    local out
    out="$(rpc_raw "$url" "$method" "$params" 2>/dev/null)" || return 1
    printf '%s' "$out" | jq -r "$jq_filter // empty" 2>/dev/null
}

print_basics() {
    section "Host Basics"
    local host_name
    if have_cmd hostname; then
        host_name="$(hostname)"
    else
        host_name="$(uname -n 2>/dev/null || echo unknown)"
    fi

    printf 'Timestamp: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    printf 'Hostname:  %s\n' "$host_name"
    printf 'RPC URL:   %s\n' "$RPC_URL"
    printf 'Config:    %s\n' "$CONFIG_PATH"
    if [[ -n "$PEER_IP" ]]; then
        printf 'Peer IP:   %s\n' "$PEER_IP"
        printf 'Peer P2P:  %s\n' "$PEER_P2P_PORT"
        printf 'Peer RPC:  %s\n' "$PEER_RPC_PORT"
    fi

    if have_cmd ss; then
        printf '\nListening ports (18232/18233):\n'
        ss -ltnp 2>/dev/null | grep -E ':(18232|18233)\b' || true
    fi
}

print_config_consistency() {
    section "Config Consistency Fields"
    if [[ ! -f "$CONFIG_PATH" ]]; then
        printf 'Config not found: %s\n' "$CONFIG_PATH"
        return
    fi

    grep -E '^[[:space:]]*(network[[:space:]]*=|listen_addr[[:space:]]*=|initial_testnet_peers[[:space:]]*=|external_addr[[:space:]]*=|network_name[[:space:]]*=|network_magic[[:space:]]*=|genesis_hash[[:space:]]*=|target_difficulty_limit[[:space:]]*=|disable_pow[[:space:]]*=|checkpoints[[:space:]]*=|Overwinter[[:space:]]*=|Sapling[[:space:]]*=|Blossom[[:space:]]*=|Heartwood[[:space:]]*=|Canopy[[:space:]]*=|NU5[[:space:]]*=|NU6[[:space:]]*=|"NU6\.1"[[:space:]]*=)' "$CONFIG_PATH" || true
}

print_local_rpc() {
    section "Local RPC Snapshot"
    rpc_print "LOCAL" "$RPC_URL" "getnetworkinfo" "[]"
    rpc_print "LOCAL" "$RPC_URL" "getpeerinfo" "[]"
    rpc_print "LOCAL" "$RPC_URL" "getblockchaininfo" "[]"
    rpc_print "LOCAL" "$RPC_URL" "getblockhash" "[0]"
    rpc_print "LOCAL" "$RPC_URL" "getbestblockhash" "[]"
}

print_peer_connectivity() {
    [[ -z "$PEER_IP" ]] && return

    section "Peer Connectivity"
    if have_cmd nc; then
        if nc -vz -w "$RPC_TIMEOUT" "$PEER_IP" "$PEER_P2P_PORT"; then
            printf 'P2P connectivity: OK (%s:%s)\n' "$PEER_IP" "$PEER_P2P_PORT"
        else
            printf 'P2P connectivity: FAIL (%s:%s)\n' "$PEER_IP" "$PEER_P2P_PORT"
        fi
    else
        printf 'Skipping nc connectivity test: nc not installed.\n'
    fi
}

try_addnode() {
    [[ -z "$PEER_IP" ]] && return
    [[ "$DO_ADDNODE" -ne 1 ]] && return

    section "addnode Probe"
    local params
    params="[\"${PEER_IP}:${PEER_P2P_PORT}\",\"add\"]"
    rpc_print "LOCAL" "$RPC_URL" "addnode" "$params"
    sleep 2
    rpc_print "LOCAL" "$RPC_URL" "getpeerinfo" "[]"
}

print_peer_rpc() {
    [[ -z "$PEER_IP" ]] && return
    local peer_rpc_url="http://${PEER_IP}:${PEER_RPC_PORT}"

    section "Peer RPC Snapshot (${peer_rpc_url})"
    rpc_print "PEER" "$peer_rpc_url" "getnetworkinfo" "[]"
    rpc_print "PEER" "$peer_rpc_url" "getpeerinfo" "[]"
    rpc_print "PEER" "$peer_rpc_url" "getblockchaininfo" "[]"
    rpc_print "PEER" "$peer_rpc_url" "getblockhash" "[0]"
    rpc_print "PEER" "$peer_rpc_url" "getbestblockhash" "[]"
}

print_summary() {
    section "Consistency Summary"
    if ! have_cmd jq; then
        printf 'jq not found, skipping summary extraction.\n'
        return
    fi

    local local_genesis local_best local_height
    local_genesis="$(rpc_value "$RPC_URL" "getblockhash" "[0]" '.result')"
    local_best="$(rpc_value "$RPC_URL" "getbestblockhash" "[]" '.result')"
    local_height="$(rpc_value "$RPC_URL" "getblockchaininfo" "[]" '.result.blocks')"

    printf 'LOCAL genesis: %s\n' "${local_genesis:-<unavailable>}"
    printf 'LOCAL best:    %s\n' "${local_best:-<unavailable>}"
    printf 'LOCAL height:  %s\n' "${local_height:-<unavailable>}"

    if [[ -n "$PEER_IP" ]]; then
        local peer_rpc_url="http://${PEER_IP}:${PEER_RPC_PORT}"
        local peer_genesis peer_best peer_height
        peer_genesis="$(rpc_value "$peer_rpc_url" "getblockhash" "[0]" '.result')"
        peer_best="$(rpc_value "$peer_rpc_url" "getbestblockhash" "[]" '.result')"
        peer_height="$(rpc_value "$peer_rpc_url" "getblockchaininfo" "[]" '.result.blocks')"

        printf 'PEER genesis:  %s\n' "${peer_genesis:-<unavailable>}"
        printf 'PEER best:     %s\n' "${peer_best:-<unavailable>}"
        printf 'PEER height:   %s\n' "${peer_height:-<unavailable>}"

        if [[ -n "${local_genesis:-}" && -n "${peer_genesis:-}" ]]; then
            if [[ "$local_genesis" == "$peer_genesis" ]]; then
                printf 'GENESIS MATCH: yes\n'
            else
                printf 'GENESIS MATCH: no\n'
            fi
        fi
    fi
}

print_basics
print_config_consistency
print_local_rpc
print_peer_connectivity
try_addnode
print_peer_rpc
print_summary

hr
printf 'Done.\n'
hr
