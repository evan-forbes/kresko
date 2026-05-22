#!/usr/bin/env bash
set -Eeuo pipefail

# Join the NU7 testnet on an x86_64 Ubuntu host.
#
# This single script:
#   1. downloads (or copies) a kresko-generated join bundle (genesis seed blocks,
#      zebrad config, and a manifest) from --bundle-url,
#   2. downloads and checksum-verifies the prebuilt zebrad and (with --mine)
#      kresko binaries from their GitHub releases — no compilation,
#   3. replaces any previous NU7 join-script install state,
#   4. seeds the local-genesis chain, then starts zebrad (and optionally a miner).
#
# The release coordinates (repo + tag) are read from the bundle manifest, so the
# binaries always match the chain the bundle describes. Override them with the
# --zebra-* / --kresko-* flags if needed.

MINE=0
FOREGROUND=0
DRY_RUN=0
KEEP_BUNDLE=0
MINER_ADDRESS=""
BUNDLE_URL="${NU7_JOIN_BUNDLE_URL:-}"
ZEBRA_REPO_OVERRIDE=""
ZEBRA_TAG_OVERRIDE=""
KRESKO_REPO_OVERRIDE=""
KRESKO_TAG_OVERRIDE=""
ORIGINAL_ARGS=("$@")

INSTALL_ROOT="/opt/nu7-testnet"
BUNDLE_DIR="$INSTALL_ROOT/bundle"
CHECKPOINTS_EXPECTED="$BUNDLE_DIR/local_genesis/checkpoints.txt"
ZEBRAD_BIN="/usr/local/bin/zebrad"
KRESKO_BIN="/usr/local/bin/kresko"
CONFIG_PATH="/root/.config/zebrad.toml"
BOOTSTRAP_CONFIG="/root/.config/zebrad.join-bootstrap.toml"
LOG_DIR="${NU7_LOG_DIR:-/var/log/nu7-testnet}"
RPC_PORT="${KRESKO_RPC_PORT:-18232}"
RPC_URL="http://127.0.0.1:${RPC_PORT}"

usage() {
    cat <<'USAGE'
Usage: join-nu7-testnet.sh --bundle-url URL_OR_PATH [options]

Downloads the NU7 join bundle, fetches the prebuilt zebrad/kresko binaries from
their releases, replaces any previous NU7 join-script install state, seeds the
local genesis chain, and starts zebrad.

Options:
  --bundle-url URL_OR_PATH  Join bundle tarball (https URL or local path). Required.
                            May also be set via NU7_JOIN_BUNDLE_URL.
  --mine                    Also download kresko and start `kresko mine`.
  --miner-address ADDRESS   Mine to this transparent testnet address (implies --mine).
  --foreground              Run zebrad in the foreground instead of a tmux session.
  --dry-run                 Validate the bundle and exit (no install, no root needed).
  --keep-bundle             Keep the downloaded work directory instead of deleting it.
  --zebra-repo OWNER/NAME   Override the zebra release repo from the manifest.
  --zebra-tag TAG           Override the zebra release tag from the manifest.
  --kresko-repo OWNER/NAME  Override the kresko release repo from the manifest.
  --kresko-tag TAG          Override the kresko release tag from the manifest.
  -h, --help                Show this help.
USAGE
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --bundle-url)
            BUNDLE_URL="${2:-}"
            [ -n "$BUNDLE_URL" ] || { echo "missing value for --bundle-url" >&2; exit 2; }
            shift 2
            ;;
        --mine)
            MINE=1
            shift
            ;;
        --miner-address)
            MINER_ADDRESS="${2:-}"
            [ -n "$MINER_ADDRESS" ] || { echo "missing value for --miner-address" >&2; exit 2; }
            MINE=1
            shift 2
            ;;
        --foreground)
            FOREGROUND=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --keep-bundle)
            KEEP_BUNDLE=1
            shift
            ;;
        --zebra-repo)
            ZEBRA_REPO_OVERRIDE="${2:-}"
            [ -n "$ZEBRA_REPO_OVERRIDE" ] || { echo "missing value for --zebra-repo" >&2; exit 2; }
            shift 2
            ;;
        --zebra-tag)
            ZEBRA_TAG_OVERRIDE="${2:-}"
            [ -n "$ZEBRA_TAG_OVERRIDE" ] || { echo "missing value for --zebra-tag" >&2; exit 2; }
            shift 2
            ;;
        --kresko-repo)
            KRESKO_REPO_OVERRIDE="${2:-}"
            [ -n "$KRESKO_REPO_OVERRIDE" ] || { echo "missing value for --kresko-repo" >&2; exit 2; }
            shift 2
            ;;
        --kresko-tag)
            KRESKO_TAG_OVERRIDE="${2:-}"
            [ -n "$KRESKO_TAG_OVERRIDE" ] || { echo "missing value for --kresko-tag" >&2; exit 2; }
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ -z "$BUNDLE_URL" ]; then
    echo "missing join bundle URL or path; pass --bundle-url or set NU7_JOIN_BUNDLE_URL" >&2
    exit 2
fi

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "required command not found: $1" >&2
        exit 1
    fi
}

for cmd in curl tar jq sha256sum mktemp awk sed find; do
    require_cmd "$cmd"
done

WORK_DIR="$(mktemp -d)"
if [ "$KEEP_BUNDLE" -eq 0 ]; then
    trap 'rm -rf "$WORK_DIR"' EXIT
else
    echo "keeping downloaded artifacts in $WORK_DIR"
fi

download_to() {
    local src="$1" dest="$2"
    if [ -f "$src" ]; then
        cp "$src" "$dest"
    else
        curl -fL --retry 3 "$src" -o "$dest"
    fi
}

verify_sha256() {
    local file="$1" sha_file="$2" expected actual
    expected="$(awk 'NR==1{print $1}' "$sha_file")"
    actual="$(sha256sum "$file" | awk '{print $1}')"
    if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
        echo "checksum mismatch for $file" >&2
        echo "expected: $expected" >&2
        echo "actual:   $actual" >&2
        exit 1
    fi
}

# Download + extract the join bundle into $WORK_DIR and echo the directory that
# directly contains join-manifest.json.
fetch_bundle() {
    local archive extract_dir manifest
    archive="$WORK_DIR/join-bundle.tar.gz"
    extract_dir="$WORK_DIR/bundle-extract"
    mkdir -p "$extract_dir"
    download_to "$BUNDLE_URL" "$archive"
    tar -xzf "$archive" -C "$extract_dir"
    manifest="$(find "$extract_dir" -maxdepth 3 -type f -name join-manifest.json | head -n 1)"
    if [ -z "$manifest" ]; then
        echo "join bundle does not contain join-manifest.json" >&2
        exit 1
    fi
    dirname "$manifest"
}

# Resolve the release coordinates from the bundle manifest, honoring overrides.
read_release_coordinates() {
    local manifest="$1/join-manifest.json"
    ZEBRA_REPO="${ZEBRA_REPO_OVERRIDE:-$(jq -r '.zebra_release_repo' "$manifest")}"
    ZEBRA_TAG="${ZEBRA_TAG_OVERRIDE:-$(jq -r '.zebra_release_tag' "$manifest")}"
    KRESKO_REPO="${KRESKO_REPO_OVERRIDE:-$(jq -r '.kresko_release_repo' "$manifest")}"
    KRESKO_TAG="${KRESKO_TAG_OVERRIDE:-$(jq -r '.kresko_release_tag' "$manifest")}"
    for var in ZEBRA_REPO ZEBRA_TAG KRESKO_REPO KRESKO_TAG; do
        if [ -z "${!var}" ] || [ "${!var}" = "null" ]; then
            echo "join manifest is missing release coordinate for $var" >&2
            exit 1
        fi
    done
}

validate_bundle_hashes() {
    local manifest="$BUNDLE_DIR/join-manifest.json"
    if [ ! -f "$manifest" ]; then
        echo "missing manifest: $manifest" >&2
        exit 1
    fi

    jq -r '.files | to_entries[] | [.key, .value] | @tsv' "$manifest" |
    while IFS=$'\t' read -r relative_path expected_hash; do
        local file="$BUNDLE_DIR/$relative_path"
        if [ ! -f "$file" ]; then
            echo "manifest file missing: $relative_path" >&2
            exit 1
        fi
        local actual_hash
        actual_hash="$(sha256sum "$file" | awk '{print $1}')"
        if [ "$actual_hash" != "$expected_hash" ]; then
            echo "hash mismatch for $relative_path" >&2
            echo "expected: $expected_hash" >&2
            echo "actual:   $actual_hash" >&2
            exit 1
        fi
    done
}

validate_join_inputs() {
    validate_bundle_hashes

    local manifest="$BUNDLE_DIR/join-manifest.json"
    local expected_genesis config_genesis peer_count checkpoint_path
    expected_genesis="$(jq -r '.genesis_hash' "$manifest" | tr '[:upper:]' '[:lower:]')"
    config_genesis="$(awk -F= '/^[[:space:]]*genesis_hash[[:space:]]*=/{gsub(/["[:space:]]/, "", $2); print tolower($2); exit}' "$BUNDLE_DIR/zebrad.join.toml")"
    checkpoint_path="$(awk -F= '/^[[:space:]]*checkpoints[[:space:]]*=/{gsub(/["[:space:]]/, "", $2); print $2; exit}' "$BUNDLE_DIR/zebrad.join.toml")"
    peer_count="$(jq '.bootstrap_peers | length' "$manifest")"

    if [ -z "$expected_genesis" ] || [ "$expected_genesis" = "null" ]; then
        echo "manifest is missing genesis_hash" >&2
        exit 1
    fi
    if [ "$config_genesis" != "$expected_genesis" ]; then
        echo "zebrad.join.toml genesis_hash does not match manifest" >&2
        exit 1
    fi
    if [ "$peer_count" -lt 1 ]; then
        echo "manifest has no bootstrap peers" >&2
        exit 1
    fi
    if [ "$checkpoint_path" != "$CHECKPOINTS_EXPECTED" ]; then
        echo "zebrad.join.toml checkpoints path does not point at $CHECKPOINTS_EXPECTED" >&2
        exit 1
    fi
    if grep -q 'initial_testnet_peers = \[\]' "$BUNDLE_DIR/zebrad.join.toml"; then
        echo "zebrad.join.toml has empty initial_testnet_peers" >&2
        exit 1
    fi
}

install_zebrad() {
    local base url archive extract_dir bin
    base="https://github.com/${ZEBRA_REPO}/releases/download/${ZEBRA_TAG}"
    url="${base}/zebra-${ZEBRA_TAG}-x86_64-unknown-linux-gnu.tar.gz"
    archive="$WORK_DIR/zebra.tar.gz"
    extract_dir="$WORK_DIR/zebra-extract"
    echo "downloading zebrad from $url"
    download_to "$url" "$archive"
    download_to "${url}.sha256" "${archive}.sha256"
    verify_sha256 "$archive" "${archive}.sha256"
    mkdir -p "$extract_dir"
    tar -xzf "$archive" -C "$extract_dir"
    # The release tarball contains a single binary (named "zebra"); install it as zebrad.
    bin="$(find "$extract_dir" -maxdepth 2 -type f | head -n 1)"
    if [ -z "$bin" ]; then
        echo "zebra release archive was empty" >&2
        exit 1
    fi
    install -m 0755 "$bin" "$ZEBRAD_BIN"
}

install_kresko() {
    local base url bin
    base="https://github.com/${KRESKO_REPO}/releases/download/${KRESKO_TAG}"
    url="${base}/kresko-${KRESKO_TAG}-x86_64-linux-gnu"
    bin="$WORK_DIR/kresko"
    echo "downloading kresko from $url"
    download_to "$url" "$bin"
    download_to "${url}.sha256" "${bin}.sha256"
    verify_sha256 "$bin" "${bin}.sha256"
    install -m 0755 "$bin" "$KRESKO_BIN"
}

rm_join_dir() {
    local path="$1"
    case "$path" in
        ""|"/")
            echo "refusing to remove unsafe path: ${path:-<empty>}" >&2
            exit 1
            ;;
    esac
    rm -rf "$path"
}

reset_existing_join_install() {
    echo "resetting existing NU7 join install, if any"

    if command -v tmux >/dev/null 2>&1; then
        tmux kill-session -t nu7-zebrad 2>/dev/null || true
        tmux kill-session -t nu7-mine 2>/dev/null || true
    fi

    pkill -INT -x zebrad 2>/dev/null || true
    pkill -INT -x zebra 2>/dev/null || true
    pkill -TERM -f '[k]resko mine' 2>/dev/null || true
    sleep 2
    pkill -TERM -x zebrad 2>/dev/null || true
    pkill -TERM -x zebra 2>/dev/null || true

    rm_join_dir "$INSTALL_ROOT"
    rm_join_dir "$LOG_DIR"
    rm -f "$CONFIG_PATH" "$BOOTSTRAP_CONFIG"
}

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

rpc_has_result_and_no_error() {
    local response="$1"
    printf '%s' "$response" | jq -e '.error == null and .result != null' >/dev/null 2>&1
}

rpc_has_no_error() {
    local response="$1"
    printf '%s' "$response" | jq -e '.error == null' >/dev/null 2>&1
}

replace_miner_address() {
    local address="$1"
    sed -i -E "s|^[[:space:]]*miner_address[[:space:]]*=.*$|miner_address = \"$address\"|" "$CONFIG_PATH"
}

generate_miner_address() {
    python3 - <<'PY'
import hashlib
import secrets

alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
version = bytes.fromhex("1cba")  # Zcash testnet P2SH, yielding t2... addresses.
payload = version + secrets.token_bytes(20)
checksum = hashlib.sha256(hashlib.sha256(payload).digest()).digest()[:4]
raw = payload + checksum
value = int.from_bytes(raw, "big")
chars = []
while value:
    value, rem = divmod(value, 58)
    chars.append(alphabet[rem])
encoded = "".join(reversed(chars)) or "1"
leading_zeroes = len(raw) - len(raw.lstrip(b"\0"))
print("1" * leading_zeroes + encoded)
PY
}

prepare_bootstrap_config() {
    awk '
        skip_array {
            if ($0 ~ /^[[:space:]]*\]/) {
                skip_array = 0
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
            if ($0 !~ /\[[[:space:]]*\]/) {
                skip_array = 1
            }
            next
        }
        in_network && $0 ~ /^[[:space:]]*initial_mainnet_peers[[:space:]]*=/ {
            print "initial_mainnet_peers = []"
            if ($0 !~ /\[[[:space:]]*\]/) {
                skip_array = 1
            }
            next
        }
        { print }
    ' "$CONFIG_PATH" > "$BOOTSTRAP_CONFIG"
    mkdir -p "$INSTALL_ROOT/state/network"
    rm -f "$INSTALL_ROOT"/state/network/*.peers
}

wait_for_rpc() {
    local attempts="${1:-120}"
    local response
    for _attempt in $(seq 1 "$attempts"); do
        response="$(curl -sS --max-time 2 -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockchaininfo","params":[]}' \
            "$RPC_URL" 2>&1 || true)"
        if rpc_has_result_and_no_error "$response"; then
            return 0
        fi
        sleep 2
    done
    return 1
}

submit_block_hex() {
    local block_hex="$1"
    local label="$2"
    local response result
    response="$(curl -sS --max-time 10 -H "Content-Type: application/json" \
        --data "{\"jsonrpc\":\"2.0\",\"id\":\"kresko\",\"method\":\"submitblock\",\"params\":[\"$block_hex\"]}" \
        "$RPC_URL" 2>&1 || true)"
    if ! rpc_has_no_error "$response"; then
        echo "submitblock RPC error while loading $label" >&2
        echo "$response" >&2
        return 1
    fi
    result="$(printf '%s' "$response" | jq -r '.result // empty' 2>/dev/null || true)"
    case "$result" in
        ""|duplicate*|inconclusive)
            return 0
            ;;
        *)
            echo "submitblock rejected $label: $result" >&2
            return 1
            ;;
    esac
}

seed_local_genesis() {
    local genesis_file="$BUNDLE_DIR/local_genesis/genesis.hex"
    local premine_file="$BUNDLE_DIR/local_genesis/premine_blocks.hex"
    local bootstrap_log="$LOG_DIR/bootstrap.log"
    local bootstrap_pid

    prepare_bootstrap_config
    mkdir -p "$LOG_DIR"
    "$ZEBRAD_BIN" -c "$BOOTSTRAP_CONFIG" start >"$bootstrap_log" 2>&1 &
    bootstrap_pid=$!

    if ! wait_for_rpc 120; then
        echo "failed to reach bootstrap RPC while seeding" >&2
        tail -n 120 "$bootstrap_log" || true
        kill "$bootstrap_pid" 2>/dev/null || true
        wait "$bootstrap_pid" 2>/dev/null || true
        exit 1
    fi

    submit_block_hex "$(tr -d '[:space:]' < "$genesis_file")" "genesis block"

    local total submitted block_hex
    total="$(grep -cve '^[[:space:]]*$' "$premine_file" || true)"
    submitted=0
    while IFS= read -r block_hex || [ -n "$block_hex" ]; do
        [ -z "$block_hex" ] && continue
        submit_block_hex "$block_hex" "seed block $((submitted + 1))"
        submitted=$((submitted + 1))
        if [ "$submitted" -eq 1 ] || [ $((submitted % 10)) -eq 0 ] || [ "$submitted" -eq "$total" ]; then
            echo "seed load progress: $submitted/$total"
        fi
    done < "$premine_file"

    local expected_genesis expected_height seeded current_genesis current_height response
    expected_genesis="$(jq -r '.genesis_hash' "$BUNDLE_DIR/join-manifest.json" | tr '[:upper:]' '[:lower:]')"
    expected_height="$total"
    seeded=0
    for _attempt in $(seq 1 120); do
        response="$(curl -sS --max-time 2 -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockhash","params":[0]}' \
            "$RPC_URL" 2>&1 || true)"
        current_genesis="$(printf '%s' "$response" | jq -r '.result // empty' 2>/dev/null | tr '[:upper:]' '[:lower:]')"
        response="$(curl -sS --max-time 2 -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockchaininfo","params":[]}' \
            "$RPC_URL" 2>&1 || true)"
        current_height="$(printf '%s' "$response" | jq -r '.result.blocks // -1' 2>/dev/null || echo -1)"
        if [ "$current_genesis" = "$expected_genesis" ] && [ "$current_height" -ge "$expected_height" ] 2>/dev/null; then
            seeded=1
            break
        fi
        sleep 1
    done

    kill -INT "$bootstrap_pid" 2>/dev/null || true
    sleep 2
    kill -TERM "$bootstrap_pid" 2>/dev/null || true
    wait "$bootstrap_pid" 2>/dev/null || true
    rm -f "$BOOTSTRAP_CONFIG"

    if [ "$seeded" -ne 1 ]; then
        echo "timed out waiting for seeded chain state to commit" >&2
        tail -n 120 "$bootstrap_log" || true
        exit 1
    fi
}

# --- main ------------------------------------------------------------------

EXTRACTED_BUNDLE="$(fetch_bundle)"
read_release_coordinates "$EXTRACTED_BUNDLE"

if [ "$DRY_RUN" -eq 1 ]; then
    # Validate the extracted bundle in place; the checkpoints path inside
    # zebrad.join.toml is still the absolute runtime path ($CHECKPOINTS_EXPECTED).
    BUNDLE_DIR="$EXTRACTED_BUNDLE"
    validate_join_inputs
    echo "dry run OK"
    echo "  zebra:  $ZEBRA_REPO @ $ZEBRA_TAG"
    echo "  kresko: $KRESKO_REPO @ $KRESKO_TAG"
    exit 0
fi

if [ "$(id -u)" -ne 0 ]; then
    exec sudo -E bash "$0" "${ORIGINAL_ARGS[@]}"
fi

export DEBIAN_FRONTEND=noninteractive
apt_retry update -y -o Dpkg::Options::="--force-confdef" -o Dpkg::Options::="--force-confold"
# Runtime-only dependencies. The released binaries are prebuilt, so no Rust
# toolchain, C/C++ compiler, or git checkout is required.
apt_retry install -y ca-certificates chrony curl jq libstdc++6 python3 tar tmux

systemctl enable chrony || true
systemctl start chrony || true

reset_existing_join_install

mkdir -p "$INSTALL_ROOT" "$BUNDLE_DIR" "$LOG_DIR" /root/.config
cp -a "$EXTRACTED_BUNDLE"/. "$BUNDLE_DIR"/

validate_join_inputs
cp "$BUNDLE_DIR/zebrad.join.toml" "$CONFIG_PATH"

if [ "$MINE" -eq 1 ] && [ -z "$MINER_ADDRESS" ]; then
    MINER_ADDRESS="$(generate_miner_address)"
    echo "generated miner address: $MINER_ADDRESS"
fi
if [ -n "$MINER_ADDRESS" ]; then
    replace_miner_address "$MINER_ADDRESS"
fi

install_zebrad
if [ "$MINE" -eq 1 ]; then
    install_kresko
fi

seed_local_genesis

if [ "$MINE" -eq 1 ]; then
    cat > "$INSTALL_ROOT/mine-wait.sh" <<'MINER_SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
RPC_URL="${KRESKO_RPC_URL:-http://127.0.0.1:18232}"
for _attempt in $(seq 1 120); do
    response="$(curl -sS --max-time 2 -H "Content-Type: application/json" \
        --data '{"jsonrpc":"2.0","id":"kresko","method":"getblockchaininfo","params":[]}' \
        "$RPC_URL" 2>&1 || true)"
    if printf '%s' "$response" | jq -e '.error == null and .result != null' >/dev/null 2>&1; then
        exec kresko mine --rpc-endpoint "$RPC_URL"
    fi
    sleep 2
done
echo "RPC was not ready after 240s; miner not started" >&2
exit 1
MINER_SCRIPT
    chmod +x "$INSTALL_ROOT/mine-wait.sh"
    tmux new-session -d -s nu7-mine "bash -lc '$INSTALL_ROOT/mine-wait.sh 2>&1 | tee -a $LOG_DIR/mine.log'"
fi

if [ "$FOREGROUND" -eq 1 ]; then
    exec "$ZEBRAD_BIN" -c "$CONFIG_PATH" start
fi

tmux new-session -d -s nu7-zebrad "bash -lc '$ZEBRAD_BIN -c $CONFIG_PATH start 2>&1 | tee -a $LOG_DIR/zebrad.log'"
echo "zebrad started in tmux session nu7-zebrad"
echo "logs: $LOG_DIR/zebrad.log"
