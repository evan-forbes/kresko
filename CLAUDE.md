# Kresko

Kresko deploys distributed Zcash mining networks on cloud infrastructure for benchmarking experiments. It manages the full lifecycle: provisioning VMs, generating genesis blocks, deploying nodes, driving block production, and collecting data.

## Build

```bash
cd kresko && cargo build --release
# Binary: target/release/kresko
```

## Experiment Workflow

Every experiment follows this sequence. Run all kresko commands from inside the experiment directory (or pass `-d <dir>`).

### 1. Initialize

```bash
kresko init --chain-id <chain-id> --experiment <name> --provider digitalocean
cd <name>
```

For PoW mining experiments (real Equihash solving, difficulty adjustment):
```bash
kresko init --chain-id <chain-id> --experiment <name> --provider digitalocean --mining-mode pow --block-time 30
```

`--mining-mode`: `generate` (default, PoW disabled, uses `generate` RPC) or `pow` (real PoW mining via `getblocktemplate` + Equihash solver).
`--block-time`: Target block time in seconds (default: 75, post-Blossom). In generate mode, controls `kresko progress` interval. In PoW mode, feeds into difficulty adjustment target.

Edit `.env` with credentials. Required vars depend on provider:
- DigitalOcean: `DIGITALOCEAN_TOKEN`
- Google Cloud: `GOOGLE_CLOUD_PROJECT`, `GOOGLE_CLOUD_KEY_JSON_PATH`
- S3 (for deploy without `--direct-payload-upload`): `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_S3_BUCKET`

SSH keys: `KRESKO_SSH_KEY_PATH`, `KRESKO_SSH_PUB_KEY_PATH`, `KRESKO_SSH_KEY_NAME` (defaults to `~/.ssh/id_rsa`).

### 2. Add Nodes

```bash
kresko add --node-type miner --count 8 --region random
```

Regions for DigitalOcean: `nyc1, nyc3, tor1, sfo2, sfo3, ams3, sgp1, lon1, fra1, syd1`.
Regions for Google Cloud: `us-central1, us-east1, us-east4, asia-southeast1, europe-west1, asia-east1`.
Use `--region random` for geographic distribution.

### 3. Provision VMs

```bash
kresko up --workers 8
```

Creates cloud instances, records IPs in `config.json`. Default droplet: `s-8vcpu-16gb` (DO), `c3d-highcpu-16` (GCP).

### 4. Generate Genesis + Payload

```bash
kresko genesis --zebrad-binary /path/to/zebrad
```

Requires a pre-built `zebrad` binary. This generates local genesis artifacts, per-node `zebrad.toml` configs with peer addresses and funded mining keys, and packages everything into `payload/`. Optionally include a txblast binary with `--txblast-binary`.

### 5. Deploy

```bash
kresko deploy --workers 8 --direct-payload-upload
```

Uploads `payload.tar.gz` to each node via SCP (or S3 if `--direct-payload-upload` is omitted), runs `node_init.sh` which installs deps, extracts payload, auto-generates miner addresses, seeds genesis blocks, and starts `zebrad` in tmux session `app`.

Use `--ignore-failed-miners` to continue past partial failures.

### 6. Monitor

```bash
# One-shot status check: block height and sync progress per node
kresko status
kresko status --json   # Machine-readable JSON output

# Health check: all nodes reachable and advancing? Exits 1 if unhealthy.
kresko check
kresko check --json    # Machine-readable JSON with issues list

# Continuous block generation loop (Ctrl-C to stop)
kresko progress --block-time 10 --concurrent 2
# Logs to progress.log.jsonl (JSONL with ts_unix_ms, tick, miner, ip, ok, latency_ms, block_hash, error)

# Start transaction blaster on remote nodes
kresko txblast --instances all --tx-type transparent --rate 10 --amount 0.001
# Runs in tmux session `txblast` on each node

# PoW mode: start miners on remote nodes (replaces `kresko progress` as block driver)
kresko start-miners --instances all
# Runs `kresko mine` in tmux session `mine` on each node

# PoW mode: run miner locally (intended for remote nodes, not direct use)
kresko mine --rpc-endpoint http://localhost:18232
```

`kresko status --json` returns a JSON object with `nodes` (array of `{name, ip, height, verification_progress, status}`), `total`, `reachable`, `unreachable`.

`kresko check --json` returns a JSON object with `healthy` (bool), `total_nodes`, `reachable_nodes`, `unreachable_nodes`, `min_height`, `max_height`, `height_spread`, `all_synced`, and `issues` (array of strings describing problems). It detects unreachable nodes, nodes stuck at height 0, and nodes falling >10 blocks behind the leader. Exit code is 1 when unhealthy, 0 when healthy.

`kresko progress` is a blocking foreground process. Run it in a background terminal or tmux session. It writes structured JSONL that is the primary data source for block production analysis. In PoW mode, it automatically switches to observer mode (polls `getblockchaininfo` instead of calling `generate`).

### 7. Collect Data

```bash
# Download node logs
kresko download --nodes all --workers 8

# Download per-block height/time/size traces via RPC (JSONL)
kresko download heights --nodes 8

# Upload data/ directory to S3
kresko upload-data
```

Downloaded logs go to `data/<node-name>/logs`. Height traces go to `data/heights.jsonl` with fields: `node, ip, height, hash, time, size`.

### 8. Teardown

```bash
# Stop tmux sessions and clean remote state
kresko reset --miners all --workers 8

# Destroy cloud instances
kresko down

# Nuclear option: destroy ALL kresko instances across all experiments
kresko down --all
```

`kresko down --all` is destructive across experiments. Confirm intent before running.

## Experiment Directory Layout

```
<experiment>/
  .env                              # Cloud + SSH + S3 credentials
  config.json                       # Instance definitions, chain ID, provider, genesis config
  zebrad.toml                       # Zebra config template
  payload/
    local_genesis/
      genesis.hex                   # Genesis block hex
      premine_blocks.hex            # Pre-mined blocks (one hex per line)
      checkpoints.txt               # Height-hash checkpoint pairs
      funded_keys.json              # Pre-allocated keys for txblast
    <node-hostname>/
      zebrad.toml                   # Per-node config (peers, miner address, testnet params)
      funded_key.json               # Node's funded key
    build/
      zebrad                        # zebrad binary
      kresko                        # Optional txblast binary
    vars.sh                         # Env vars for remote nodes
  payload.tar.gz                    # Cached tarball (rebuilt if payload/ is newer)
  data/                             # Downloaded logs and traces
  scripts/
    node_init.sh                    # Remote node setup script
    vars.sh                         # Env vars template
  progress.log.jsonl                # Block generation log from `kresko progress`
```

## Instance Selection

Several commands accept node selection patterns:
- `all` or `*` - all active instances
- `0,2,5` - comma-separated indices
- `miner-0-*,miner-1-*` - wildcard name patterns

Active means the instance has a public IP assigned (not "TBD").

## Writing Experiment Plans

When planning an experiment, write a plan that captures the intent and parameters before executing. A good plan includes:

1. **Goal**: What question does this experiment answer?
2. **Network topology**: Node count, regions, provider
3. **Chain parameters**: Chain ID, which network upgrade to target
4. **Workload**: Block time, concurrent miners, txblast rate/type/amount
5. **Duration or completion criteria**: Target block height, time duration, or specific condition
6. **Data collection**: What to download, what analysis to run
7. **Zebrad binary**: Path to the built binary (and which branch/commit it was built from)

Example plan:

```
## Goal
Measure block propagation latency across 8 geographically distributed miners
with NU6.1 enabled and transparent tx load at 10 tx/s.

## Setup
- Provider: digitalocean
- Chain ID: nu6-propagation-01
- Nodes: 8 miners, random regions
- Zebrad: ~/src/zebra/target/release/zebrad (branch: main, commit: abc123)

## Workload
- Block time: 10s, concurrent miners: 2, mode: round-robin
- Txblast: transparent, 10 tx/s, 0.001 ZEC per tx, all nodes

## Completion
- Target: 500 blocks
- Estimated time: ~40 minutes

## Data Collection
- progress.log.jsonl (block production timing)
- download heights from all 8 nodes (propagation analysis)
- download logs from all nodes (error analysis)

## Analysis
- Block propagation delay distribution across regions
- Transaction inclusion rate vs mempool pressure
- Per-node block production share
```

## Monitoring Long-Running Experiments

Experiments often run for 30+ minutes. Things that commonly go wrong:

- **Node unreachable**: Cloud provider recycled the VM, network issue, or SSH timeout. Run `kresko status` to check. If a node shows "unreachable", SSH in directly to diagnose: `ssh -i <key> root@<ip>`.
- **Node stuck at height 0**: Genesis seeding may have failed. Check `kresko-app.log` on the node. May need to `kresko reset` and `kresko deploy` again.
- **Block height not advancing**: `kresko progress` may have stopped or the node's RPC is unresponsive. Check the tmux `app` session on the node.
- **Txblast not running**: Check tmux `txblast` session. Common cause: insufficient UTXOs if the chain hasn't progressed enough before starting txblast.
- **DO rate limiting**: DigitalOcean API rate limits can cause `kresko up` failures. Wait a few minutes and retry.

To check experiment health:

```bash
# Are all nodes up and advancing?
kresko status

# What's the latest block production?
tail -5 progress.log.jsonl | python3 -c "
import sys, json
for line in sys.stdin:
    e = json.loads(line)
    print(f\"tick={e['tick']} miner={e['miner']} ok={e['ok']} height_proxy=tick*concurrent latency={e['latency_ms']}ms\")
"

# Is txblast still running on nodes?
kresko kill-session --session txblast  # Only if you need to restart it
```

When using Claude Code's `/schedule` to monitor, set a 10-15 minute interval. The monitoring check should:
1. Run `kresko check --json` to get structured health status (exit code 1 = unhealthy)
2. If unhealthy, inspect the `issues` array and attempt recovery (SSH into stuck nodes, retry failed operations)
3. Check `progress.log.jsonl` for recent entries (no entries = progress loop stopped)
4. Compare `max_height` from check output against the target completion height
5. If target reached: run `kresko download`, `kresko download heights`, `kresko upload-data`, then `kresko down`

## Data Analysis

After downloading experiment data, analyze with Python (pandas, matplotlib, numpy) and DuckDB.

### Data Files

- `progress.log.jsonl` - One JSON object per block generation attempt. Fields: `ts_unix_ms`, `tick`, `mode`, `mining_mode`, `miner`, `ip`, `ok`, `latency_ms`, `status_code`, `block_hash`, `error`.
- `data/heights.jsonl` - One JSON object per block per node. Fields: `node`, `ip`, `height`, `hash`, `time` (unix timestamp), `size` (bytes).
- `data/<node>/logs` or `data/<node>/logs.xz` - Raw zebrad stdout/stderr logs from each node.

### DuckDB

DuckDB reads JSONL natively. Use it for fast exploratory queries without loading into pandas first.

```sql
-- Load and inspect progress log
SELECT * FROM read_json_auto('progress.log.jsonl') LIMIT 5;

-- Block production success rate per miner
SELECT miner, count(*) as attempts, sum(ok::int) as successes,
       round(100.0 * sum(ok::int) / count(*), 1) as success_pct,
       round(avg(latency_ms), 0) as avg_latency_ms
FROM read_json_auto('progress.log.jsonl')
GROUP BY miner ORDER BY success_pct;

-- Block propagation: compare when each node saw each height
SELECT height, count(distinct node) as nodes_with_block,
       max(time) - min(time) as propagation_delay_s,
       min(time) as first_seen, max(time) as last_seen
FROM read_json_auto('data/heights.jsonl')
GROUP BY height ORDER BY height;

-- Propagation delay distribution
SELECT
    percentile_cont(0.50) WITHIN GROUP (ORDER BY prop_delay) as p50_s,
    percentile_cont(0.90) WITHIN GROUP (ORDER BY prop_delay) as p90_s,
    percentile_cont(0.99) WITHIN GROUP (ORDER BY prop_delay) as p99_s,
    max(prop_delay) as max_s
FROM (
    SELECT height, max(time) - min(time) as prop_delay
    FROM read_json_auto('data/heights.jsonl')
    GROUP BY height
    HAVING count(distinct node) > 1
);

-- Block size distribution
SELECT round(avg(size), 0) as avg_bytes, min(size) as min_bytes,
       max(size) as max_bytes,
       percentile_cont(0.50) WITHIN GROUP (ORDER BY size) as median_bytes
FROM read_json_auto('data/heights.jsonl');

-- Inter-block time distribution from a single node's perspective
WITH ordered AS (
    SELECT node, height, time,
           time - lag(time) OVER (PARTITION BY node ORDER BY height) as ibt_s
    FROM read_json_auto('data/heights.jsonl')
)
SELECT node, round(avg(ibt_s), 1) as avg_ibt_s,
       min(ibt_s) as min_ibt_s, max(ibt_s) as max_ibt_s,
       percentile_cont(0.50) WITHIN GROUP (ORDER BY ibt_s) as median_ibt_s
FROM ordered WHERE ibt_s IS NOT NULL
GROUP BY node;
```

Run DuckDB queries from the command line:

```bash
duckdb -c "SELECT ... FROM read_json_auto('progress.log.jsonl')"
```

Or in Python:

```python
import duckdb
con = duckdb.connect()
df = con.sql("SELECT * FROM read_json_auto('progress.log.jsonl')").df()
```

### Python Analysis

```python
import pandas as pd
import matplotlib.pyplot as plt
import numpy as np

# Load data
progress = pd.read_json('progress.log.jsonl', lines=True)
heights = pd.read_json('data/heights.jsonl', lines=True)

# Convert timestamps
progress['ts'] = pd.to_datetime(progress['ts_unix_ms'], unit='ms')
heights['dt'] = pd.to_datetime(heights['time'], unit='s')
```

Common analysis patterns:

```python
# Block production timeline
successful = progress[progress['ok'] == True]
plt.figure(figsize=(12, 4))
plt.scatter(successful['ts'], successful['latency_ms'], alpha=0.5, s=10)
plt.xlabel('Time')
plt.ylabel('Latency (ms)')
plt.title('Block Generation Latency Over Time')
plt.tight_layout()
plt.savefig('block_latency.png', dpi=150)

# Propagation delay per height
prop = heights.groupby('height').agg(
    nodes=('node', 'nunique'),
    first=('time', 'min'),
    last=('time', 'max')
)
prop['delay_s'] = prop['last'] - prop['first']
prop = prop[prop['nodes'] > 1]

plt.figure(figsize=(12, 4))
plt.plot(prop.index, prop['delay_s'])
plt.xlabel('Block Height')
plt.ylabel('Propagation Delay (s)')
plt.title('Block Propagation Delay')
plt.tight_layout()
plt.savefig('propagation_delay.png', dpi=150)

# Per-miner block share (from progress log)
shares = progress[progress['ok']].groupby('miner').size()
plt.figure(figsize=(8, 8))
plt.pie(shares, labels=shares.index, autopct='%1.0f%%')
plt.title('Block Production Share')
plt.tight_layout()
plt.savefig('miner_shares.png', dpi=150)

# Chain growth over time from each node's perspective
fig, ax = plt.subplots(figsize=(12, 6))
for node, group in heights.groupby('node'):
    ax.plot(group['dt'], group['height'], label=node, alpha=0.7)
ax.set_xlabel('Time')
ax.set_ylabel('Block Height')
ax.set_title('Chain Growth Per Node')
ax.legend(fontsize='small', ncol=2)
plt.tight_layout()
plt.savefig('chain_growth.png', dpi=150)
```

### Analysis Checklist

For a standard experiment report, produce:

1. **Summary stats**: Total blocks, duration, avg block time, success rate, node count
2. **Block production latency**: Time series plot + p50/p90/p99 stats
3. **Propagation delay**: Per-height delay + distribution (histogram or CDF)
4. **Per-miner fairness**: Block production share pie/bar chart
5. **Chain growth**: Height-over-time for all nodes overlaid
6. **Error analysis**: Count and categorize failures from progress log (`error` field)
7. **Block size**: Distribution if txblast was running
8. **Inter-block time**: Distribution and any outliers

## Debugging on Remote Nodes

SSH into a node directly:

```bash
ssh -i <key-path> -o StrictHostKeyChecking=no root@<node-ip>
```

Useful commands on the node:
- `tmux attach -t app` - view live zebrad output
- `tmux attach -t txblast` - view live txblast output
- `tail -f /root/logs` - stream zebrad logs
- `cat /root/kresko-app.log` - tmux session startup log
- `cat /root/.config/zebrad.toml` - node's zebra config
- `cat /root/payload/local_genesis/funded_keys.json` - funded keys
- `curl -s -H 'Content-Type: application/json' --data '{"jsonrpc":"2.0","id":1,"method":"getblockchaininfo","params":[]}' http://localhost:18232 | jq` - query local RPC
