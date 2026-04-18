use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::{Config, MiningMode, OrchardTxblastConfig, Provider, resolve_value};
use crate::zebra_config;

const DEFAULT_TARGET_SPACING_SECS: u32 = 75;

pub fn run(
    chain_id: &str,
    experiment: &str,
    provider: &str,
    ssh_pub_key_path: Option<String>,
    ssh_key_name: Option<String>,
    mining_mode: MiningMode,
    block_time_secs: Option<u32>,
    env_source: Option<&str>,
) -> Result<()> {
    let provider: Provider = provider.parse()?;
    let dir = Path::new(experiment);
    if dir.exists() {
        anyhow::bail!("Experiment directory '{}' already exists", experiment);
    }

    std::fs::create_dir_all(dir.join("payload"))?;
    std::fs::create_dir_all(dir.join("data"))?;
    std::fs::create_dir_all(dir.join("runs"))?;
    std::fs::create_dir_all(dir.join("runs/examples"))?;
    std::fs::create_dir_all(dir.join("scripts"))?;
    std::fs::create_dir_all(dir.join("scripts/steps"))?;
    std::fs::create_dir_all(dir.join("state"))?;

    std::fs::write(dir.join("zebrad.toml"), zebra_config::DEFAULT_ZEBRAD_TOML)?;
    std::fs::write(dir.join("scripts/node_init.sh"), NODE_INIT_SH)?;
    std::fs::write(dir.join("scripts/vars.sh"), VARS_SH_TEMPLATE)?;
    std::fs::write(dir.join("scripts/common.sh"), render_common_sh())?;
    std::fs::write(dir.join("scripts/bootstrap.sh"), render_bootstrap_script())?;

    let ssh_key_name_val = resolve_value(
        ssh_key_name.as_deref(),
        "KRESKO_SSH_KEY_NAME",
        &default_ssh_key_name(),
    );
    let ssh_pub_key_path_val = resolve_value(
        ssh_pub_key_path.as_deref(),
        "KRESKO_SSH_PUB_KEY_PATH",
        "~/.ssh/id_ed25519.pub",
    );
    let ssh_key_path_val = resolve_value(None, "KRESKO_SSH_KEY_PATH", "~/.ssh/id_ed25519");

    let config = Config {
        miners: Vec::new(),
        chain_id: chain_id.to_string(),
        experiment: experiment.to_string(),
        ssh_pub_key_path: ssh_pub_key_path_val.clone(),
        ssh_key_name: ssh_key_name_val.clone(),
        ssh_key_path: ssh_key_path_val.clone(),
        provider,
        mining_mode,
        block_time_secs,
        orchard_txblast: OrchardTxblastConfig::default(),
        local_genesis: None,
    };

    config.save(dir)?;

    let env_metadata = write_env(
        dir,
        provider,
        &ssh_key_name_val,
        &ssh_pub_key_path_val,
        &ssh_key_path_val,
        env_source,
    )?;

    let target_spacing_secs = block_time_secs.unwrap_or(DEFAULT_TARGET_SPACING_SECS);
    std::fs::write(
        dir.join("PLAN.md"),
        render_plan(
            experiment,
            chain_id,
            provider,
            mining_mode,
            target_spacing_secs,
            env_metadata.shared_source.as_deref(),
        ),
    )?;
    std::fs::write(
        dir.join("AGENTS.md"),
        render_agents(experiment, target_spacing_secs),
    )?;
    std::fs::write(dir.join("flups.md"), FLUPS_TEMPLATE)?;
    std::fs::write(
        dir.join("runs/01_bounded_pow.env"),
        RUN_BOUNDED_POW_MANIFEST_TEMPLATE,
    )?;
    std::fs::write(
        dir.join("runs/examples/01_bounded_generate.env.example"),
        RUN_BOUNDED_GENERATE_MANIFEST_TEMPLATE,
    )?;
    std::fs::write(
        dir.join("runs/examples/02_txblast_shielded.env.example"),
        RUN_TXBLAST_MANIFEST_TEMPLATE,
    )?;
    std::fs::write(dir.join("scripts/init.sh"), render_init_script())?;
    std::fs::write(
        dir.join("scripts/collect_artifacts.sh"),
        render_collect_artifacts_script(),
    )?;
    std::fs::write(
        dir.join("scripts/run_bounded_pow.sh"),
        render_run_bounded_pow_script(),
    )?;
    std::fs::write(
        dir.join("scripts/run_bounded_generate.sh"),
        render_run_bounded_generate_script(),
    )?;
    std::fs::write(
        dir.join("scripts/run_txblast_sample.sh"),
        render_run_txblast_sample_script(),
    )?;
    std::fs::write(
        dir.join("scripts/start_campaign.sh"),
        START_CAMPAIGN_COMPAT_SH,
    )?;
    std::fs::write(
        dir.join("scripts/steps/01_build_binaries.sh"),
        render_step_build_binaries(),
    )?;
    std::fs::write(
        dir.join("scripts/steps/02_generate_genesis.sh"),
        render_step_generate_genesis(),
    )?;
    std::fs::write(dir.join("scripts/steps/03_deploy.sh"), render_step_deploy())?;
    std::fs::write(
        dir.join("scripts/steps/04_validate.sh"),
        render_step_validate(),
    )?;
    std::fs::write(
        dir.join("scripts/steps/05_run_experiment.sh"),
        render_step_run_experiment(),
    )?;
    std::fs::write(
        dir.join("scripts/steps/06_collect_artifacts.sh"),
        render_step_collect_artifacts(),
    )?;
    std::fs::write(
        dir.join("scripts/steps/07_teardown.sh"),
        render_step_teardown(),
    )?;

    make_executable(&dir.join("scripts/node_init.sh"))?;
    make_executable(&dir.join("scripts/common.sh"))?;
    make_executable(&dir.join("scripts/bootstrap.sh"))?;
    make_executable(&dir.join("scripts/init.sh"))?;
    make_executable(&dir.join("scripts/collect_artifacts.sh"))?;
    make_executable(&dir.join("scripts/run_bounded_pow.sh"))?;
    make_executable(&dir.join("scripts/run_bounded_generate.sh"))?;
    make_executable(&dir.join("scripts/run_txblast_sample.sh"))?;
    make_executable(&dir.join("scripts/start_campaign.sh"))?;
    make_executable(&dir.join("scripts/steps/01_build_binaries.sh"))?;
    make_executable(&dir.join("scripts/steps/02_generate_genesis.sh"))?;
    make_executable(&dir.join("scripts/steps/03_deploy.sh"))?;
    make_executable(&dir.join("scripts/steps/04_validate.sh"))?;
    make_executable(&dir.join("scripts/steps/05_run_experiment.sh"))?;
    make_executable(&dir.join("scripts/steps/06_collect_artifacts.sh"))?;
    make_executable(&dir.join("scripts/steps/07_teardown.sh"))?;

    println!("Initialized experiment '{experiment}' for chain '{chain_id}'");
    println!("  Directory: {}", dir.display());
    println!("  Provider:  {provider}");
    println!("  .env:      {}/.env", dir.display());
    println!("  PLAN.md:   {}/PLAN.md", dir.display());
    println!("  AGENTS.md: {}/AGENTS.md", dir.display());
    println!("  flups.md:  {}/flups.md", dir.display());
    println!("  bootstrap: {}/scripts/bootstrap.sh", dir.display());
    println!("  init.sh:   {}/scripts/init.sh", dir.display());
    println!();
    println!("Credential sources:");
    for entry in &env_metadata.entries {
        println!("  {}: {}", entry.key, entry.source);
    }
    println!();
    println!("Next steps:");
    println!("  1. cd {experiment}");
    println!("  2. Review PLAN.md and AGENTS.md");
    println!("  3. MINER_COUNT=<N> scripts/bootstrap.sh");
    println!("  4. kresko up");
    println!("  5. Customize runs/*.env and scripts/steps/05_run_experiment.sh if needed");
    println!("  6. scripts/init.sh");

    Ok(())
}

#[derive(Debug, Clone)]
struct EnvEntry {
    key: &'static str,
    value: String,
    source: String,
}

#[derive(Debug, Clone)]
struct EnvMetadata {
    entries: Vec<EnvEntry>,
    shared_source: Option<String>,
}

fn write_env(
    dir: &Path,
    provider: Provider,
    ssh_key_name: &str,
    ssh_pub_key_path: &str,
    ssh_key_path: &str,
    env_source: Option<&str>,
) -> Result<EnvMetadata> {
    let explicit_env_source = env_source.map(PathBuf::from);
    let shared_env_path = match explicit_env_source {
        Some(path) => Some(path),
        None => discover_shared_env_path(dir)?,
    };
    let shared_env = match shared_env_path.as_deref() {
        Some(path) => read_env_file(path)?,
        None => BTreeMap::new(),
    };
    let shared_source = shared_env_path.map(|path| path.display().to_string());

    let (default_region, default_endpoint) = match provider {
        Provider::DigitalOcean => ("nyc3", "https://nyc3.digitaloceanspaces.com"),
        Provider::GoogleCloud => ("us-east-1", ""),
        Provider::Linode => ("us-east-1", ""),
    };

    let env_entries = vec![
        EnvEntry {
            key: "DIGITALOCEAN_TOKEN",
            value: resolve_env_entry("DIGITALOCEAN_TOKEN", "", &shared_env),
            source: resolve_env_source(
                "DIGITALOCEAN_TOKEN",
                "",
                &shared_env,
                shared_source.as_deref(),
            ),
        },
        EnvEntry {
            key: "LINODE_TOKEN",
            value: resolve_env_entry("LINODE_TOKEN", "", &shared_env),
            source: resolve_env_source("LINODE_TOKEN", "", &shared_env, shared_source.as_deref()),
        },
        EnvEntry {
            key: "GOOGLE_CLOUD_PROJECT",
            value: resolve_env_entry("GOOGLE_CLOUD_PROJECT", "", &shared_env),
            source: resolve_env_source(
                "GOOGLE_CLOUD_PROJECT",
                "",
                &shared_env,
                shared_source.as_deref(),
            ),
        },
        EnvEntry {
            key: "GOOGLE_CLOUD_KEY_JSON_PATH",
            value: resolve_env_entry("GOOGLE_CLOUD_KEY_JSON_PATH", "", &shared_env),
            source: resolve_env_source(
                "GOOGLE_CLOUD_KEY_JSON_PATH",
                "",
                &shared_env,
                shared_source.as_deref(),
            ),
        },
        EnvEntry {
            key: "KRESKO_SSH_KEY_NAME",
            value: ssh_key_name.to_string(),
            source: source_for_value(
                "KRESKO_SSH_KEY_NAME",
                ssh_key_name,
                &shared_env,
                shared_source.as_deref(),
                "generated default",
            ),
        },
        EnvEntry {
            key: "KRESKO_SSH_PUB_KEY_PATH",
            value: ssh_pub_key_path.to_string(),
            source: source_for_value(
                "KRESKO_SSH_PUB_KEY_PATH",
                ssh_pub_key_path,
                &shared_env,
                shared_source.as_deref(),
                "generated default",
            ),
        },
        EnvEntry {
            key: "KRESKO_SSH_KEY_PATH",
            value: ssh_key_path.to_string(),
            source: source_for_value(
                "KRESKO_SSH_KEY_PATH",
                ssh_key_path,
                &shared_env,
                shared_source.as_deref(),
                "generated default",
            ),
        },
        EnvEntry {
            key: "AWS_ACCESS_KEY_ID",
            value: resolve_env_entry("AWS_ACCESS_KEY_ID", "", &shared_env),
            source: resolve_env_source(
                "AWS_ACCESS_KEY_ID",
                "",
                &shared_env,
                shared_source.as_deref(),
            ),
        },
        EnvEntry {
            key: "AWS_SECRET_ACCESS_KEY",
            value: resolve_env_entry("AWS_SECRET_ACCESS_KEY", "", &shared_env),
            source: resolve_env_source(
                "AWS_SECRET_ACCESS_KEY",
                "",
                &shared_env,
                shared_source.as_deref(),
            ),
        },
        EnvEntry {
            key: "AWS_DEFAULT_REGION",
            value: resolve_env_entry("AWS_DEFAULT_REGION", default_region, &shared_env),
            source: resolve_env_source(
                "AWS_DEFAULT_REGION",
                default_region,
                &shared_env,
                shared_source.as_deref(),
            ),
        },
        EnvEntry {
            key: "AWS_S3_BUCKET",
            value: resolve_env_entry("AWS_S3_BUCKET", "kresko-data", &shared_env),
            source: resolve_env_source(
                "AWS_S3_BUCKET",
                "kresko-data",
                &shared_env,
                shared_source.as_deref(),
            ),
        },
        EnvEntry {
            key: "AWS_S3_ENDPOINT",
            value: resolve_env_entry("AWS_S3_ENDPOINT", default_endpoint, &shared_env),
            source: resolve_env_source(
                "AWS_S3_ENDPOINT",
                default_endpoint,
                &shared_env,
                shared_source.as_deref(),
            ),
        },
    ];

    let env_content = render_env_file(&env_entries, shared_source.as_deref());
    std::fs::write(dir.join(".env"), env_content)?;

    Ok(EnvMetadata {
        entries: env_entries,
        shared_source,
    })
}

fn render_env_file(entries: &[EnvEntry], shared_source: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("# Generated by kresko init\n");
    out.push_str("# Precedence: explicit init flags > shared env file > current shell environment > defaults\n");
    if let Some(path) = shared_source {
        out.push_str(&format!("# Shared env file: {path}\n"));
    } else {
        out.push_str("# Shared env file: none discovered\n");
    }
    out.push('\n');

    for section in [
        ("DigitalOcean Configuration", &["DIGITALOCEAN_TOKEN"][..]),
        ("Linode Configuration", &["LINODE_TOKEN"][..]),
        (
            "Google Cloud Configuration",
            &["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_KEY_JSON_PATH"][..],
        ),
        (
            "SSH Configuration",
            &[
                "KRESKO_SSH_KEY_NAME",
                "KRESKO_SSH_PUB_KEY_PATH",
                "KRESKO_SSH_KEY_PATH",
            ][..],
        ),
        (
            "S3 Configuration (DigitalOcean Spaces, GCS, Linode Object Storage, or AWS S3)",
            &[
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_DEFAULT_REGION",
                "AWS_S3_BUCKET",
                "AWS_S3_ENDPOINT",
            ][..],
        ),
    ] {
        out.push_str(&format!("# {}\n", section.0));
        for key in section.1 {
            if let Some(entry) = entries.iter().find(|entry| entry.key == *key) {
                out.push_str(&format!("# source: {}\n", entry.source));
                out.push_str(&format!(
                    "{}={}\n",
                    entry.key,
                    format_env_value(&entry.value)
                ));
            }
        }
        out.push('\n');
    }

    out
}

fn render_plan(
    experiment: &str,
    chain_id: &str,
    provider: Provider,
    mining_mode: MiningMode,
    target_spacing_secs: u32,
    shared_env_source: Option<&str>,
) -> String {
    let credential_source = shared_env_source.unwrap_or("current shell environment / defaults");
    format!(
        r#"# Experiment Plan

## Goal
- Primary objective:
- Secondary objective:

## Campaign
- Experiment: `{experiment}`
- Chain ID: `{chain_id}`
- Provider: `{provider}`
- Mining mode: `{mining_mode}`

## Credentials
- `.env` was populated programmatically by `kresko init`.
- Shared credential source used during init: `{credential_source}`
- Manual corrections needed:

## Execution
- `PLAN.md` is for planning and review only. Do not treat it as executable logic.
- `scripts/bootstrap.sh` is the standard helper for `kresko add` + binary build + `kresko genesis`.
- After `kresko init`, use `MINER_COUNT=<N> scripts/bootstrap.sh` as the default bootstrap path.
- Do not hand-sequence `kresko add` and `kresko genesis` unless the plan explicitly requires a non-default bootstrap flow.
- `scripts/init.sh` is the restartable campaign entrypoint.
- `scripts/steps/` contains numbered restartable steps.
- `runs/*.env` defines the ordered run sequence for the campaign.
- `runs/examples/*.env.example` contains additional built-in sample workloads that are not active by default.
- `scripts/steps/05_run_experiment.sh` executes those run manifests, records status, and collects artifacts per run.
- `scripts/run_bounded_pow.sh`, `scripts/run_bounded_generate.sh`, and `scripts/run_txblast_sample.sh` are built-in workload samples.
- Resume from a later stage with `START_AT=<NN> scripts/init.sh` or by running a specific step script directly.

## Topology
- Requested nodes:
- Minimum healthy nodes required to proceed:
- Acceptable failed-node percentage:
- Providers / regions:
- Late-created nodes should be: ignored / reconciled / added

## Build
- Build Ubuntu-compatible binaries by default: yes
- If disabled, explicit binary sources:
- If building, expected commands:
  - `make ubuntu` in `kresko`
  - `cargo xtask package ubuntu` in Zebra

## PoW
- Target block spacing in config: `{target_spacing_secs}`
- PoW profile passed to `kresko genesis`: `mainnet`
- Headroom bits passed to `kresko genesis`: `2`
- Calibration mode: `benchmark`
- Explicit sol/s override:
- Reason these settings are appropriate:
- If target block spacing changes, update the campaign defaults before `kresko genesis`.

## Provision
- Run `kresko up`
- Proceed when healthy node threshold is met: yes / no
- Reconcile late-created resources before teardown: yes / no

## Deploy
- Run `scripts/init.sh` after reviewing the generated defaults and customizing step `05_run_experiment.sh`.
- Deploy only to healthy nodes when partial startup is acceptable.
- Use `kresko deploy --nodes <selector> --restart-app-session` for targeted recovery.

## Workload
- Ordered run manifests: `runs/*.env`
- Additional sample manifests: `runs/examples/*.env.example`
- Execution step: `scripts/steps/05_run_experiment.sh`
- Built-in workload samples: `scripts/run_bounded_pow.sh`, `scripts/run_bounded_generate.sh`, `scripts/run_txblast_sample.sh`
- Stop condition:
- Abort condition:
- Recovery allowed: yes / no

## Monitoring
- Default cadence: every few minutes unless debugging
- Routine view: `kresko status --summary`
- Investigation view: `kresko status --deep`
- Conditions that trigger investigation:

## Artifacts
- Required downloads:
  - logs
  - heights
  - traces
  - experiment-specific derived outputs
- Use `kresko collect` before teardown unless explicitly waived.

## Teardown
- Clear to move to next run when stop condition is met and artifacts are downloaded.
- Clear to end final run when artifacts are downloaded and `kresko down` confirms no resources remain.
- Verify provider cleanup after teardown: yes

## Flups
- Keep `flups.md` updated during the campaign.
"#
    )
}

fn render_agents(experiment: &str, target_spacing_secs: u32) -> String {
    format!(
        r#"# AGENTS

1. Read `PLAN.md` before changing or running anything.
2. Keep planning in `PLAN.md`, operational notes in `flups.md`, and executable logic in `scripts/`.
3. Prefer `kresko` commands over custom glue unless the tool cannot do the job.
4. After `kresko init`, use `MINER_COUNT=<N> scripts/bootstrap.sh` as the default bootstrap path.
5. Do not hand-sequence `kresko add` and `kresko genesis` unless `PLAN.md` explicitly calls for a non-default bootstrap flow.
6. Define the campaign run sequence in `runs/*.env`. The generated default is a bounded-PoW sample that calls `scripts/run_bounded_pow.sh`.
7. Additional built-in run samples live under `runs/examples/*.env.example`; copy one into `runs/` when you want to activate it.
8. Adjust `scripts/steps/05_run_experiment.sh` only when the default per-run loop is not enough.
9. Use `scripts/init.sh` as the default executable entrypoint.
10. Resume from a later point with `START_AT=<NN> scripts/init.sh`, or rerun a specific step script directly.
11. Step scripts write markers under `state/`. Use `FORCE_STEPS=1` when intentionally rerunning a completed step after changing its inputs.
12. Keep `flups.md` updated as issues are discovered.
13. Use `kresko status --summary` for routine monitoring and avoid polling more than every few minutes unless debugging.
14. Use `kresko status --deep` before assuming a node is dead; distinguish host reachability, tmux state, and RPC health.
15. For targeted recovery, prefer:
    - `kresko deploy --nodes <selector> --reuse-app-session` when the app is already healthy
    - `kresko deploy --nodes <selector> --restart-app-session` when the app needs to be restarted
16. Every run must end in one of two ways:
    - artifacts collected, then proceed to the next run
    - artifacts collected, then final teardown
17. Unless the plan says otherwise, collect artifacts with `kresko collect` before `kresko down`.

Defaults for this campaign scaffold:
- experiment: `{experiment}`
- target block spacing: `{target_spacing_secs}` seconds
- PoW profile: `mainnet`
- headroom bits: `2`
"#
    )
}

fn render_common_sh() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EXPERIMENT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
WORKSPACE_ROOT="$(cd "${EXPERIMENT_DIR}/../.." && pwd)"
STEPS_DIR="${SCRIPT_DIR}/steps"
STATE_DIR="${EXPERIMENT_DIR}/state"

KRESKO_REPO="${KRESKO_REPO:-${WORKSPACE_ROOT}/kresko}"
ZEBRA_REPO="${ZEBRA_REPO:-${WORKSPACE_ROOT}/zebra}"

BUILD_BINARIES="${BUILD_BINARIES:-auto}"
POW_ADJUST="${POW_ADJUST:-0.0}"
NO_POW_CALIBRATION="${NO_POW_CALIBRATION:-0}"
DEPLOY_NODES="${DEPLOY_NODES:-}"
IGNORE_FAILED_MINERS="${IGNORE_FAILED_MINERS:-1}"
REUSE_APP_SESSION="${REUSE_APP_SESSION:-0}"
RESTART_APP_SESSION="${RESTART_APP_SESSION:-0}"
STATUS_DEEP_ON_VALIDATE="${STATUS_DEEP_ON_VALIDATE:-0}"
DATA_SUBDIR="${DATA_SUBDIR:-}"
SKIP_DOWN="${SKIP_DOWN:-0}"
FORCE_STEPS="${FORCE_STEPS:-0}"

ZEBRAD_BINARY="${ZEBRAD_BINARY:-${ZEBRA_REPO}/target/ubuntu/zebrad}"
KRESKO_BINARY="${KRESKO_BINARY:-${KRESKO_REPO}/target/ubuntu/kresko}"

mkdir -p "${STATE_DIR}"

step_marker_path() {
  printf '%s/%s.done\n' "${STATE_DIR}" "$1"
}

step_should_skip() {
  local marker
  marker="$(step_marker_path "$1")"
  [[ "${FORCE_STEPS}" != "1" && -f "${marker}" ]]
}

mark_step_done() {
  date -u +"%Y-%m-%dT%H:%M:%SZ" > "$(step_marker_path "$1")"
}

announce_step() {
  printf '\n==> %s\n' "$1"
}

require_file() {
  if [[ ! -f "$1" ]]; then
    echo "missing required file: $1" >&2
    exit 1
  fi
}

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}
"#
    .to_string()
}

fn render_init_script() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STEPS_DIR="${SCRIPT_DIR}/steps"

START_AT="${START_AT:-${1:-01}}"
STOP_AFTER="${STOP_AFTER:-${2:-99}}"

printf 'Running step scripts %s through %s\n' "${START_AT}" "${STOP_AFTER}"

for step_path in "${STEPS_DIR}"/[0-9][0-9]_*.sh; do
  step_name="$(basename "${step_path}")"
  step_num="${step_name%%_*}"
  if (( 10#${step_num} < 10#${START_AT} || 10#${step_num} > 10#${STOP_AFTER} )); then
    continue
  fi
  "${step_path}"
done
"#
    .to_string()
}

fn render_bootstrap_script() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

MINER_COUNT="${MINER_COUNT:-${1:-}}"
ADD_PROVIDER="${ADD_PROVIDER:-}"
ADD_REGION="${ADD_REGION:-random}"
ADD_LOW_RESOURCE="${ADD_LOW_RESOURCE:-0}"
APPEND_MINERS="${APPEND_MINERS:-0}"

if [[ -z "${MINER_COUNT}" ]]; then
  cat >&2 <<'EOF'
usage: MINER_COUNT=<N> scripts/bootstrap.sh

Optional environment:
  ADD_PROVIDER=digitalocean|googlecloud|linode
  ADD_REGION=random|<region>
  ADD_LOW_RESOURCE=1
  APPEND_MINERS=1
EOF
  exit 1
fi

existing_miners="$(grep -c '"node_type":[[:space:]]*"miner"' "${EXPERIMENT_DIR}/config.json" || true)"
if [[ "${existing_miners}" != "0" && "${APPEND_MINERS}" != "1" ]]; then
  cat >&2 <<EOF
config already has ${existing_miners} miner entries.
Refusing to append more miners implicitly.
Set APPEND_MINERS=1 to add more, or run kresko add manually.
EOF
  exit 1
fi

announce_step "bootstrap add miners"
add_args=(-d "${EXPERIMENT_DIR}" -t miner -c "${MINER_COUNT}")
if [[ -n "${ADD_PROVIDER}" ]]; then
  add_args+=(--provider "${ADD_PROVIDER}")
fi
if [[ -n "${ADD_REGION}" ]]; then
  add_args+=(--region "${ADD_REGION}")
fi
if [[ "${ADD_LOW_RESOURCE}" == "1" ]]; then
  add_args+=(--low-resource)
fi

kresko add "${add_args[@]}"

announce_step "bootstrap build + genesis"
"${SCRIPT_DIR}/steps/01_build_binaries.sh"
"${SCRIPT_DIR}/steps/02_generate_genesis.sh"
"#
    .to_string()
}

fn render_step_build_binaries() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../common.sh"

STEP_ID="01_build_binaries"
if step_should_skip "${STEP_ID}"; then
  announce_step "${STEP_ID} (already done, skipping)"
  exit 0
fi

announce_step "01 build binaries"
if [[ "${BUILD_BINARIES}" == "auto" ]]; then
  (cd "${KRESKO_REPO}" && make ubuntu)
  (cd "${ZEBRA_REPO}" && cargo xtask package ubuntu)
else
  echo "BUILD_BINARIES=${BUILD_BINARIES}; skipping binary build"
fi

require_file "${ZEBRAD_BINARY}"
require_file "${KRESKO_BINARY}"
mark_step_done "${STEP_ID}"
"#
    .to_string()
}

fn render_step_generate_genesis() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../common.sh"

STEP_ID="02_generate_genesis"
payload_kresko="${EXPERIMENT_DIR}/payload/build/kresko"
payload_zebrad="${EXPERIMENT_DIR}/payload/build/zebrad"
payload_is_fresh=1
if [[ ! -f "${payload_kresko}" || ! -f "${payload_zebrad}" ]]; then
  payload_is_fresh=0
elif [[ "${KRESKO_BINARY}" -nt "${payload_kresko}" || "${ZEBRAD_BINARY}" -nt "${payload_zebrad}" ]]; then
  payload_is_fresh=0
fi

if step_should_skip "${STEP_ID}" && [[ "${payload_is_fresh}" == "1" ]]; then
  announce_step "${STEP_ID} (already done, skipping)"
  exit 0
fi

if [[ "${payload_is_fresh}" != "1" ]]; then
  announce_step "${STEP_ID} (payload binaries out of date, regenerating)"
fi

announce_step "02 generate genesis"
require_file "${ZEBRAD_BINARY}"
require_file "${KRESKO_BINARY}"

args=(
  --zebrad-binary "${ZEBRAD_BINARY}"
  --kresko-binary "${KRESKO_BINARY}"
  --pow-adjust "${POW_ADJUST}"
  -d "${EXPERIMENT_DIR}"
)

if [[ "${NO_POW_CALIBRATION}" == "1" ]]; then
  args+=(--no-pow-calibration)
fi

kresko genesis "${args[@]}"
mark_step_done "${STEP_ID}"
"#
    .to_string()
}

fn render_step_deploy() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../common.sh"

STEP_ID="03_deploy"
if step_should_skip "${STEP_ID}"; then
  announce_step "${STEP_ID} (already done, skipping)"
  exit 0
fi

announce_step "03 deploy"
args=(-d "${EXPERIMENT_DIR}")

if [[ -n "${DEPLOY_NODES}" ]]; then
  args+=(--nodes "${DEPLOY_NODES}")
fi
if [[ "${IGNORE_FAILED_MINERS}" == "1" ]]; then
  args+=(--ignore-failed-miners)
fi
if [[ "${REUSE_APP_SESSION}" == "1" ]]; then
  args+=(--reuse-app-session)
fi
if [[ "${RESTART_APP_SESSION}" == "1" ]]; then
  args+=(--restart-app-session)
fi

kresko deploy "${args[@]}"
mark_step_done "${STEP_ID}"
"#
    .to_string()
}

fn render_step_validate() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../common.sh"

STEP_ID="04_validate"
if step_should_skip "${STEP_ID}"; then
  announce_step "${STEP_ID} (already done, skipping)"
  exit 0
fi

announce_step "04 validate"
kresko status -d "${EXPERIMENT_DIR}" --summary
if [[ "${STATUS_DEEP_ON_VALIDATE}" == "1" ]]; then
  kresko status -d "${EXPERIMENT_DIR}" --deep
fi

mark_step_done "${STEP_ID}"
"#
    .to_string()
}

fn render_step_run_experiment() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../common.sh"

STEP_ID="05_run_experiment"
if step_should_skip "${STEP_ID}"; then
  announce_step "${STEP_ID} (already done, skipping)"
  exit 0
fi

announce_step "05 run experiment"

shopt -s nullglob
run_manifests=("${EXPERIMENT_DIR}"/runs/*.env)

if [[ "${#run_manifests[@]}" -eq 0 ]]; then
  cat >&2 <<'EOF'
No run manifests found under runs/*.env.
Create one or more run manifest files to define the campaign sequence.
EOF
  exit 1
fi

for manifest_path in "${run_manifests[@]}"; do
  run_file="$(basename "${manifest_path}")"
  run_id="${run_file%.env}"

  unset RUN_NAME RUN_SCRIPT RUN_COMMAND RUN_PRE_COMMAND RUN_POST_COMMAND
  unset RUN_DATA_SUBDIR RUN_SKIP_COLLECT

  set -a
  # shellcheck disable=SC1090
  source "${manifest_path}"
  set +a

  if [[ -z "${RUN_NAME:-}" ]]; then
    echo "run manifest ${manifest_path} is missing RUN_NAME" >&2
    exit 1
  fi
  if [[ -z "${RUN_SCRIPT:-}" && -z "${RUN_COMMAND:-}" ]]; then
    echo "run manifest ${manifest_path} must set RUN_SCRIPT or RUN_COMMAND" >&2
    exit 1
  fi

  run_step_id="05_run_${run_id}"
  if step_should_skip "${run_step_id}"; then
    announce_step "${run_step_id} (already done, skipping)"
    continue
  fi

  run_data_subdir="${RUN_DATA_SUBDIR:-${RUN_NAME}}"
  run_data_dir="${EXPERIMENT_DIR}/data/${run_data_subdir}"
  mkdir -p "${run_data_dir}"
  cp "${manifest_path}" "${run_data_dir}/manifest.env"

  announce_step "05 run ${RUN_NAME}"
  kresko status -d "${EXPERIMENT_DIR}" --summary | tee "${run_data_dir}/status.before.txt"
  kresko status --json -d "${EXPERIMENT_DIR}" > "${run_data_dir}/status.before.json"

  if [[ -n "${RUN_PRE_COMMAND:-}" ]]; then
    bash -lc "${RUN_PRE_COMMAND}"
  fi

  if [[ -n "${RUN_SCRIPT:-}" ]]; then
    run_script_path="${RUN_SCRIPT}"
    if [[ "${run_script_path}" != /* ]]; then
      run_script_path="${EXPERIMENT_DIR}/${run_script_path}"
    fi
    bash "${run_script_path}"
  else
    bash -lc "${RUN_COMMAND}"
  fi

  kresko status -d "${EXPERIMENT_DIR}" --summary | tee "${run_data_dir}/status.after.txt"
  kresko status --json -d "${EXPERIMENT_DIR}" > "${run_data_dir}/status.after.json"

  if [[ "${RUN_SKIP_COLLECT:-0}" != "1" ]]; then
    kresko collect -d "${EXPERIMENT_DIR}" --data-subdir "${run_data_subdir}"
  fi

  if [[ -n "${RUN_POST_COMMAND:-}" ]]; then
    bash -lc "${RUN_POST_COMMAND}"
  fi

  mark_step_done "${run_step_id}"
done

mark_step_done "${STEP_ID}"
"#
    .to_string()
}

fn render_step_collect_artifacts() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../common.sh"

STEP_ID="06_collect_artifacts"
if step_should_skip "${STEP_ID}"; then
  announce_step "${STEP_ID} (already done, skipping)"
  exit 0
fi

announce_step "06 collect artifacts"
args=(-d "${EXPERIMENT_DIR}")
if [[ -n "${DATA_SUBDIR}" ]]; then
  args+=(--data-subdir "${DATA_SUBDIR}")
fi

kresko collect "${args[@]}"
mark_step_done "${STEP_ID}"
"#
    .to_string()
}

fn render_run_bounded_pow_script() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

require_command jq

RUN_NAME="${RUN_NAME:-pow-bounded-40}"
RUN_DATA_SUBDIR="${RUN_DATA_SUBDIR:-${RUN_NAME}}"
TARGET_BLOCK_DELTA="${TARGET_BLOCK_DELTA:-40}"
RUN_TIMEOUT_SECS="${RUN_TIMEOUT_SECS:-3600}"
POLL_SECS="${POLL_SECS:-120}"
MINER_INSTANCES="${MINER_INSTANCES:-all}"
REQUIRE_FULL_REACHABILITY="${REQUIRE_FULL_REACHABILITY:-1}"
MIN_REACHABLE_NODES="${MIN_REACHABLE_NODES:-}"
DEEP_STATUS_ON_FAILURE="${DEEP_STATUS_ON_FAILURE:-1}"
STOP_MINERS_ON_EXIT="${STOP_MINERS_ON_EXIT:-1}"

run_data_dir="${EXPERIMENT_DIR}/data/${RUN_DATA_SUBDIR}"
progress_log="${run_data_dir}/progress.log.jsonl"
status_json_path="${run_data_dir}/status.latest.json"
mkdir -p "${run_data_dir}"

cleanup() {
  if [[ "${STOP_MINERS_ON_EXIT}" == "1" ]]; then
    kresko kill-session -d "${EXPERIMENT_DIR}" --session mine >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

capture_failure_status() {
  kresko status -d "${EXPERIMENT_DIR}" --summary | tee "${run_data_dir}/status.failure.txt" || true
  kresko status --json -d "${EXPERIMENT_DIR}" > "${run_data_dir}/status.failure.json" || true
  if [[ "${DEEP_STATUS_ON_FAILURE}" == "1" ]]; then
    kresko status -d "${EXPERIMENT_DIR}" --deep > "${run_data_dir}/status.failure.deep.txt" || true
  fi
}

announce_step "bounded pow: restart mining sessions"
kresko kill-session -d "${EXPERIMENT_DIR}" --session mine >/dev/null 2>&1 || true
kresko start-miners -d "${EXPERIMENT_DIR}" -i "${MINER_INSTANCES}"

start_height=""
target_height=""
deadline_epoch="$(( $(date +%s) + RUN_TIMEOUT_SECS ))"

while true; do
  now_epoch="$(date +%s)"
  if (( now_epoch >= deadline_epoch )); then
    echo "timed out waiting for ${TARGET_BLOCK_DELTA} post-start blocks" >&2
    capture_failure_status
    exit 1
  fi

  if ! status_json="$(kresko status --json -d "${EXPERIMENT_DIR}")"; then
    echo "failed to query kresko status during bounded PoW run" >&2
    capture_failure_status
    exit 1
  fi
  printf '%s\n' "${status_json}" > "${status_json_path}"

  timestamp="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
  metrics="$(printf '%s\n' "${status_json}" | jq -c --arg ts "${timestamp}" '
    def heights: [.nodes[].height | select(. != null)];
    {
      ts: $ts,
      total: .total,
      reachable: .reachable,
      unreachable: .unreachable,
      min_height: (if (heights | length) == 0 then null else (heights | min) end),
      max_height: (if (heights | length) == 0 then null else (heights | max) end),
      spread: (if (heights | length) == 0 then null else ((heights | max) - (heights | min)) end),
      nodes: [.nodes[] | {name, height, status}]
    }
  ')"

  reachable_count="$(printf '%s\n' "${metrics}" | jq -r '.reachable')"
  total_count="$(printf '%s\n' "${metrics}" | jq -r '.total')"
  current_max="$(printf '%s\n' "${metrics}" | jq -r '.max_height // empty')"

  baseline_ready=0
  if [[ -n "${current_max}" ]]; then
    if [[ -n "${MIN_REACHABLE_NODES}" ]]; then
      if (( reachable_count >= MIN_REACHABLE_NODES )); then
        baseline_ready=1
      fi
    elif [[ "${REQUIRE_FULL_REACHABILITY}" == "1" ]]; then
      if (( reachable_count == total_count )); then
        baseline_ready=1
      fi
    else
      baseline_ready=1
    fi
  fi

  if [[ -z "${start_height}" && "${baseline_ready}" == "1" ]]; then
    start_height="${current_max}"
    target_height="$(( start_height + TARGET_BLOCK_DELTA ))"
  fi

  entry="$(
    printf '%s\n' "${metrics}" | jq -c \
      --argjson start_height "${start_height:-null}" \
      --argjson target_height "${target_height:-null}" \
      '. + {start_height: $start_height, target_height: $target_height}'
  )"
  printf '%s\n' "${entry}" >> "${progress_log}"

  if [[ -n "${target_height}" && -n "${current_max}" ]] && (( current_max >= target_height )); then
    break
  fi

  sleep "${POLL_SECS}"
done

announce_step "bounded pow target reached"
"#
    .to_string()
}

fn render_run_bounded_generate_script() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

require_command jq

RUN_NAME="${RUN_NAME:-generate-bounded-40}"
RUN_DATA_SUBDIR="${RUN_DATA_SUBDIR:-${RUN_NAME}}"
TARGET_BLOCK_DELTA="${TARGET_BLOCK_DELTA:-40}"
RUN_TIMEOUT_SECS="${RUN_TIMEOUT_SECS:-1800}"
POLL_SECS="${POLL_SECS:-30}"
GENERATE_BLOCK_TIME_SECS="${GENERATE_BLOCK_TIME_SECS:-10}"
PROGRESS_RANDOM="${PROGRESS_RANDOM:-0}"
PROGRESS_CONCURRENT="${PROGRESS_CONCURRENT:-1}"
DEEP_STATUS_ON_FAILURE="${DEEP_STATUS_ON_FAILURE:-1}"

run_data_dir="${EXPERIMENT_DIR}/data/${RUN_DATA_SUBDIR}"
status_json_path="${run_data_dir}/status.latest.json"
mkdir -p "${run_data_dir}"

progress_pid=""
cleanup() {
  if [[ -n "${progress_pid}" ]]; then
    kill "${progress_pid}" >/dev/null 2>&1 || true
    wait "${progress_pid}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

capture_failure_status() {
  kresko status -d "${EXPERIMENT_DIR}" --summary | tee "${run_data_dir}/status.failure.txt" || true
  kresko status --json -d "${EXPERIMENT_DIR}" > "${run_data_dir}/status.failure.json" || true
  if [[ "${DEEP_STATUS_ON_FAILURE}" == "1" ]]; then
    kresko status -d "${EXPERIMENT_DIR}" --deep > "${run_data_dir}/status.failure.deep.txt" || true
  fi
}

progress_args=(
  -d "${EXPERIMENT_DIR}"
  --block-time "${GENERATE_BLOCK_TIME_SECS}"
  --data-subdir "${RUN_DATA_SUBDIR}"
  --concurrent "${PROGRESS_CONCURRENT}"
)
if [[ "${PROGRESS_RANDOM}" == "1" ]]; then
  progress_args+=(--random)
fi

announce_step "bounded generate: start progress driver"
kresko progress "${progress_args[@]}" &
progress_pid="$!"

start_height=""
target_height=""
deadline_epoch="$(( $(date +%s) + RUN_TIMEOUT_SECS ))"

while true; do
  now_epoch="$(date +%s)"
  if (( now_epoch >= deadline_epoch )); then
    echo "timed out waiting for ${TARGET_BLOCK_DELTA} generated blocks" >&2
    capture_failure_status
    exit 1
  fi

  if ! status_json="$(kresko status --json -d "${EXPERIMENT_DIR}")"; then
    echo "failed to query kresko status during generate run" >&2
    capture_failure_status
    exit 1
  fi
  printf '%s\n' "${status_json}" > "${status_json_path}"

  current_max="$(printf '%s\n' "${status_json}" | jq -r '.nodes[].height | select(. != null)' | sort -n | tail -1)"
  if [[ -n "${current_max}" ]]; then
    if [[ -z "${start_height}" ]]; then
      start_height="${current_max}"
      target_height="$(( start_height + TARGET_BLOCK_DELTA ))"
    elif (( current_max >= target_height )); then
      break
    fi
  fi

  sleep "${POLL_SECS}"
done

announce_step "bounded generate target reached"
"#
    .to_string()
}

fn render_run_txblast_sample_script() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/common.sh"

require_command jq

RUN_NAME="${RUN_NAME:-txblast-shielded-sample}"
RUN_DATA_SUBDIR="${RUN_DATA_SUBDIR:-${RUN_NAME}}"
TXBLAST_INSTANCES="${TXBLAST_INSTANCES:-all}"
TXBLAST_RATE="${TXBLAST_RATE:-25}"
TXBLAST_AMOUNT="${TXBLAST_AMOUNT:-0.001}"
TXBLAST_DURATION_SECS="${TXBLAST_DURATION_SECS:-300}"
TXBLAST_STATUS_POLL_SECS="${TXBLAST_STATUS_POLL_SECS:-30}"
TXBLAST_TRACE_ENABLE="${TXBLAST_TRACE_ENABLE:-1}"
TXBLAST_TRACE_DIR="${TXBLAST_TRACE_DIR:-/root/.cache/kresko/txblast-traces}"
TXBLAST_STALL_SECS="${TXBLAST_STALL_SECS:-120}"
TXBLAST_REQUIRE_READY_NODES="${TXBLAST_REQUIRE_READY_NODES:-0}"
TXBLAST_ORCHARD_PROGRESS_INTERVAL_SECS="${TXBLAST_ORCHARD_PROGRESS_INTERVAL_SECS:-5}"

run_data_dir="${EXPERIMENT_DIR}/data/${RUN_DATA_SUBDIR}"
status_json_path="${run_data_dir}/txblast-status.latest.json"
status_log_path="${run_data_dir}/txblast-status.log.jsonl"
mkdir -p "${run_data_dir}"

cleanup() {
  kresko kill-session -d "${EXPERIMENT_DIR}" --session txblast >/dev/null 2>&1 || true
}
trap cleanup EXIT

announce_step "txblast sample: clear previous txblast session"
kresko kill-session -d "${EXPERIMENT_DIR}" --session txblast >/dev/null 2>&1 || true

announce_step "txblast sample: clear stale txblast traces"
kresko exec -d "${EXPERIMENT_DIR}" -w 4 -c "rm -f '${TXBLAST_TRACE_DIR}'/txblast_*.jsonl 2>/dev/null || true"

txblast_args=(
  -d "${EXPERIMENT_DIR}"
  -i "${TXBLAST_INSTANCES}"
  --rate "${TXBLAST_RATE}"
  --amount "${TXBLAST_AMOUNT}"
  --orchard-progress-interval-secs "${TXBLAST_ORCHARD_PROGRESS_INTERVAL_SECS}"
)
if [[ "${TXBLAST_TRACE_ENABLE}" == "1" ]]; then
  txblast_args+=(--trace-enable --trace-dir "${TXBLAST_TRACE_DIR}")
fi

announce_step "txblast sample: start txblast"
kresko txblast "${txblast_args[@]}"

deadline_epoch="$(( $(date +%s) + TXBLAST_DURATION_SECS ))"
max_ready_nodes=0

while true; do
  now_epoch="$(date +%s)"
  if (( now_epoch >= deadline_epoch )); then
    break
  fi

  if status_json="$(kresko txblast-status --json -d "${EXPERIMENT_DIR}" -i "${TXBLAST_INSTANCES}" --trace-dir "${TXBLAST_TRACE_DIR}" --stall-secs "${TXBLAST_STALL_SECS}")"; then
    printf '%s\n' "${status_json}" > "${status_json_path}"
    timestamp="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
    printf '%s\n' "${status_json}" | jq -c --arg ts "${timestamp}" '. + {ts: $ts}' >> "${status_log_path}"
    ready_nodes="$(printf '%s\n' "${status_json}" | jq -r '.ready_nodes')"
    if (( ready_nodes > max_ready_nodes )); then
      max_ready_nodes="${ready_nodes}"
    fi
  fi

  sleep "${TXBLAST_STATUS_POLL_SECS}"
done

if (( TXBLAST_REQUIRE_READY_NODES > 0 && max_ready_nodes < TXBLAST_REQUIRE_READY_NODES )); then
  echo "txblast never reached required ready node count: max=${max_ready_nodes} required=${TXBLAST_REQUIRE_READY_NODES}" >&2
  exit 1
fi

announce_step "txblast sample duration reached"
"#
    .to_string()
}

fn render_step_teardown() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/../common.sh"

STEP_ID="07_teardown"
if step_should_skip "${STEP_ID}"; then
  announce_step "${STEP_ID} (already done, skipping)"
  exit 0
fi

announce_step "07 teardown"
if [[ "${SKIP_DOWN}" == "1" ]]; then
  echo "SKIP_DOWN=1; leaving infrastructure running"
  mark_step_done "${STEP_ID}"
  exit 0
fi

kresko down -d "${EXPERIMENT_DIR}"
mark_step_done "${STEP_ID}"
"#
    .to_string()
}

fn render_collect_artifacts_script() -> String {
    r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"${SCRIPT_DIR}/steps/06_collect_artifacts.sh"
"#
    .to_string()
}

fn discover_shared_env_path(dir: &Path) -> Result<Option<PathBuf>> {
    let parent = if dir.is_absolute() {
        dir.parent().map(Path::to_path_buf)
    } else {
        let cwd = std::env::current_dir().context("failed to determine current directory")?;
        cwd.join(dir).parent().map(Path::to_path_buf)
    }
    .unwrap_or_else(|| PathBuf::from("."));

    for candidate in env_candidates(&parent) {
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }

    Ok(None)
}

fn env_candidates(parent: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![parent.join("env"), parent.join(".env")];
    if let Some(grandparent) = parent.parent() {
        candidates.push(grandparent.join("env"));
        candidates.push(grandparent.join(".env"));
    }
    candidates
}

fn read_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let iter = dotenvy::from_path_iter(path)
        .with_context(|| format!("failed to read env file {}", path.display()))?;
    let mut values = BTreeMap::new();
    for item in iter {
        let (key, value) =
            item.with_context(|| format!("failed to parse env file {}", path.display()))?;
        values.insert(key, value);
    }
    Ok(values)
}

fn resolve_env_entry(key: &str, default: &str, shared_env: &BTreeMap<String, String>) -> String {
    if let Some(value) = shared_env.get(key).filter(|value| !value.is_empty()) {
        return value.clone();
    }
    if let Ok(value) = std::env::var(key) {
        if !value.is_empty() {
            return value;
        }
    }
    default.to_string()
}

fn resolve_env_source(
    key: &str,
    default: &str,
    shared_env: &BTreeMap<String, String>,
    shared_source: Option<&str>,
) -> String {
    if shared_env.get(key).is_some_and(|value| !value.is_empty()) {
        return shared_source
            .map(|path| format!("copied from {path}"))
            .unwrap_or_else(|| "copied from shared env file".to_string());
    }
    if std::env::var(key)
        .ok()
        .is_some_and(|value| !value.is_empty())
    {
        return "copied from current shell environment".to_string();
    }
    if default.is_empty() {
        "left blank".to_string()
    } else {
        format!("defaulted to {default}")
    }
}

fn source_for_value(
    key: &str,
    value: &str,
    shared_env: &BTreeMap<String, String>,
    shared_source: Option<&str>,
    fallback: &str,
) -> String {
    if shared_env
        .get(key)
        .is_some_and(|candidate| candidate == value)
    {
        return shared_source
            .map(|path| format!("copied from {path}"))
            .unwrap_or_else(|| "copied from shared env file".to_string());
    }
    if std::env::var(key)
        .ok()
        .is_some_and(|candidate| candidate == value)
    {
        return "copied from current shell environment".to_string();
    }
    fallback.to_string()
}

fn format_env_value(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }

    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':' | '@'))
    {
        value.to_string()
    } else {
        format!("{value:?}")
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

const NODE_INIT_SH: &str = include_str!("../../scripts/node_init.sh");

fn default_ssh_key_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

const VARS_SH_TEMPLATE: &str = r#"#!/bin/bash
# Environment variables for kresko nodes
# This file is generated by kresko genesis

export CHAIN_ID=""
export AWS_ACCESS_KEY_ID=""
export AWS_SECRET_ACCESS_KEY=""
export AWS_DEFAULT_REGION=""
export AWS_S3_BUCKET=""
export AWS_S3_ENDPOINT=""
"#;

const FLUPS_TEMPLATE: &str = r#"# Flups

Record campaign-specific issues here as they happen. Tag each entry with one of:
- `tool-gap`
- `runbook-gap`
- `external`

## Entries
"#;

const RUN_BOUNDED_POW_MANIFEST_TEMPLATE: &str = r#"# Ordered run manifest for scripts/steps/05_run_experiment.sh
#
# This default sample runs a bounded PoW workload, waits for the network tip
# to advance by TARGET_BLOCK_DELTA blocks, and leaves artifact download to the
# default per-run collector in step 05.

RUN_NAME="pow-bounded-40"
RUN_SCRIPT="scripts/run_bounded_pow.sh"
RUN_DATA_SUBDIR="${RUN_NAME}"

# Stop once the network tip advances by this many blocks beyond the post-start baseline.
TARGET_BLOCK_DELTA=40

# Poll kresko status at this cadence while waiting.
POLL_SECS=120

# Fail the run if the target is not reached before this timeout.
RUN_TIMEOUT_SECS=3600

# Start miners on this instance set.
MINER_INSTANCES="all"

# Baseline selection policy. Leave REQUIRE_FULL_REACHABILITY=1 for the strict default,
# or set MIN_REACHABLE_NODES to a lower threshold if partial cluster operation is acceptable.
REQUIRE_FULL_REACHABILITY=1
# MIN_REACHABLE_NODES=2

# Capture deep status snapshots when the run fails or times out.
DEEP_STATUS_ON_FAILURE=1

# The run script already writes progress.log.jsonl and status.latest.json;
# keep artifact collection enabled so logs/heights/traces are downloaded after the run.
RUN_SKIP_COLLECT=0
"#;

const RUN_BOUNDED_GENERATE_MANIFEST_TEMPLATE: &str = r#"# Example run manifest for mining_mode=generate experiments.
#
# Copy this file into runs/ (for example runs/02_bounded_generate.env) to activate it.

RUN_NAME="generate-bounded-40"
RUN_SCRIPT="scripts/run_bounded_generate.sh"
RUN_DATA_SUBDIR="${RUN_NAME}"

TARGET_BLOCK_DELTA=40
GENERATE_BLOCK_TIME_SECS=10
POLL_SECS=30
RUN_TIMEOUT_SECS=1800

# Set PROGRESS_RANDOM=1 to pick miners randomly instead of round-robin.
PROGRESS_RANDOM=0
PROGRESS_CONCURRENT=1

DEEP_STATUS_ON_FAILURE=1
RUN_SKIP_COLLECT=0
"#;

const RUN_TXBLAST_MANIFEST_TEMPLATE: &str = r#"# Example run manifest for a txblast sample workload.
#
# Copy this file into runs/ (for example runs/02_txblast_shielded.env) to activate it.

RUN_NAME="txblast-shielded-sample"
RUN_SCRIPT="scripts/run_txblast_sample.sh"
RUN_DATA_SUBDIR="${RUN_NAME}"

TXBLAST_INSTANCES="all"
TXBLAST_RATE=25
TXBLAST_AMOUNT=0.001
TXBLAST_DURATION_SECS=300
TXBLAST_STATUS_POLL_SECS=30
TXBLAST_TRACE_ENABLE=1
TXBLAST_TRACE_DIR="/root/.cache/kresko/txblast-traces"
TXBLAST_STALL_SECS=120

# Optional gate: fail if txblast never reaches this many ready nodes.
# TXBLAST_REQUIRE_READY_NODES=2
TXBLAST_REQUIRE_READY_NODES=0

RUN_SKIP_COLLECT=0
"#;

const START_CAMPAIGN_COMPAT_SH: &str = r#"#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"${SCRIPT_DIR}/init.sh" "$@"
"#;
