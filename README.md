# Kresko

Kresko is an experimental Zcash bench for spinning up arbitrary numbers of geographically distributed nodes, with a strong focus on being easy to debug for non-DevOps developers.

## Why Kresko

- Fast iteration on multi-node Zcash experiments.
- Region-aware node placement across cloud providers.
- Debug-first runtime model (tmux-managed sessions, easy log retrieval, diagnostic scripts).
- Local genesis generation and per-node config generation for repeatable test networks.

## Current Scope

- Node role: `miner`
- Providers: `digitalocean`, `googlecloud`, `linode`
- RPC-focused workflows: chain progress, status checks, transaction blasting, height trace collection
- Data export: local `data/` plus optional S3 upload

## How It Works

Typical flow:

1. `init` creates an experiment directory with config, scripts, and `.env`.
2. `add` defines miners (count + region/provider).
3. `up` creates cloud instances and records their IPs in `config.json`.
4. `sync-ips` reconstructs missing IPs in `config.json` from provider state when needed.
5. `genesis` builds payload content (local genesis artifacts, per-node `zebrad.toml`, binaries).
6. `deploy` ships payload and starts nodes via tmux session `app`.
7. `status` / `progress` / `txblast` drive and observe network behavior.
8. `download` / `download heights` / `upload-data` collect artifacts.
9. `reset` / `down` clean up sessions, state, and instances.

## Prerequisites

- Rust toolchain with `cargo`
- Local binaries/tools:
  - `ssh`, `scp`, `tar`, `curl`, `bash`
  - `openssl` (required for Google Cloud auth flow)
- A built `zebrad` binary path for `kresko genesis --zebrad-binary ...`
- Cloud credentials:
  - DigitalOcean: `DIGITALOCEAN_TOKEN`
  - Google Cloud: `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_KEY_JSON_PATH`
  - Linode: `LINODE_TOKEN`
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
# Required: AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_S3_BUCKET
# Optional: AWS_S3_ENDPOINT for Spaces-compatible providers
# Payload distribution always goes through S3 — nodes curl the tarball
# from a presigned URL. The deploy path retries S3 uploads and falls back
# to `aws s3 cp` if the AWS CLI is installed.

# 3) Define miners (random regions)
../target/release/kresko add --node-type miner --count 8 --region random

# Optional: add a smaller proof node without memorizing provider-specific slugs
../target/release/kresko add --node-type miner --count 1 --region random --low-resource

# 4) Create cloud instances
../target/release/kresko up -w 16

# 5) Build payload (point to your local zebrad binary)
../target/release/kresko genesis \
  --zebrad-binary /path/to/zebrad \
  --orchard-lanes-per-miner 384 \
  --orchard-lane-value-zats 100000 \
  --orchard-fanout-source-value-zats 500000 \
  --orchard-fanout-outputs 4

# 6) Deploy and start remote nodes (tmux session: app)
../target/release/kresko deploy -w 16

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
- `payload/build/zebrad` and `payload/build/kresko` (single remote runner for `txblast-local` and `mine`)
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
kresko download -n all -w 16

# Download only peer_message structured traces
kresko download -n all -w 16 traces -t peer_message

# Download every file from each node's discovered trace directories
kresko download -n all -w 16 traces

# Download a canonical block height/time/size trace from selected miners,
# using up to 16 concurrent RPC block fetches, 16-height batches,
# async tip probing, failover between miners, and resume from any
# existing data/heights.jsonl unless -f/--force is set
kresko download -n 0,1,2 -w 16 heights -b 16
```

`scripts/network_diag.sh` is included for per-node RPC/network checks and can be run directly on a node.

Structured trace table reference:

- [`docs/trace-tables.md`](docs/trace-tables.md)

## Command Reference

- `init`: bootstrap experiment directory and provider-specific `.env`
- `add`: append miner definitions to config (`--region random` and `--low-resource` supported)
- `up`: create instances across the providers referenced by the experiment config
- `sync-ips`: repopulate missing `public_ip` / `private_ip` fields in `config.json` from cloud provider state
- `list`: list running kresko instances across the providers referenced by the experiment config
- `genesis`: generate local genesis + payload
- `deploy`: distribute payload via S3 (operator uploads to S3, nodes curl a presigned URL) and start nodes.
  The S3 upload path retries and can fall back to the `aws` CLI. Override with:
  `KRESKO_S3_UPLOAD_ATTEMPTS`, `KRESKO_S3_UPLOAD_RETRY_DELAY_SECS`, and `KRESKO_S3_UPLOAD_AWS_CLI_FALLBACK=0`.
- `status`: query node RPC status/height/sync
- `progress`: continuously call `generate` on miners
- `txblast`: start remote tx blast (`transparent`, `shielded`, or `both`; shielded mode supports Orchard lane and fanout controls)
- `txblast-local`: local tx blast runner intended for remote execution
- `download`: fetch logs from nodes
- `download traces`: fetch every file from discovered remote trace directories by default, or a selected trace-table subset via `--tables`
- `download heights`: collect one canonical per-block RPC trace into JSONL, with async tip probing, retry/fallback across selected nodes, and reuse of existing heights unless `--force` is set
- `upload-data`: upload collected `data/` to S3 prefix `<experiment>/data/`
- `reset`: stop sessions and clean remote node state
- `down`: destroy instances for this experiment
- `down --all`: destroy all kresko-tagged/grouped instances across configured providers

## Notes and Caveats

- Experimental project: interfaces and behavior may change.
- `down --all` is intentionally destructive. Use carefully.
- Provider credentials are loaded from `.env` (current directory first, then experiment directory).
- `workers` values must be greater than `0`.
- Shielded txblast now uses the local-genesis premine UTXO to create an initial Orchard lane inventory, then replenishes width with fanout when ready lanes fall below the configured watermark.

## License

No license file is currently included in this repository.
