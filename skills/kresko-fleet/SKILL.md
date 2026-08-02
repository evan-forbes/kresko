---
name: kresko-fleet
description: >-
  Operate Kresko fleets — provision, deploy, run, collect, and tear down
  geographically distributed Zcash (zebrad) nodes for testnets, mainnet
  observers, and CI. Use when authoring or running a fleet script
  (`from kresko import Fleet`), using the `kresko-fleet` CLI (ls/status/sync/
  assets/down/archive/download), debugging a stuck/leaked fleet, or doing
  anything with `~/.kresko/`, the asset store, payload deploys, the block
  explorer, or state-snapshot bootstrap.
---

# Kresko fleets

Kresko spins up arbitrary numbers of cloud nodes, tracks their metadata, deploys
a payload, runs something, reads measurements, and tears down. Think "small,
debuggable Ansible for Zcash." Two pieces, two commands:

- **Rust `kresko`** — Zcash/compute tooling: `genesis`, `txblast-local`, `mine`,
  the PoW simulator, `join-bundle`, `init`. A standalone binary on PATH; fleet
  scripts shell out to it. `kresko init` bootstraps `~/.kresko/` (idempotent).
- **Python `kresko` package** — the fleet API. Orchestration is **plain
  Python**: `from kresko import Fleet`, then call methods. Its companion console
  is a separate command, **`kresko-fleet`** (= `python -m kresko`), so it never
  collides with the Rust `kresko` binary.

There is no framework and no "experiment template vs run" split. A **fleet** is a
named, tagged set of nodes plus its state under `~/.kresko/fleets/<name>/`. A
long-running network is a persistent fleet; a CI job is an ephemeral fleet named
after the commit that calls `down()` at the end. Same operations either way.

## Authoring a fleet (the whole surface is `Fleet` methods)

```python
from kresko import Fleet, DigitalOcean, Vultr

fleet = Fleet("ci-abc123", ssh={"key_name": "kresko-key"})
fleet.add("miner", count=4, provider=Vultr(region="ord", size="vc2-4c-8gb", image="os:1743"))
fleet.add("rpc",   count=1, provider=DigitalOcean(region="nyc3", size="s-2vcpu-4gb"))

fleet.up()                                   # idempotent: create missing, adopt live, tag all
fleet.deploy("payload/")                     # ship a payload over pyinfra/SSH
fleet.run("kresko mine ...", role="miner", background="mine")  # long-running tmux session
fleet.run("kresko status ...", role="rpc")                    # ephemeral; waits, captures output
fleet.status()                               # RPC heights/health -> dict with an "ok" gate
fleet.collect("/root/traces", role="miner")  # pull -> fleets/<name>/data/
fleet.archive()                              # tar the fleet dir = reproducible bundle
fleet.down()                                 # tear down (omit for a long-running net)
```

| Method | Notes |
|---|---|
| `Fleet(name, *, ssh, tags, providers)` | direct constructor; state at `~/.kresko/fleets/<name>/` (also `fleet.dir`) |
| `.add(role, count, *, provider, payload=, name_prefix=)` | declare nodes in one call |
| `.override(role=None, *, size=, image=, region=, count=)` | patch already-added specs from env/flags |
| `.plan()` / `.up(*, dry_run, retry_failed)` | `plan()` == `up(dry_run=True)`; `up` is idempotent |
| `.deploy(payload, *, role/name/pattern, state_snapshot=False, dry_run=)` | pyinfra/SSH payload ship |
| `.run(cmd, *, background=None, role/..., log_path=, dry_run=)` | `background="name"` ⇒ detached tmux |
| `.collect(paths, *, role/..., dest=)` / `.download_traces(...)` | write to `fleets/<name>/data/` |
| `.status(*, role/..., rpc_port=)` / `.reset(...)` | RPC heights+`ok`; `reset` wipes node state and per-run evidence in place |
| `.archive(dest=)` / `.down(*, dry_run, force_tag)` | bundle / tear down by fleet tag |
| `.shell([...])` | run the Rust binary (or any cmd) locally, teeing logs into the fleet dir |
| `.add_explorer(node=)` + `.deploy_explorer()` / `redeploy/_status/_logs/_stop/plan_` | co-located block explorer |

Run a script with `uv run` so deps (pyinfra/paramiko) are present:
`uv run fleets/mainnet_zakura.py up`. Real examples: `examples/fleet_smoke.py`
(reference), `fleets/mainnet_zakura.py` (production mainnet observer fleet).

Node names are `<role>-<index>` (e.g. `miner-0`). The fleet is **not** in the
name — scoping is the `fleet-<name>` tag, so the same `miner-0` can live in two
fleets. Mix DigitalOcean + Vultr in one fleet; names must be unique (use
`name_prefix=` when splitting a role across providers). Vultr images need
explicit selectors (`os:<id>`, `image:<uuid>`, `snapshot:<id>`, `app:<id>`,
`iso:<id>`) because Vultr IDs aren't human-readable.

## The asset store is the source of truth (safety model)

`~/.kresko/assets/<provider>-<id>.json` mirrors every live cloud node. Selectors
and destroy paths read from it; `fleet.up()` adopts live nodes by
`(fleet-<name> tag, node name)`. Every kresko-managed instance carries:

- `kresko` — **mandatory** marker. sync/destroy refuse to act on anything
  without it. This is the guard that stops a colliding `fleet-foo` tag from some
  other tool from tricking us into deleting unrelated cloud instances.
- `fleet-<name>` — which fleet owns it. `role-<role>` — e.g. `role-miner`.

`up()` does **not** raise on per-node failure; it returns a dict with `ok`,
`requested`, `succeeded`, and a structured `failed` list. Wait-timeout nodes are
written with `status: "failed"` and skipped by deploy/run/collect/down;
`up(retry_failed=True)` re-polls them in place.

## CLI: inspection + safety only (`kresko-fleet`)

The script is the orchestration entrypoint. `kresko-fleet` is a console over the
**global asset store** that works *without re-running any script* — which is what
makes emergency teardown and CI cleanup reliable.

- `kresko-fleet ls [<fleet>] [--provider P]` — list fleets and their nodes.
- `kresko-fleet status [<fleet>] [--role/--name/--pattern/--tag/--provider] [--rpc-port N] [--summary] [--json]`
  — query each node's RPC for height + sync progress concurrently. RPC port
  defaults to `$KRESKO_RPC_PORT` or 8232; pass `--rpc-port 18232` for
  local-genesis nodes.
- `kresko-fleet sync [--provider P ...]` — refresh `~/.kresko/assets/` from the
  clouds (every known provider by default).
- `kresko-fleet assets list [--tag T ...] [--provider P]` / `assets show <provider> <id>`.
- `kresko-fleet down <fleet> [--dry-run] [--force-tag fleet-…|role-…]` — destroy
  by tag, no script needed.
- `kresko-fleet archive <fleet> [--dest PATH]` — tar the fleet dir.
- `kresko-fleet download traces <fleet> [--role/--name/--pattern] [--dest] [--dry-run]`
  — pull standard logs + trace dirs into the fleet data dir.

`down` works two ways on purpose — `fleet.down()` when you hold the object, and
`kresko-fleet down <name>` from the asset store when you don't (CI trap, leaked
fleet):

```bash
trap 'uv run kresko-fleet down "ci-$GIT_SHA"' EXIT
uv run fleets/ci.py
uv run kresko-fleet status "ci-$GIT_SHA" --json > result.json
```

## Payload transport — important contract

- `fleet.deploy()` ships payloads over the **pyinfra SSH transport**
  (`files.sync`/`files.put`).
- The **S3 presigned-URL path** (operator uploads to S3; the node `curl`s a
  short-lived URL — **never scp/rsync**) is used for the **explorer source** and
  the **public-testnet join bundle**, where pushing large/owned artifacts to many
  nodes warrants it. Needs `AWS_S3_BUCKET` + AWS creds in `.env`.
- **Never scp/rsync owned payloads to nodes** as a substitute for these paths.

## State-snapshot bootstrap (optional, default off)

By default a public node syncs the whole chain over P2P from genesis. Opt into
hydrating zebrad's state DB from a pre-built snapshot:

```python
fleet.deploy("payload/", role="rpc", state_snapshot=True)              # default mainnet mirror
fleet.deploy("payload/", state_snapshot="http://host/snapshot.tar.gz") # explicit URL
fleet.deploy("payload/")                                               # False -> normal P2P sync
```

`state_snapshot` is `False | True | "<url>"`. `True` resolves to
`$KRESKO_STATE_SNAPSHOT_URL` or the default public mirror
(`http://mainnet.zebra.legends.sh/`). The node `curl`s the tarball directly and
extracts it into zebrad's state cache before zebrad starts. This is an external
public dataset, so it deliberately does **not** go through the S3 payload path.
Public-network only, and the snapshot's network must match the node's network (a
mainnet snapshot will not verify on a testnet node). Node-side mechanism lives in
`scripts/node_init_public.sh`.

## Operational gotchas

- **Credentials** live in `~/.kresko/.env` (`DIGITALOCEAN_TOKEN`, `VULTR_API_KEY`,
  `AWS_*`, `AWS_S3_BUCKET`, `KRESKO_SSH_KEY_NAME`, `KRESKO_SSH_KEY_PATH`). A
  project `.env` is also loaded. `KRESKO_HOME` overrides `~/.kresko/`.
- **Passphrase-protected SSH key:** load it into `ssh-agent` and **unset
  `KRESKO_SSH_KEY_PATH`** so pyinfra uses the agent instead of trying to read the
  encrypted key file directly.
- **Local Rust release build needs** `CXXFLAGS='-include cstdint'` (Arch GCC +
  rocksdb): `CXXFLAGS='-include cstdint' cargo build --release --bin kresko`.
  The Ubuntu deploy binary is built with `make ubuntu` (or `scripts/build-ubuntu.sh`).
- **`~/.kresko/` is per-user-per-host.** Run from two machines and `assets/`
  diverges until each runs `kresko-fleet sync`.
- **Logs:** remote `/root/logs`; local — each fleet dir has `result.json`,
  `pyinfra.<stage>.{stdout,stderr}.log`, and per-`shell()` logs.

## Install / where to look

- `make install` — Rust `kresko` binary (to `~/.local/bin`) + this skill.
- `make install-py` — `uv tool install` the Python package (`kresko-fleet` CLI on PATH).
- `uv sync --extra dev` — dev environment (run tests with `uv run pytest`).
- Full reference: `README.md`. Design rationale: `docs/simplify-to-fleets.md`.
