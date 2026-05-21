# Kresko

Kresko is an experimental Zcash bench for spinning up arbitrary numbers of geographically distributed nodes, with a strong focus on being easy to debug for non-DevOps developers.

## Two pieces

Kresko is split into a Rust binary and a Python orchestration layer.

- **Rust `kresko`** — Zcash- and protocol-specific tooling: `genesis`,
  `txblast-local`, `mine`. Stays a standalone binary; experiment scripts
  invoke it as a subprocess.
- **Python `harness` / `kresko` CLI** — experiment lifecycle, provisioning,
  asset tracking, deploy, tmux, collect, sync, teardown.

## `~/.kresko/` is the home

Everything kresko (Python) does lives under `~/.kresko/` (override with
`KRESKO_HOME`):

```
~/.kresko/
├── .env                          # global credentials
├── config.toml                   # global defaults
├── cache/                        # disposable scratch
├── experiments/<exp>/            # stateless experiment source: run.py + payload + configs
├── runs/<exp>/<run-name>/        # one self-contained run per invocation
│   ├── manifest.json
│   ├── result.json
│   ├── run.py / payload/ / *.toml    # ← copied from experiments/<exp>/ at run start
│   ├── inventory.py / deploy_*.py    # ← generated
│   ├── stdout.log / stderr.log
│   ├── nodes/<name>.json             # immutable asset snapshot
│   └── data/                         # files collected from nodes
└── assets/<provider>-<id>.json   # one JSON per live cloud asset
```

### Invariants

1. Experiment scripts under `experiments/<exp>/` are pure orchestration. They
   never write to their own dir; re-running creates a new run, never
   overwrites.
2. A run is the unit of result encapsulation. Tar the run dir and you have
   everything that experiment produced — script, payload, configs, inventory,
   logs, node snapshots, collected data.
3. `~/.kresko/assets/` mirrors live cloud infrastructure. `kresko sync`
   refreshes it. Selectors and destroy paths read from it.

### Tagging contract

Every kresko-managed cloud asset carries:

- `kresko` — mandatory marker; sync/destroy refuse without it.
- `experiment-<experiment>`
- `role-<role>` — e.g. `role-miner`, `role-rpc`.
- `run-<run-name>`

The typed prefixes are intentionally short and provider-portable; the
`kresko` marker exists separately so a colliding `experiment-foo` tag from
some other tool can't trick us into deleting unrelated cloud instances.

### Run naming

Default run name is a UTC-timestamped slug like `r-20260507-141502`. Use
`--run-name <slug>` to set one explicitly. On collision, kresko appends
`-2`, `-3`, etc. — `runs/<exp>/<slug>/`, `runs/<exp>/<slug>-2/`, …

## Install

Install the Python CLI:

```bash
uv sync --extra dev
```

This registers the `kresko` script. Add the Rust binary to `$PATH` for
experiments that need it (`genesis`, `txblast-local`, `mine`):

```bash
cargo build --release
ln -sf "$PWD/target/release/kresko" ~/.local/bin/kresko
```

## Quick start

```bash
# 1) Bootstrap ~/.kresko/ and copy bundled reference experiments.
kresko init

# 2) (Optional) scaffold a new experiment from the reference one.
kresko init my-exp                          # copy pyinfra_do_smoke -> ~/.kresko/experiments/my-exp
kresko init my-exp --from pyinfra_do_smoke  # explicit reference
$EDITOR ~/.kresko/experiments/my-exp/run.py

# 3) Set credentials
$EDITOR ~/.kresko/.env     # DIGITALOCEAN_TOKEN, AWS_*, KRESKO_SSH_KEY_NAME, ...

# 4) Provision and drive
kresko run pyinfra_do_smoke -- plan
kresko run pyinfra_do_smoke --run-name nyc-1 -- up
kresko run pyinfra_do_smoke --run-name nyc-1 -- deploy
kresko run pyinfra_do_smoke --run-name nyc-1 -- smoke
kresko run pyinfra_do_smoke --run-name nyc-1 -- collect
kresko run pyinfra_do_smoke --run-name nyc-1 -- down
```

`kresko init` is a Rust subcommand. Re-running it is idempotent: existing
`~/.kresko/.env`, `config.toml`, and experiment directories are left
untouched. Pass `--force` with a `<name>` to overwrite a scaffolded
experiment.

`kresko run <exp> [--run-name <slug>] -- <verb>` allocates a new run dir,
copies `experiments/<exp>/` into it, and exec's the copied `run.py` with
the verb as its argv. The literal `--` is required when forwarding args
so that experiment-level flags (e.g. `--name` for node filtering) cannot
be silently consumed as a run dir name. The verbs (`plan / up / deploy /
run / collect / down`) come from the shared `run_experiment()` helper —
see "Writing an experiment".

## CLI reference

- `kresko init [<name>] [--from <ref>] [--force]` — bootstrap `~/.kresko/`
  (subdirs + `.env` + `config.toml` stubs), copy bundled reference
  experiments into `~/.kresko/experiments/`, and (with `<name>`) scaffold a
  new experiment from a reference. This is a Rust subcommand baked into the
  binary; the bundled experiments come from the build-time source tree.
- `kresko run <experiment> [--run-name <slug>] -- [args...]` — allocate a
  fresh run dir, copy `experiments/<exp>/` into it, exec the copied
  `run.py` with `KRESKO_EXPERIMENT` / `KRESKO_RUN_NAME` / `KRESKO_RUN_DIR`
  set. The `--` is required before forwarded args. By default the spawned
  `run.py` runs through `uv run --project <kresko-repo>` so pyinfra and
  the other repo deps are available; pass `--python <path>` to bypass uv.
- `kresko sync [--provider <name>]` — refresh `~/.kresko/assets/` from cloud
  providers. By default this tries every known provider and reports auth/API
  errors per provider; repeat `--provider` to limit the run.
- `kresko status [--experiment <name>] [--run <name>] [--role <role>]
  [--provider <name>] [--tag <tag>] [--name <node>] [--pattern <glob>]
  [--rpc-port <port>] [--timeout <secs>] [--summary] [--json]` — read the
  asset store, select the active nodes, and query each one's RPC for block
  height + sync progress (concurrently). Defaults to a table; `--summary`
  prints aggregate height stats with buckets. The RPC port defaults to
  `$KRESKO_RPC_PORT` or 8232 (mainnet/public); pass `--rpc-port 18232` for
  local-genesis nodes.
- `kresko assets list [--tag <tag>] [--provider <name>]` — list assets,
  filtered by tag (repeat `--tag` for AND).
- `kresko assets show <provider> <provider_id>` — print one asset.
- `kresko runs list <experiment>` — list runs, with stage/ok summary.
- `kresko runs show <experiment> <run-name>` — print manifest and result.

## Writing an experiment

Each experiment provides a `run.py` that builds an `Experiment` and hands
it to `run_experiment()`, which gives you the standard verbs for free:

```python
import os, sys
from harness import DigitalOcean, Experiment, Vultr, node_type, run_experiment


def build_experiment() -> Experiment:
    miner = node_type(
        role="miner",
        provider=DigitalOcean(region="nyc3", size="s-1vcpu-1gb"),
        payload=["payload"],
    )
    rpc = node_type(
        role="rpc",
        provider=Vultr(region="ord", size="vc2-1c-1gb", image="os:1743"),
        name_prefix="vultr-rpc",
        payload=["payload"],
    )
    exp = Experiment.current()         # picks up the run dir from env vars
    exp.add(miner, count=4)
    exp.add(rpc, count=1)
    return exp


def smoke(exp: Experiment, args) -> dict:
    # An experiment-specific verb. `args` is the parsed argparse Namespace,
    # so shared filters like `--role` / `--pattern` are available here too.
    return exp.run_tmux(
        "app",
        "zebrad -c /root/kresko/zebrad.toml",
        role=args.role,
        log_path="/root/app.log",
    )


if __name__ == "__main__":
    sys.exit(run_experiment(build_experiment, extra_actions={"smoke": smoke}))
```

`run_experiment()` parses the standard verbs (`plan / up / deploy / run /
reset / collect / down`) plus anything you register in `extra_actions`,
dispatches to the matching `Experiment` method, prints the result as
JSON, and exits non-zero when `result["ok"]` is false. Shared flags
include `--dry-run`, `--retry-failed`, `--role`, `--name`, `--pattern`,
`--failed-from`, `--command`, `--path`, `--dest`, `--force-tag`.

`reset` wipes Zebra state, configs, logs, and known kresko tmux sessions
(`zebra`, `app`, `mine`, `txblast`) on the selected nodes, leaving the
cloud instances themselves running. Use it to start the next deploy from a
clean slate without re-provisioning.

### Providers and overrides

DigitalOcean and Vultr can be used in the same experiment. Every node has a
provider, and names must be unique within the run; if you split one role across
providers, give one side a distinct `name_prefix`.

Vultr images must use explicit selectors because Vultr IDs are not
human-readable: `os:<id>`, `image:<uuid>`, `snapshot:<id>`, `app:<id>`, or
`iso:<id>`. Vultr `user_data` is accepted as plain text and encoded by the
adapter. IPv6 is off by default on Vultr; pass `enable_ipv6=True` if an
experiment requires it. Vultr `private_ip` is empty unless the instance is
attached to a VPC with `vpc_ids=[...]`.

Cloud size / image / region / count are first-class CLI flags so a run can be
retuned without editing the experiment script:

```bash
kresko run nu7-pow-4node -- up --size miner=s-8vcpu-16gb --count miner=8
kresko run nu7-pow-4node -- up --image miner=ubuntu-25-04-x64
kresko run nu7-pow-4node -- up --region ams3        # bare value = all roles
```

Each flag accepts `role=value` or just `value` (apply to all roles), and
unknown roles fail loudly so a typo cannot silently no-op.
These overrides are role-scoped, not provider-scoped; v1 assumes each role uses
one provider when using provider-specific size, image, or region slugs.

To call the Rust binary from inside a verb handler:

```python
exp.shell([
    "kresko", "genesis",
    "--zebrad-binary", os.environ["ZEBRAD_BIN"],
    "--out", str(exp.run_dir / "payload" / "local_genesis"),
])
```

`exp.shell()` tees stdout/stderr into the run dir. The Rust binary stays
unaware of `~/.kresko/`; pass `--out` paths it should write to.

### Block explorer

Any experiment can ship a co-located Zcash block explorer (the
[devdotbo/zcash-explorer](https://github.com/devdotbo/zcash-explorer) Phoenix
app) by adding one line to `build_experiment()`:

```python
exp.add_explorer(node="miner-0")
```

It then pops up during launch: once the target node is up, the explorer is
deployed there with `docker compose` and reaches that node's Zebra RPC
locally through `host.docker.internal`. The public URL is
`http://<node-ip>:20001` (testnet), recorded in the run dir's `explorer.json`.

Source delivery follows the S3 contract: the operator tars the explorer
source, uploads it to S3, and the node `curl`s a short-lived presigned URL —
never scp/rsync. This needs `AWS_S3_BUCKET` (plus AWS creds, and optionally
`AWS_S3_ENDPOINT`) in your `.env`. The container's secret `.env` is written
over the SSH session's stdin, so credentials never touch S3.

`add_explorer()` accepts overrides (each also settable via a
`KRESKO_EXPLORER_*` env var): `source`, `network` (`testnet` / `mainnet`),
`node`, `role`, `public_port`, `rpc_port`, `compose_service`,
`lightwalletd_enabled`.

For a testnet faucet, pass `faucet_enabled=True` or set
`KRESKO_EXPLORER_FAUCET_ENABLED=true`. Kresko discovers the selected node's
public funded/miner address from `/root/.config/funded_key.json` (falling back
to `mining.miner_address` in `/root/.config/zebrad.toml`) and writes the
explorer's faucet env:

```text
FAUCET_ENABLED=true
FAUCET_SOURCE_ADDRESS=<selected miner address>
FAUCET_AMOUNT=0.1
FAUCET_DAILY_IP_LIMIT=10
FAUCET_WINDOW_SECONDS=86400
FAUCET_MIN_CONFIRMATIONS=1
```

The faucet is refused for mainnet. The explorer still expects its configured
RPC endpoint to be able to sign wallet spends from `FAUCET_SOURCE_ADDRESS`; if
the node only exposes Zebra's read/mining RPC, the page can be configured but
faucet sends will fail until a wallet-capable RPC service owns that key.

Register the ops verbs by merging `explorer_actions()` into `extra_actions`:

```python
from harness import explorer_actions
...
run_experiment(build_experiment, extra_actions={**explorer_actions(), "smoke": smoke})
```

This adds `explorer-deploy`, `explorer-redeploy`, `explorer-status`,
`explorer-logs`, `explorer-stop`, and `explorer-plan`:

```bash
kresko run nu7-pow-4node --run-name r1 -- explorer-status
kresko run nu7-pow-4node --run-name r1 -- explorer-logs
kresko run nu7-pow-4node --run-name r1 -- explorer-redeploy   # after a source change
```

### Failure handling

`exp.up()` no longer raises on per-node failures. It returns:

```python
{
  "stage":      "up",
  "ok":         False,             # True iff every requested node came up
  "requested":  4,
  "succeeded":  3,
  "failed":     [{"name": "miner-3", "kind": "wait_timeout",
                  "region": "nyc3", "size": "s-1vcpu-1gb",
                  "message": "..."}],
  "plan":       {...},
}
```

Two failure shapes:

- **Create-time** (e.g. region capacity exhausted): no asset is written for
  the failed node; the failure is recorded in the run's `result.json`.
- **Wait-timeout** (instance created but never reported an IP): the asset is
  written with `status: "failed"` and a structured `failure_reason`. The
  selector layer treats `status: "failed"` as inactive, so subsequent
  `deploy / run / collect / down` automatically skip it.

To retry the failed nodes only, pass `--retry-failed`:

```bash
kresko run my-exp --run-name nyc-1 -- up --retry-failed
```

This re-polls the failed assets in place; healthy nodes are not touched.

### Programmatic / automation use

For scripts that drive an experiment without going through the CLI, use
the `open_run` context manager. It allocates a run dir, sets the
`KRESKO_*` env vars for the duration of the block, and restores them on
exit:

```python
from harness import open_run
from experiments.my_exp.run import build_experiment

with open_run("my-exp", name="auto-001"):
    exp = build_experiment()
    up = exp.up()
    if up["succeeded"]:
        exp.deploy()
        exp.run_tmux("smoke", "...", log_path="/root/smoke.log")
        exp.collect(["/root/logs"])
        exp.down()
```

## Debugging

- The Rust binary's app runs in tmux session `app` on each node.
- Tx blaster runs in tmux session `txblast`.
- Remote logs: `/root/logs`, `/root/kresko-app.log`, `/root/kresko-txblast.log`.
- Local logs: every run dir contains `stdout.log`, `stderr.log`,
  `pyinfra.<stage>.{stdout,stderr}.log`, plus per-shell logs from
  `experiment.shell()`.

## Notes and caveats

- Experimental project: interfaces and behavior may change.
- Payload distribution always goes through S3. `experiment.deploy()`
  uploads the payload tarball; nodes curl a presigned URL.
- `~/.kresko/` is per-user-per-host. If you run from two machines,
  `assets/` diverges until each runs `kresko sync`.
- Provider credentials are loaded from `~/.kresko/.env`.

## License

No license file is currently included in this repository.
