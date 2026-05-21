use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::{
    Config, DaaConfig, Instance, LocalGenesisActivationHeights, LocalGenesisConfig, NetworkKind,
    NodeType, Provider,
};
use crate::zebra_config::{self, LocalTestnetParameters};

const JOIN_INSTALL_ROOT: &str = "/opt/nu7-testnet";
const JOIN_BUNDLE_DIR: &str = "/opt/nu7-testnet/bundle";
const JOIN_CHECKPOINTS_PATH: &str = "/opt/nu7-testnet/bundle/local_genesis/checkpoints.txt";
const DEFAULT_OBSERVER_MINER_ADDRESS: &str = "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v";
const DEFAULT_RUSTFLAGS: &str =
    r#"--cfg zcash_unstable="nu7" --cfg zcash_unstable="zip235" --cfg zcash_unstable="nsm""#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinManifest {
    pub chain_id: String,
    pub genesis_hash: String,
    pub seeded_tip_hash: Option<String>,
    pub network_magic: [u8; 4],
    pub target_difficulty_limit: String,
    pub target_spacing_secs: Option<u32>,
    pub activation_heights: LocalGenesisActivationHeights,
    pub bootstrap_peers: Vec<String>,
    pub zebra_git_url: String,
    pub zebra_ref: String,
    pub kresko_git_url: String,
    pub kresko_ref: String,
    pub zebra_jsonl_trace_git_url: String,
    pub zebra_jsonl_trace_ref: String,
    pub generated_at_unix_secs: u64,
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct PayloadPremineManifest {
    #[serde(default)]
    pow_start_height: Option<u32>,
}

pub fn run(
    run_dir: &str,
    zebra_git_url: &str,
    zebra_ref: &str,
    kresko_git_url: &str,
    kresko_ref: &str,
    zebra_jsonl_trace_git_url: &str,
    zebra_jsonl_trace_ref: &str,
    out: &str,
) -> Result<()> {
    let run_dir = Path::new(run_dir);
    let out_dir = Path::new(out);
    let config = Config::load(run_dir)?;
    config.require_local_genesis("join-bundle")?;

    let local_genesis = config
        .local_genesis
        .as_ref()
        .context("config.json has no local_genesis; run `kresko genesis` first")?;
    let bootstrap_peers = bootstrap_peers(&config)?;
    if bootstrap_peers.is_empty() {
        anyhow::bail!("no active bootstrap peers found in config.json; run `kresko up` first");
    }

    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("failed to create {}", out_dir.display()))?;
    let out_local_genesis = out_dir.join("local_genesis");
    if out_local_genesis.exists() {
        std::fs::remove_dir_all(&out_local_genesis)
            .with_context(|| format!("failed to clear {}", out_local_genesis.display()))?;
    }
    std::fs::create_dir_all(&out_local_genesis)
        .with_context(|| format!("failed to create {}", out_local_genesis.display()))?;

    let payload_local_genesis = run_dir.join("payload/local_genesis");
    for file_name in ["genesis.hex", "premine_blocks.hex", "checkpoints.txt"] {
        let source = payload_local_genesis.join(file_name);
        if !source.is_file() {
            anyhow::bail!(
                "missing required payload artifact {}; run `kresko genesis` first",
                source.display()
            );
        }
        std::fs::copy(&source, out_local_genesis.join(file_name))
            .with_context(|| format!("failed to copy {}", source.display()))?;
    }

    let zebrad_config = render_join_zebrad_config(run_dir, &config, local_genesis)?;
    let zebrad_config_path = out_dir.join("zebrad.join.toml");
    std::fs::write(&zebrad_config_path, zebrad_config)
        .with_context(|| format!("failed to write {}", zebrad_config_path.display()))?;

    let script = render_join_script();
    let script_path = out_dir.join("join-nu7-testnet.sh");
    std::fs::write(&script_path, script)
        .with_context(|| format!("failed to write {}", script_path.display()))?;
    make_executable(&script_path)?;

    let mut files = BTreeMap::new();
    for relative_path in [
        "join-nu7-testnet.sh",
        "zebrad.join.toml",
        "local_genesis/genesis.hex",
        "local_genesis/premine_blocks.hex",
        "local_genesis/checkpoints.txt",
    ] {
        files.insert(
            relative_path.to_string(),
            sha256_file(&out_dir.join(relative_path))?,
        );
    }

    let generated_at_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_secs();
    let manifest = JoinManifest {
        chain_id: config.chain_id.clone(),
        genesis_hash: local_genesis.genesis_hash.clone(),
        seeded_tip_hash: local_genesis.seeded_tip_hash.clone(),
        network_magic: local_genesis.network_magic,
        target_difficulty_limit: local_genesis.target_difficulty_limit.clone(),
        target_spacing_secs: local_genesis.target_spacing_secs,
        activation_heights: local_genesis.activation_heights.clone(),
        bootstrap_peers,
        zebra_git_url: zebra_git_url.to_string(),
        zebra_ref: zebra_ref.to_string(),
        kresko_git_url: kresko_git_url.to_string(),
        kresko_ref: kresko_ref.to_string(),
        zebra_jsonl_trace_git_url: zebra_jsonl_trace_git_url.to_string(),
        zebra_jsonl_trace_ref: zebra_jsonl_trace_ref.to_string(),
        generated_at_unix_secs,
        files,
    };
    let manifest_path = out_dir.join("join-manifest.json");
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;

    println!(
        "Join bundle generated in {} ({} bootstrap peers)",
        out_dir.display(),
        manifest.bootstrap_peers.len()
    );
    Ok(())
}

fn render_join_zebrad_config(
    run_dir: &Path,
    config: &Config,
    local_genesis: &LocalGenesisConfig,
) -> Result<String> {
    let template_path = run_dir.join("zebrad.toml");
    let template = if template_path.exists() {
        std::fs::read_to_string(&template_path)
            .with_context(|| format!("failed to read {}", template_path.display()))?
    } else {
        zebra_config::template_for(config.network_kind)?
    };
    let toml_network = zebra_config::testnet_toml_parameters(&template)
        .with_context(|| format!("invalid testnet parameters in {}", template_path.display()))?;
    let daa = toml_network
        .daa
        .with_missing_from(config.daa)
        .with_missing_from(DaaConfig::tuned_25s_defaults());
    let pow_start_height = payload_pow_start_height(&run_dir.join("payload/local_genesis"))?;
    let local_testnet = LocalTestnetParameters {
        network_name: local_genesis.network_name.clone(),
        network_magic: local_genesis.network_magic,
        target_difficulty_limit: local_genesis.target_difficulty_limit.clone(),
        disable_pow: local_genesis.disable_pow,
        genesis_hash: local_genesis.genesis_hash.clone(),
        checkpoints_path: JOIN_CHECKPOINTS_PATH.to_string(),
        slow_start_interval: local_genesis.slow_start_interval,
        pre_blossom_halving_interval: local_genesis.pre_blossom_halving_interval,
        activation_height: local_genesis.activation_heights.overwinter,
        lockbox_disbursements: zebra_config::default_nu6_1_lockbox_disbursements()?,
        post_blossom_pow_target_spacing: None,
        daa,
        pow_start_height,
    };
    let observer = observer_instance();
    let active_instances = active_instances(config);
    let mut rendered = zebra_config::generate_node_config(
        &template,
        NetworkKind::LocalGenesis,
        &observer,
        &active_instances,
    )?;
    rendered = zebra_config::set_miner_address(&rendered, DEFAULT_OBSERVER_MINER_ADDRESS)?;
    rendered = zebra_config::apply_local_testnet_parameters(&rendered, &local_testnet)?;
    rendered = set_toml_string_in_section(
        &rendered,
        "state",
        "cache_dir",
        &format!("{JOIN_INSTALL_ROOT}/state"),
    )?;
    rendered = set_toml_string_in_section(&rendered, "rpc", "listen_addr", "127.0.0.1:18232")?;
    zebra_config::verify_local_testnet_parameters(&rendered, &local_testnet)
        .context("rendered invalid join zebrad.toml")?;
    Ok(rendered)
}

fn bootstrap_peers(config: &Config) -> Result<Vec<String>> {
    if config.network_kind != NetworkKind::LocalGenesis {
        anyhow::bail!("join bundles are only supported for local-genesis experiments");
    }

    Ok(active_instances(config)
        .iter()
        .map(|inst| format!("{}:{}", inst.public_ip, config.p2p_port()))
        .collect())
}

fn active_instances(config: &Config) -> Vec<Instance> {
    config
        .miners
        .iter()
        .filter(|inst| !inst.public_ip.is_empty() && inst.public_ip != "TBD")
        .cloned()
        .collect()
}

fn observer_instance() -> Instance {
    Instance {
        node_type: NodeType::Miner,
        public_ip: "TBD".to_string(),
        private_ip: "TBD".to_string(),
        provider: Provider::DigitalOcean,
        slug: "observer".to_string(),
        region: "local".to_string(),
        name: "__join_observer__".to_string(),
        tags: vec!["kresko".to_string(), "join-bundle".to_string()],
        tier: "observer".to_string(),
    }
}

fn payload_pow_start_height(local_genesis_dir: &Path) -> Result<Option<u32>> {
    let manifest_path = local_genesis_dir.join("manifest.json");
    if !manifest_path.is_file() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: PayloadPremineManifest = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    Ok(manifest.pow_start_height)
}

fn set_toml_string_in_section(
    config: &str,
    section: &str,
    key: &str,
    value: &str,
) -> Result<String> {
    let mut parsed: toml::Value = toml::from_str(config).context("failed to parse zebrad.toml")?;
    let root = parsed
        .as_table_mut()
        .context("zebrad.toml root should be a TOML table")?;
    let section_table = root
        .entry(section.to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .with_context(|| format!("[{section}] should be a TOML table"))?;
    section_table.insert(key.to_string(), toml::Value::String(value.to_string()));
    toml::to_string_pretty(&parsed).context("failed to serialize zebrad.toml")
}

fn render_join_script() -> String {
    JOIN_SCRIPT_TEMPLATE
        .replace("@@DEFAULT_RUSTFLAGS@@", DEFAULT_RUSTFLAGS)
        .replace("@@JOIN_BUNDLE_DIR@@", JOIN_BUNDLE_DIR)
        .replace("@@JOIN_INSTALL_ROOT@@", JOIN_INSTALL_ROOT)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

const JOIN_SCRIPT_TEMPLATE: &str = r#"#!/usr/bin/env bash
set -Eeuo pipefail

MINE=0
FOREGROUND=0
DRY_RUN=0
MINER_ADDRESS=""
ORIGINAL_ARGS=("$@")
INSTALL_ROOT="@@JOIN_INSTALL_ROOT@@"
BUNDLE_DIR="@@JOIN_BUNDLE_DIR@@"
ZEBRA_DIR=""
KRESKO_DIR=""
LOG_DIR="${NU7_LOG_DIR:-/var/log/nu7-testnet}"

usage() {
    cat <<'USAGE'
Usage: join-nu7-testnet.sh [--mine] [--miner-address ADDRESS] [--foreground] [--dry-run]

Installs Zebra from source, seeds the NU7 local genesis blocks, and starts zebrad.
With --mine, also builds Kresko from source and starts kresko mine after RPC is ready.
USAGE
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --mine)
            MINE=1
            shift
            ;;
        --miner-address)
            MINER_ADDRESS="${2:-}"
            if [ -z "$MINER_ADDRESS" ]; then
                echo "missing value for --miner-address" >&2
                exit 2
            fi
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
        --zebra-dir)
            ZEBRA_DIR="${2:-}"
            if [ -z "$ZEBRA_DIR" ]; then
                echo "missing value for --zebra-dir" >&2
                exit 2
            fi
            shift 2
            ;;
        --kresko-dir)
            KRESKO_DIR="${2:-}"
            if [ -z "$KRESKO_DIR" ]; then
                echo "missing value for --kresko-dir" >&2
                exit 2
            fi
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

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ZEBRA_DIR="${ZEBRA_DIR:-$INSTALL_ROOT/zebra}"
KRESKO_DIR="${KRESKO_DIR:-/opt/nu7-join-src/kresko}"
CONFIG_PATH="/root/.config/zebrad.toml"
RPC_PORT="${KRESKO_RPC_PORT:-18232}"
RPC_URL="http://127.0.0.1:${RPC_PORT}"
BOOTSTRAP_CONFIG="/root/.config/zebrad.join-bootstrap.toml"
RUSTFLAGS_VALUE='@@DEFAULT_RUSTFLAGS@@'

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "required command not found: $1" >&2
        exit 1
    fi
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
    if [ "$checkpoint_path" != "@@JOIN_BUNDLE_DIR@@/local_genesis/checkpoints.txt" ]; then
        echo "zebrad.join.toml checkpoints path does not point at @@JOIN_BUNDLE_DIR@@/local_genesis/checkpoints.txt" >&2
        exit 1
    fi
    if grep -q 'initial_testnet_peers = \[\]' "$BUNDLE_DIR/zebrad.join.toml"; then
        echo "zebrad.join.toml has empty initial_testnet_peers" >&2
        exit 1
    fi
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
    "$ZEBRA_DIR/target/release/zebrad" -c "$BOOTSTRAP_CONFIG" start >"$bootstrap_log" 2>&1 &
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

prepare_kresko_source_layout() {
    # kresko.giga-refactor currently has local path dependencies that match the
    # developer checkout layout. Recreate that sibling layout without copying
    # the Zebra workspace built above.
    local trace_git_url trace_ref source_root trace_root zebra_link
    trace_git_url="$(jq -r '.zebra_jsonl_trace_git_url' "$BUNDLE_DIR/join-manifest.json")"
    trace_ref="$(jq -r '.zebra_jsonl_trace_ref' "$BUNDLE_DIR/join-manifest.json")"
    source_root="$(dirname "$KRESKO_DIR")"
    trace_root="$source_root/zebra"
    zebra_link="$source_root/nu7-testnet"
    mkdir -p "$source_root"
    if [ ! -e "$zebra_link" ]; then
        ln -s "$ZEBRA_DIR" "$zebra_link"
    fi
    if [ ! -e "$trace_root/zebra-jsonl-trace" ]; then
        if [ ! -d "$trace_root/.git" ]; then
            git clone "$trace_git_url" "$trace_root"
        fi
        git -C "$trace_root" fetch origin "$trace_ref"
        git -C "$trace_root" checkout --detach FETCH_HEAD
    fi
    if [ ! -d "$trace_root/zebra-jsonl-trace" ]; then
        echo "zebra-jsonl-trace was not found in $trace_root after checkout" >&2
        exit 1
    fi
}

build_kresko_if_mining() {
    [ "$MINE" -eq 1 ] || return 0

    local kresko_git_url kresko_ref
    kresko_git_url="$(jq -r '.kresko_git_url' "$BUNDLE_DIR/join-manifest.json")"
    kresko_ref="$(jq -r '.kresko_ref' "$BUNDLE_DIR/join-manifest.json")"
    if [ ! -d "$KRESKO_DIR/.git" ]; then
        git clone "$kresko_git_url" "$KRESKO_DIR"
    fi
    git -C "$KRESKO_DIR" fetch origin "$kresko_ref"
    git -C "$KRESKO_DIR" checkout --detach FETCH_HEAD
    prepare_kresko_source_layout
    cargo build --manifest-path "$KRESKO_DIR/Cargo.toml" --locked --release --bin kresko
    install -m 0755 "$KRESKO_DIR/target/release/kresko" /usr/local/bin/kresko
}

if [ "$DRY_RUN" -eq 1 ]; then
    require_cmd jq
    require_cmd sha256sum
    BUNDLE_DIR="$SCRIPT_DIR"
    validate_join_inputs
    echo "dry run OK"
    exit 0
fi

if [ "$(id -u)" -ne 0 ]; then
    exec sudo -E bash "$0" "${ORIGINAL_ARGS[@]}"
fi

export DEBIAN_FRONTEND=noninteractive
apt_retry update -y -o Dpkg::Options::="--force-confdef" -o Dpkg::Options::="--force-confold"
apt_retry install -y build-essential ca-certificates clang curl git jq libssl-dev pkg-config chrony python3 tmux

systemctl enable chrony || true
systemctl start chrony || true

if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck disable=SC1091
    source /root/.cargo/env
fi

mkdir -p "$INSTALL_ROOT" "$BUNDLE_DIR" "$LOG_DIR" /root/.config
if [ "$(realpath "$SCRIPT_DIR")" != "$(realpath "$BUNDLE_DIR" 2>/dev/null || true)" ]; then
    cp -a "$SCRIPT_DIR"/. "$BUNDLE_DIR"/
fi

validate_join_inputs
cp "$BUNDLE_DIR/zebrad.join.toml" "$CONFIG_PATH"
if [ "$MINE" -eq 1 ] && [ -z "$MINER_ADDRESS" ]; then
    MINER_ADDRESS="$(generate_miner_address)"
    echo "generated miner address: $MINER_ADDRESS"
fi
if [ -n "$MINER_ADDRESS" ]; then
    replace_miner_address "$MINER_ADDRESS"
fi

ZEBRA_GIT_URL="$(jq -r '.zebra_git_url' "$BUNDLE_DIR/join-manifest.json")"
ZEBRA_REF="$(jq -r '.zebra_ref' "$BUNDLE_DIR/join-manifest.json")"
if [ ! -d "$ZEBRA_DIR/.git" ]; then
    git clone "$ZEBRA_GIT_URL" "$ZEBRA_DIR"
fi
git -C "$ZEBRA_DIR" fetch origin "$ZEBRA_REF"
git -C "$ZEBRA_DIR" checkout --detach FETCH_HEAD

export RUSTFLAGS="$RUSTFLAGS_VALUE"
export CXXFLAGS="${CXXFLAGS:--include cstdint}"
cargo --version
cargo build --manifest-path "$ZEBRA_DIR/Cargo.toml" --locked --release --bin zebrad
build_kresko_if_mining

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
    exec "$ZEBRA_DIR/target/release/zebrad" -c "$CONFIG_PATH" start
fi

tmux new-session -d -s nu7-zebrad "bash -lc '$ZEBRA_DIR/target/release/zebrad -c $CONFIG_PATH start 2>&1 | tee -a $LOG_DIR/zebrad.log'"
echo "zebrad started in tmux session nu7-zebrad"
echo "logs: $LOG_DIR/zebrad.log"
"#;

#[cfg(test)]
mod tests {
    use super::{
        JOIN_SCRIPT_TEMPLATE, JoinManifest, render_join_script, set_toml_string_in_section,
    };
    use crate::config::{
        Config, DaaConfig, EquihashParameterSet, Instance, LocalGenesisActivationHeights,
        LocalGenesisConfig, MiningMode, NetworkKind, NodeType, OrchardTxblastConfig, Provider,
    };

    fn miner(name: &str, ip: &str) -> Instance {
        Instance {
            node_type: NodeType::Miner,
            public_ip: ip.to_string(),
            private_ip: "10.0.0.1".to_string(),
            provider: Provider::DigitalOcean,
            slug: "s-1vcpu-1gb".to_string(),
            region: "nyc3".to_string(),
            name: name.to_string(),
            tags: vec!["kresko".to_string()],
            tier: "full".to_string(),
        }
    }

    fn test_config() -> Config {
        Config {
            miners: vec![
                miner("miner-0-abc", "1.1.1.1"),
                miner("miner-1-def", "2.2.2.2"),
                miner("miner-2-ghi", "TBD"),
            ],
            chain_id: "nu7-test".to_string(),
            experiment: "nu7".to_string(),
            ssh_pub_key_path: String::new(),
            ssh_key_name: String::new(),
            ssh_key_path: String::new(),
            provider: Provider::DigitalOcean,
            network_kind: NetworkKind::LocalGenesis,
            mining_mode: MiningMode::Pow,
            block_time_secs: Some(25),
            equihash_params: EquihashParameterSet::Regtest,
            daa: DaaConfig::tuned_25s_defaults(),
            orchard_txblast: OrchardTxblastConfig::default(),
            local_genesis: Some(LocalGenesisConfig {
                network_name: "Kresko_nu7".to_string(),
                network_magic: [1, 2, 3, 4],
                target_difficulty_limit: "0x0f".to_string(),
                target_spacing_secs: Some(25),
                disable_pow: false,
                genesis_hash: "00".repeat(32),
                seeded_tip_hash: Some("11".repeat(32)),
                genesis_hex: "abcd".to_string(),
                slow_start_interval: 0,
                pre_blossom_halving_interval: 144,
                activation_heights: LocalGenesisActivationHeights {
                    overwinter: 1,
                    sapling: 1,
                    blossom: 1,
                    heartwood: 1,
                    canopy: 1,
                    nu5: 1,
                    nu6: 1,
                    nu6_1: 1,
                    nu7: 1,
                },
                maturity_padding_block_count: 1,
                premine_block_count: 1,
                seeded_block_count: 2,
                bootstrap_treasury_key: None,
                funded_keys: Vec::new(),
            }),
        }
    }

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "kresko-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be after UNIX_EPOCH")
                .as_nanos()
        ));
        path
    }

    #[test]
    fn string_section_replacement_updates_existing_key() {
        let rendered = set_toml_string_in_section(
            "[rpc]\nlisten_addr = \"0.0.0.0:18232\"\n",
            "rpc",
            "listen_addr",
            "127.0.0.1:18232",
        )
        .expect("replacement should succeed");

        assert!(rendered.contains("listen_addr = \"127.0.0.1:18232\""));
        assert!(!rendered.contains("0.0.0.0:18232"));
    }

    #[test]
    fn join_script_has_arg_parsing_and_source_builds() {
        let script = render_join_script();

        assert!(script.contains("--mine"));
        assert!(script.contains("--miner-address"));
        assert!(script.contains("build_kresko_if_mining"));
        assert!(script.contains("git clone \"$kresko_git_url\""));
        assert!(script.contains("validate_bundle_hashes"));
        assert!(!JOIN_SCRIPT_TEMPLATE.contains("@@DEFAULT_RUSTFLAGS@@ --"));
    }

    #[test]
    fn generated_join_bundle_manifest_and_observer_config_are_source_only() {
        let run_dir = unique_temp_dir("join-run");
        let out_dir = unique_temp_dir("join-out");
        std::fs::create_dir_all(run_dir.join("payload/local_genesis"))
            .expect("should create payload dir");
        let config = test_config();
        std::fs::write(
            run_dir.join("config.json"),
            serde_json::to_vec_pretty(&config).expect("config should serialize"),
        )
        .expect("should write config");
        std::fs::write(run_dir.join("payload/local_genesis/genesis.hex"), "abcd\n")
            .expect("should write genesis");
        std::fs::write(
            run_dir.join("payload/local_genesis/premine_blocks.hex"),
            "dcba\n",
        )
        .expect("should write premine");
        std::fs::write(
            run_dir.join("payload/local_genesis/checkpoints.txt"),
            format!("0 {}\n1 {}", "00".repeat(32), "11".repeat(32)),
        )
        .expect("should write checkpoints");

        super::run(
            run_dir.to_str().expect("temp path is utf8"),
            "https://github.com/ZcashFoundation/zebra.git",
            "evan/nu7/testnet",
            "https://github.com/evan-forbes/kresko.git",
            "giga-refactor",
            "https://github.com/evan-forbes/zebra.git",
            "evan/benchmark-worst-case-block-verification",
            out_dir.to_str().expect("temp path is utf8"),
        )
        .expect("join bundle generation should succeed");

        let manifest_path = out_dir.join("join-manifest.json");
        let manifest: JoinManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("manifest should exist"))
                .expect("manifest should parse");
        assert_eq!(manifest.chain_id, "nu7-test");
        assert_eq!(
            manifest.bootstrap_peers,
            vec!["1.1.1.1:18233", "2.2.2.2:18233"]
        );
        assert_eq!(manifest.kresko_ref, "giga-refactor");
        assert_eq!(
            manifest.zebra_jsonl_trace_ref,
            "evan/benchmark-worst-case-block-verification"
        );
        assert!(manifest.files.contains_key("local_genesis/genesis.hex"));
        assert!(!manifest.files.contains_key("join-manifest.json"));

        let join_config =
            std::fs::read_to_string(out_dir.join("zebrad.join.toml")).expect("config should exist");
        let parsed: toml::Value = toml::from_str(&join_config).expect("join config should parse");
        let peers = parsed
            .get("network")
            .and_then(|network| network.get("initial_testnet_peers"))
            .and_then(toml::Value::as_array)
            .expect("peers should exist")
            .iter()
            .map(|value| value.as_str().expect("peer should be string"))
            .collect::<Vec<_>>();
        assert_eq!(peers, vec!["1.1.1.1:18233", "2.2.2.2:18233"]);
        assert_eq!(
            parsed
                .get("mining")
                .and_then(|mining| mining.get("miner_address"))
                .and_then(toml::Value::as_str),
            Some("t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v"),
        );
        assert_eq!(
            parsed
                .get("state")
                .and_then(|state| state.get("cache_dir"))
                .and_then(toml::Value::as_str),
            Some("/opt/nu7-testnet/state"),
        );
        assert!(!join_config.contains("secret_key_hex"));
        assert!(!out_dir.join("local_genesis/funded_keys.json").exists());

        let _ = std::fs::remove_dir_all(run_dir);
        let _ = std::fs::remove_dir_all(out_dir);
    }
}
