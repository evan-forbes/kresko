# Kresko

Kresko is an experimental Zcash bench for spinning up arbitrary numbers of geographically distributed nodes, with a strong focus on being easy to debug for non-DevOps developers.

## Why Kresko

- Fast iteration on multi-node Zcash experiments.
- Region-aware node placement across cloud providers.
- Debug-first runtime model (tmux-managed sessions, easy log retrieval, diagnostic scripts).
- Local genesis generation and per-node config generation for repeatable test networks.

## Current Scope

- Node role: `miner`
- Providers: `digitalocean`, `googlecloud`
- RPC-focused workflows: chain progress, status checks, transaction blasting, height trace collection
- Data export: local `data/` plus optional S3 upload

## How It Works

Typical flow:

1. `init` creates an experiment directory with config, scripts, and `.env`.
2. `add` defines miners (count + region/provider).
3. `up` creates cloud instances and records their IPs in `config.json`.
4. `genesis` builds payload content (local genesis artifacts, per-node `zebrad.toml`, binaries).
5. `deploy` ships payload and starts nodes via tmux session `app`.
6. `status` / `progress` / `txblast` drive and observe network behavior.
7. `download` / `download heights` / `upload-data` collect artifacts.
8. `reset` / `down` clean up sessions, state, and instances.

## Prerequisites

- Rust toolchain with `cargo`
- Local binaries/tools:
  - `ssh`, `scp`, `tar`, `curl`, `bash`
  - `openssl` (required for Google Cloud auth flow)
- A built `zebrad` binary path for `kresko genesis --zebrad-binary ...`
- Cloud credentials:
  - DigitalOcean: `DIGITALOCEAN_TOKEN`
  - Google Cloud: `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_KEY_JSON_PATH`
- SSH key pair available for instance access

## Install

```bash
cargo build --release
```

Optional:

```bash
make install
```

## Quick Start (DigitalOcean)

```bash
# 1) Create experiment
./target/release/kresko init \
  --chain-id nu6-lab \
  --experiment exp-nyc-sfo \
  --provider digitalocean

cd exp-nyc-sfo

# 2) Fill credentials in .env
# Required: DIGITALOCEAN_TOKEN
# Required for default deploy path: AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_S3_BUCKET
# (or use --direct-payload-upload on deploy to skip S3 payload distribution)

# 3) Define miners (random regions)
../target/release/kresko add --node-type miner --count 8 --region random

# 4) Create cloud instances
../target/release/kresko up --workers 8

# 5) Build payload (point to your local zebrad binary)
../target/release/kresko genesis --zebrad-binary /path/to/zebrad

# 6) Deploy and start remote nodes (tmux session: app)
../target/release/kresko deploy --workers 8

# 7) Check RPC health/sync
../target/release/kresko status
```

## Experiment Directory Layout

Created by `kresko init`:

```text
<experiment>/
  .env
  config.json
  zebrad.toml
  payload/
  data/
  scripts/
```

Generated later:

- `payload/local_genesis/*` (genesis artifacts, checkpoints, funded keys)
- `payload/<node>/zebrad.toml` (per-node peer config + local testnet params)
- `payload/build/zebrad` and optional `payload/build/kresko` (txblast-local runner)
- `payload.tar.gz` (cached payload archive)
- `progress.log.jsonl` (from `kresko progress`)

## Debugging Workflow (Non-DevOps Friendly)

Kresko is designed so you can debug with simple SSH + logs:

- Remote node app runs in tmux session `app`
- Tx blaster runs in tmux session `txblast`
- Remote logs:
  - `/root/logs`
  - `/root/kresko-app.log`
  - `/root/kresko-txblast.log`

Useful commands:

```bash
# Kill a session across active nodes
kresko kill-session --session app
kresko kill-session --session txblast

# Download logs from all nodes into ./data/
kresko download --nodes all --workers 8

# Download block height/time/size traces
kresko download heights --nodes 3 --workers 4
```

`scripts/network_diag.sh` is included for per-node RPC/network checks and can be run directly on a node.

## Command Reference

- `init`: bootstrap experiment directory and provider-specific `.env`
- `add`: append miner definitions to config (`--region random` supported)
- `up`: create instances in provider
- `list`: list running kresko instances in provider
- `genesis`: generate local genesis + payload
- `deploy`: distribute payload and start nodes (`--direct-payload-upload` skips S3 payload hop)
- `status`: query node RPC status/height/sync
- `progress`: continuously call `generate` on miners
- `txblast`: start remote tx blast (`transparent` currently supported with zebrad-compatible mode)
- `txblast-local`: local tx blast runner intended for remote execution
- `download`: fetch logs from nodes
- `download heights`: collect per-block traces via RPC into JSONL
- `upload-data`: upload collected `data/` to S3 prefix `<experiment>/data/`
- `reset`: stop sessions and clean remote node state
- `down`: destroy instances for this experiment
- `down --all`: destroy all kresko-tagged instances across configured providers

## Notes and Caveats

- Experimental project: interfaces and behavior may change.
- `down --all` is intentionally destructive. Use carefully.
- Provider credentials are loaded from `.env` (current directory first, then experiment directory).
- `workers` values must be greater than `0`.

## License

No license file is currently included in this repository.
