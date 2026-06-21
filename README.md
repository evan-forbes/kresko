# Kresko

Kresko is an experimental Zcash bench for spinning up arbitrary numbers of
geographically distributed nodes, with a strong focus on being easy to debug
for non-DevOps developers.

Think of it as a small, proprietary Ansible: spin up nodes and track their
metadata, deploy a payload, run something, read the measurements, tear down.

## Two pieces

Kresko is split into a Rust binary and a Python orchestration layer.

- **Rust `kresko`** — Zcash- and protocol-specific tooling: `genesis`,
  `txblast-local`, `mine`, the PoW simulator, Orchard proving. Generates and
  uses artifacts that Python can't. Stays a standalone binary; fleet scripts
  invoke it as a subprocess. This is the `kresko` command on your PATH.
- **Python `kresko` package** — the fleet API: provisioning, asset tracking,
  deploy, run/tmux, collect, sync, teardown. Orchestration is plain Python:
  `from kresko import Fleet`, then call methods. Its companion CLI is a
  separate command, **`kresko-fleet`** (equivalently `python -m kresko`), so it
  doesn't collide with the Rust `kresko` binary.

So there are two commands: `kresko` (the Rust binary — Zcash/compute) and
`kresko-fleet` (the Python fleet console — `ls`/`status`/`sync`/`assets`/
`down`/`archive`). `kresko-fleet` is installed by `uv sync`; run it with
`uv run kresko-fleet ...` (or directly once the venv is on PATH).

## The fleet model

A **fleet** is a named, tagged set of cloud nodes plus its accumulated state.

- A **long-running network** (e.g. the Vultr testnet) is a persistent fleet.
- A **CI job** is an ephemeral fleet named after the commit, torn down at the end.

Same operations either way. The only difference is whether you call `down()`.
There is no separate "experiment template" vs "run instantiation" — the fleet
dir is created on first `up()` and accumulates state in place.

## `~/.kresko/` is the home

Everything kresko (Python) does lives under `~/.kresko/` (override with
`KRESKO_HOME`):

```
~/.kresko/
├── .env                          # global credentials
├── config.toml                   # global defaults
├── cache/                        # disposable scratch
├── assets/<provider>-<id>.json   # GLOBAL mirror of every live cloud node
└── fleets/<name>/                # per-fleet working state
    ├── result.json               # last operation result
    ├── inventory.py / deploy_*.py # ← generated
    ├── *.log                     # operation + pyinfra logs
    ├── nodes/<name>.json         # per-node asset snapshot
    └── data/                     # files collected from nodes
```

### Invariants

1. `~/.kresko/assets/` is the source of truth — it mirrors live cloud
   infrastructure. `kresko-fleet sync` refreshes it; selectors and destroy
   paths read from it. The fleet is the working handle over it.
2. A fleet dir is a self-contained bundle. `kresko-fleet archive <name>` tars it
   (inventory, deploy files, logs, node snapshots, collected data).

### Tagging contract

Every kresko-managed cloud asset carries:

- `kresko` — mandatory marker; sync/destroy refuse without it.
- `fleet-<name>` — which fleet owns the node.
- `role-<role>` — e.g. `role-miner`, `role-rpc`.

The `kresko` marker exists separately so a colliding `fleet-foo` tag from some
other tool can't trick us into deleting unrelated cloud instances. Node
identity for idempotent `up` is `(fleet-<name> tag, node name)`.

## Install

The `Makefile` wraps the common installs:

```bash
make install      # build + install the Rust `kresko` binary to ~/.local/bin, plus the agent skill
make install-py   # install the Python fleet package + `kresko-fleet` CLI globally (uv tool)
```

For local development of the Python package, sync a venv instead and run from it:

```bash
uv sync --extra dev          # create .venv with deps (incl. pytest)
uv run pytest                # run the test suite
uv run examples/fleet_smoke.py plan
```

`make install-py` runs `uv tool install --force .`, putting `kresko-fleet` on
your PATH everywhere in an isolated environment. To import the library
(`from kresko import Fleet`) in another project, add this package as a
dependency there rather than relying on the global tool.

### Agent skill

`make install` also installs a [Claude Code](https://claude.com/claude-code)
skill (`skills/kresko-fleet/`) into `~/.claude/skills/` so an agent picks up how
to drive fleets automatically. Install or refresh it on its own with `make
install-skill`.

## Quick start

```bash
# 1) Bootstrap ~/.kresko/ (fleets/, assets/, cache/, .env, config.toml stubs).
kresko init

# 2) Set credentials
$EDITOR ~/.kresko/.env     # DIGITALOCEAN_TOKEN, VULTR_API_KEY, AWS_*, KRESKO_SSH_KEY_NAME, ...

# 3) Drive a fleet from a plain Python script (see examples/fleet_smoke.py)
uv run examples/fleet_smoke.py up
uv run examples/fleet_smoke.py deploy
uv run examples/fleet_smoke.py smoke
uv run examples/fleet_smoke.py down

# 4) Inspect / clean up from the asset store without re-running any script
uv run kresko-fleet ls
uv run kresko-fleet status smoke
uv run kresko-fleet down smoke   # destroy by fleet tag — works even if the script is gone
```

`kresko init` (a Rust subcommand) is idempotent: existing `.env`/`config.toml`
are left untouched. There are no bundled templates to scaffold — a fleet is
just a Python script.

## Authoring a fleet

The operations *are* the `Fleet` methods. That is the entire surface:

```python
from kresko import Fleet, DigitalOcean, Vultr

fleet = Fleet("ci-abc123", ssh={"key_name": "kresko-key"})
fleet.add("miner", count=4, provider=Vultr(region="ord", size="vc2-4c-8gb", image="os:1743"))
fleet.add("rpc",   count=1, provider=DigitalOcean(region="nyc3", size="s-2vcpu-4gb"))

fleet.up()                          # idempotent: create missing, adopt live, tag all
fleet.deploy("payload/")            # ship a payload to the nodes (pyinfra over SSH)
fleet.run("kresko mine ...", role="miner", background="mine")  # long-running (tmux session "mine")
fleet.run("kresko status ...", role="rpc")                     # ephemeral (waits, captures output)
heights = fleet.status()            # RPC heights / health -> dict, incl. an "ok" gate
fleet.collect("/root/traces", role="miner")   # pull -> fleets/<name>/data/
fleet.archive()                     # tar the fleet dir into a reproducible bundle
fleet.down()                        # tear down (omit for a long-running net)
```

| Method | Notes |
|---|---|
| `Fleet(name, *, ssh, tags, providers)` | constructed directly; state at `~/.kresko/fleets/<name>/` |
| `.add(role, count, *, provider, payload=..., name_prefix=...)` | declare nodes; one call |
| `.up(*, dry_run, retry_failed)` | idempotent against existing nodes |
| `.deploy(payload, *, role/name/pattern, state_snapshot=False)` | ships payload to nodes (pyinfra/SSH) |
| `.run(cmd, *, background=None, role/...)` | `background="name"` ⇒ detached tmux |
| `.collect(paths, *, role/..., dest=...)` | writes to `fleets/<name>/data/` |
| `.status(*, role/..., rpc_port=...)` | RPC heights/health + `ok` flag |
| `.reset(*, role/...)` | wipe node state without reprovisioning |
| `.down(*, dry_run, force_tag)` | tear down by fleet tag |
| `.archive(dest=...)` | tar the fleet dir |
| `.plan()` | `up(dry_run=True)` |

Node names are simple: `<role>-<index>` (e.g. `miner-0`). The fleet is *not*
in the name — scoping is the `fleet-<name>` tag, so the same `miner-0` can live
in two different fleets without collision.

### Providers and overrides

DigitalOcean and Vultr can be mixed in one fleet. Every node has a provider,
and names must be unique within the fleet; if you split one role across
providers, give one side a distinct `name_prefix`.

Vultr images must use explicit selectors because Vultr IDs are not
human-readable: `os:<id>`, `image:<uuid>`, `snapshot:<id>`, `app:<id>`, or
`iso:<id>`. Vultr `user_data` is accepted as plain text and encoded by the
adapter. IPv6 is off by default; pass `enable_ipv6=True` if needed. Vultr
`private_ip` is empty unless the instance is attached to a VPC (`vpc_ids=[...]`).

`fleet.override(role, size=..., image=..., region=..., count=...)` patches
already-added node specs (all roles when `role` is omitted), so a script can
retune from env/flags without rewriting the `add()` calls.

To call the Rust binary from a script:

```python
fleet.shell([
    "kresko", "genesis",
    "--zebrad-binary", os.environ["ZEBRAD_BIN"],
    "--out", str(fleet.dir / "payload" / "local_genesis"),
])
```

`fleet.shell()` tees stdout/stderr into the fleet dir. The Rust binary stays
unaware of `~/.kresko/`; pass it `--out` paths to write to.

### Snapshot bootstrap (sync from a state snapshot)

By default a public node syncs the whole chain over P2P from genesis. Opt into
hydrating zebrad's state DB from a pre-built snapshot instead — **default off**:

```python
fleet.deploy("payload/", role="rpc", state_snapshot=True)   # default mainnet mirror
fleet.deploy("payload/", state_snapshot="http://host/snapshot.tar.gz")
fleet.deploy("payload/")                                    # state_snapshot=False -> normal P2P sync
```

`state_snapshot` is `False | True | "<url>"`. `True` resolves to
`$KRESKO_STATE_SNAPSHOT_URL` or the default public mirror
(`http://mainnet.zebra.legends.sh/`). The node curls the tarball directly and
extracts it into zebrad's state cache before zebrad starts; this is an external
public dataset, so it deliberately does **not** go through the S3 payload path.
Public-network only, and the operator is responsible for pointing at a snapshot
for the right network — a mainnet snapshot will not verify on a testnet node.
The node-side mechanism also lives in `scripts/node_init_public.sh`
(`KRESKO_STATE_SNAPSHOT_URL`).

### Failure handling

`fleet.up()` does not raise on per-node failures. It returns:

```python
{
  "stage": "up", "ok": False,        # True iff every requested node came up
  "requested": 4, "succeeded": 3,
  "failed": [{"name": "miner-3", "kind": "timeout", "region": "nyc3", ...}],
  "plan": {...},
}
```

- **Create-time** failure (e.g. region capacity): no asset is written; the
  failure is recorded in the fleet's `result.json`.
- **Wait-timeout** (created but never reported an IP): the asset is written
  with `status: "failed"` and a structured `failure_reason`. Selectors treat
  `failed` as inactive, so `deploy / run / collect / down` skip it automatically.

`fleet.up(retry_failed=True)` re-polls the failed assets in place; healthy
nodes are untouched.

### Block explorer

A fleet can ship a co-located Zcash block explorer (the
[devdotbo/zcash-explorer](https://github.com/devdotbo/zcash-explorer) Phoenix
app):

```python
fleet.add_explorer(node="miner-0")
fleet.deploy_explorer()
```

The explorer is deployed to the target node with `docker compose` and reaches
that node's Zebra RPC locally. The public URL is `http://<node-ip>:20001`
(testnet), recorded in the fleet dir's `explorer.json`. Source delivery follows
the S3 contract: the operator tars the source, uploads it to S3, and the node
`curl`s a short-lived presigned URL — never scp/rsync. Needs `AWS_S3_BUCKET`
(plus AWS creds) in `.env`. Ops methods: `deploy_explorer`, `redeploy_explorer`,
`explorer_status`, `explorer_logs`, `explorer_stop`, `plan_explorer`.

For a testnet faucet, pass `faucet_enabled=True`. Kresko discovers the node's
funded/miner address and writes the explorer's faucet env. The faucet is
refused for mainnet.

## CLI reference (`kresko-fleet`)

`kresko-fleet` (the Python console; also `python -m kresko`) is a console over
the global asset store — orchestration lives in your scripts, so this is the
inspection + safety layer (it works without re-running any script, which is
what makes CI cleanup reliable). Note this is distinct from the Rust `kresko`
binary, whose `status` is the local-genesis config-dir status, not the fleet
asset-store status below.

- `kresko-fleet ls [<fleet>] [--provider <name>]` — list fleets and their nodes.
- `kresko-fleet status [<fleet>] [--role <role>] [--name <node>] [--pattern <glob>]
  [--provider <name>] [--tag <tag>] [--rpc-port <port>] [--timeout <secs>]
  [--summary] [--json]` — read the asset store, select active nodes, query each
  one's RPC for height + sync progress (concurrently). RPC port defaults to
  `$KRESKO_RPC_PORT` or 8232; pass `--rpc-port 18232` for local-genesis nodes.
- `kresko-fleet sync [--provider <name>]` — refresh `~/.kresko/assets/` from the
  clouds. Tries every known provider by default; repeat `--provider` to limit.
- `kresko-fleet assets list [--tag <tag>] [--provider <name>]` — list assets
  (repeat `--tag` for AND). `kresko-fleet assets show <provider> <provider_id>`.
- `kresko-fleet down <fleet> [--dry-run] [--force-tag <tag>]` — destroy a fleet
  by its tag, from the asset store, no script required. `--force-tag` (must be
  `fleet-<...>` or `role-<...>`) destroys everything carrying that tag.
- `kresko-fleet archive <fleet> [--dest <path>]` — tar the fleet dir.
- `kresko-fleet download traces <fleet> [--role <role>] [--name <node>]
  [--pattern <glob>] [--dest <path>] [--dry-run]` — pull standard logs and trace
  directories from the fleet's nodes into the fleet data dir.

`down` works two ways on purpose: `fleet.down()` from a script when you hold the
object, and `kresko-fleet down <name>` from the asset store when you don't (the
script is gone, the CI job is being cleaned up in a trap/finally):

```bash
trap 'uv run kresko-fleet down "ci-$GIT_SHA"' EXIT
uv run fleets/ci.py
uv run kresko-fleet status "ci-$GIT_SHA" --json > result.json
```

## Debugging

- Long-running tasks run in detached tmux sessions named by `background=`.
- Remote logs: `/root/logs`.
- Local logs: each fleet dir contains `result.json`,
  `pyinfra.<stage>.{stdout,stderr}.log`, plus per-shell logs from `fleet.shell()`.

## Notes and caveats

- Experimental project: interfaces and behavior may change.
- Payload transport is S3-first. `fleet.deploy()` tars the selected payloads,
  uploads the archive once to `AWS_S3_BUCKET`, presigns it, and each node
  `curl`s and extracts it under `/root/kresko` — never into `/root` and never
  by repeated scp/rsync uploads. The explorer source uses the same presigned-URL
  contract.
- `~/.kresko/` is per-user-per-host. Run from two machines and `assets/`
  diverges until each runs `kresko-fleet sync`.
- Provider credentials are loaded from `~/.kresko/.env` (and a project `.env`).

## License

No license file is currently included in this repository.
