# Simplifying kresko: experiments + runs → fleets

Status: **implemented** (branch `refactor-fleets`). Authoring surface:
**plain Python** (import a class, call methods). CLI is a thin inspection/safety
layer, not a framework.

Resolved open questions from the original plan:
- **Package renamed `harness` → `kresko`** — `from kresko import Fleet`. Done.
- **`kresko exec`** — not added; the script holds the `Fleet` object, and
  `kresko down`/`status`/`ls` cover the no-script cases.
- **`archive`** — `fleet.archive()` / `kresko archive <name>` writes
  `~/.kresko/fleets/<name>.tar.gz`.
- **Snapshot artifact format** — the URL is treated as a direct tarball URL
  (curl + `tar -xzf` into the state cache); `mainnet.zebra.legends.sh` was
  unreachable from the dev sandbox, so the exact "latest" pointer is left to
  the operator-supplied URL. Wire-up is in place (`state_snapshot=...`).
- **Explorer** — kept working against `Fleet` (demoted, not extracted): the
  `add_explorer`/`deploy_explorer` methods and `kresko.explorer` remain, now
  pointed at the fleet dir. Full extraction to a plain payload is still a later
  step.
- Node re-tag of the 4 live Vultr nodes (phase 5) is an operational step
  requiring live API creds — not performed here; `up` adopts by
  `(fleet tag, name)` once they carry `fleet-<name>`.

## The core idea

Collapse the two-level `experiment` (template) + `run` (instantiation) model into
one noun: the **fleet**.

- A **fleet** is a named, tagged set of nodes plus its accumulated state.
- A **long-running network** (the Vultr testnet) is a persistent fleet.
- A **CI job** is an ephemeral fleet named after the commit, destroyed at the end.

Same operations either way. The only difference is whether you call `down()`.
This removes: the experiment-vs-run distinction, the copy-template-into-run-dir
machinery, the `run_experiment()` / `build_experiment()` / `extra_actions` shim,
and the auto-incrementing numbered run dirs.

The asset store (`~/.kresko/assets/` + the `kresko` safety tag) is already the
real source of truth. It stays. The fleet is the working handle over it.

## Decisions (locked in)

- **Idempotent `up`** — yes. `up()` creates only missing nodes and skips live ones.
- **Node names stay simple: `<role>-<i>`** (e.g. `miner-1`). The fleet is **not**
  encoded in the name. Scoping is done purely by the `fleet-<name>` **tag**.
  Node identity for skip/adopt = `(fleet-<name> tag, node name)`. The same
  `miner-1` can exist in two fleets — no collision, because the global asset
  store is keyed by `<provider>-<id>` and disambiguated by the fleet tag. (Only
  if an operator ever wants display disambiguation do we optionally prefix
  `<fleet>-miner-1` — not the default.)
- **Adoption via fleet tags.** `up` on a fleet adopts any live asset already
  carrying that fleet tag + name. The 4 running Vultr testnet nodes get a
  one-shot re-tag to the new `fleet-<name>` scheme; the running net is preserved,
  no instance churn.
- **No backwards compatibility.** This is a clean break on its own branch/commit.
  No deprecated `run_experiment` shim, no parallel old/new code path — delete and
  rewrite.

## The operation surface (this is "what operations are present")

Because authoring is plain Python, **the operations *are* the `Fleet` methods.**
That is the entire intuitive surface:

```python
from kresko import Fleet, DigitalOcean, Vultr   # package rename harness -> kresko (optional, see below)

fleet = Fleet("ci-abc123")          # names the fleet; state at ~/.kresko/fleets/ci-abc123/
fleet.add("miner", count=4, provider=Vultr(region="ord", size="vc2-4c-8gb", image="os:1743"))
fleet.add("rpc",   count=1, provider=DigitalOcean(region="nyc3", size="s-2vcpu-4gb"))

fleet.up()                          # idempotent: create missing nodes, skip live ones, tag all
fleet.deploy("payload/")            # ship a payload (S3-only contract preserved)
fleet.run("kresko mine ...", role="miner", background="mine")  # long-running (tmux session "mine")
fleet.run("kresko status ...", role="rpc")                     # ephemeral (waits, captures output)
heights = fleet.status()            # RPC heights / health  -> dict
fleet.collect("/root/traces", role="miner")   # pull -> ~/.kresko/fleets/ci-abc123/data/
fleet.archive()                     # optional: tar the fleet dir = reproducible bundle
fleet.down()                        # tear down (omit for a long-running net)
```

Method list (the whole API):

| Method | Replaces today | Notes |
|---|---|---|
| `Fleet(name)` | `Experiment` + `run_experiment` + `open_run` + `current()` | constructed directly; no factory, no env-var coupling |
| `.add(role, count, provider, **opts)` | `node_type()` + `.add(node, count)` | one call instead of two |
| `.up(...)` | `.up()` | now **idempotent** against existing nodes |
| `.deploy(payload, ...)` | `.deploy()` | unchanged mechanics (S3) |
| `.run(cmd, *, background=None, ...)` | `.run_command()` + `.run_tmux()` | one method; `background="name"` ⇒ tmux/long-running |
| `.collect(path, ...)` | `.collect()` | writes to `fleets/<name>/data/` |
| `.status()` | `kresko status` | RPC heights / health |
| `.reset(...)` | `.reset()` | keep — wipe node state without reprovisioning |
| `.down(...)` | `.down()` | tear down |
| `.archive()` | the implicit value of "runs" | tar the fleet dir on demand |

## CLI = inspection + safety only (no orchestration shim)

The script *is* the orchestration entrypoint (`python fleets/ci.py` / `uv run`).
The `kresko` Python CLI shrinks to a console over the **global asset store** —
deliberately able to operate without re-running any script, which is what makes
emergency teardown and CI cleanup reliable:

```
kresko ls                       # list fleets and their nodes
kresko status <fleet>           # heights/health (reads asset store)
kresko sync [--provider ...]    # refresh ~/.kresko/assets/ from clouds
kresko assets list|show ...     # raw asset inspection
kresko down <fleet>             # destroy by fleet tag — no script needed (CI trap, leaked fleet)
kresko archive <fleet>          # tar the fleet dir
```

`down` works two ways on purpose: `fleet.down()` from a script when you hold the
object, and `kresko down <name>` from the asset store when you don't (the script
is gone, the CI job is being cleaned up in a `trap`/`finally`).

Deleted CLI surface: `kresko run <exp> -- <verb>` (copy-then-exec), `kresko runs
list|show`, the verb dispatcher, `_apply_provider_overrides`. (Optional keep:
`kresko exec <fleet> -- <cmd>` for ad-hoc one-off commands without writing a
script — decide later.)

## State model

```
~/.kresko/
├── .env                          # creds                          (keep)
├── config.toml                   # defaults                       (keep)
├── assets/<provider>-<id>.json   # GLOBAL mirror of all live nodes (keep: safety + sync)
└── fleets/<name>/                # replaces experiments/ + runs/
    ├── nodes/<node>.json         # per-node snapshot   (was runs/<exp>/<run>/nodes/)
    ├── data/                     # collected files     (was runs/<exp>/<run>/data/)
    └── log/                      # operation logs      (was stdout/stderr/pyinfra logs)
```

- Global `assets/` is unchanged — still the truth for `sync`/`down` and the
  `kresko` safety marker.
- `fleets/<name>/` is created on first `up`, accumulates state, is the unit you
  `archive`. No template copy step.

### Tagging contract (simplified)

Today: `kresko`, `experiment-<exp>`, `role-<role>`, `run-<run>`.
New: `kresko`, `fleet-<name>`, `role-<role>`. Two tags collapse into one
(`fleet-<name>`). The `kresko` marker still gates all destroy/sync.

## File-by-file disposition

**Keep, minor edits (swap `experiment`+`run` filtering for `fleet`):**
- `harness/providers.py` — provider adapters. As-is.
- `harness/assets.py` — read/write asset store. Filter by `fleet` tag.
- `harness/sync.py`, `harness/reconcile.py` — cloud → local. Adjust tag fields.
- `harness/status.py` — RPC heights. Select by fleet.
- `harness/s3.py` — payload upload. As-is.
- `harness/remote.py`, `harness/inventory.py` — pyinfra/ssh. As-is.
- `harness/selectors.py` — keep `is_active`/`select`; `run_name` → `fleet`.
- `harness/env.py`, `harness/paths.py` — add `fleet_dir()`; drop `run_dir`,
  `runs_dir`, `experiment_dir`, `experiments_dir`.

**Rewrite / rename:**
- `harness/experiment.py` → `harness/fleet.py`. `Experiment` → `Fleet`.
  - constructor takes `name`, points state at `fleet_dir(name)`.
  - drop `current()`, `open_run` coupling, the `KRESKO_RUN_*` env vars.
  - merge `node_type()` + `add()` → `add(role, count, provider, **opts)`.
  - merge `run_command` + `run_tmux` → `run(cmd, *, background=None, ...)`.
  - `up()` becomes idempotent (see open question on node identity).
- `harness/cli.py` → slim to the inspection/safety verbs above. Delete
  `run_experiment`, `_build_experiment_parser`, `_apply_provider_overrides`,
  `cmd_run`, `cmd_runs`.

**Delete (salvage noted bits):**
- `harness/runs.py` — run-dir lifecycle / manifest / result. Salvage
  `git_revision` and `write_node_snapshot` into `fleet.py`/`assets.py`.
- `harness/spec.py` — `ExperimentSpec`/`NodeGroup`/`load_spec`. Fleet owns its
  node list directly.
- `experiments/pyinfra_do_smoke/` template + Rust `kresko init` experiment
  scaffolding. (Rust `init` may keep creating the `~/.kresko/` skeleton, or drop
  it and let the library create the home lazily on first `Fleet(...)`.)

**Demote (defer full extraction):**
- `harness/explorer.py` (33KB) + `add_explorer` + `explorer_actions` + the six
  `explorer-*` verbs. The explorer is "a payload deployed to one node" — move it
  onto the normal `deploy`/`run` path. Phase 1: stop wiring it into the Fleet
  core; full extraction later.

**Untouched:**
- The entire Rust binary (`src/`). genesis / mine / txblast / pow-* stay as the
  artifact-generation + node-side compute layer. The Python/Rust split is a
  strength — keep it. (Only optional trim: `kresko init` no longer copying
  experiment templates.)

**Tests (`tests/`):**
- delete `test_runs.py`; `test_experiment.py` → `test_fleet.py`.
- update `test_selectors.py`, `test_assets.py`, `test_status.py`, `test_cli.py`
  for the `fleet` tag and the slim CLI.
- `test_digitalocean.py`, `test_vultr.py`, `test_s3.py`, `test_remote.py`,
  `test_env.py`, `test_paths.py` — mostly unchanged.

## CI usage after the refactor (plain Python + safety CLI)

```python
# fleets/ci.py
import os, sys
from kresko import Fleet, Vultr

f = Fleet(f"ci-{os.environ['GIT_SHA']}")
f.add("miner", count=4, provider=Vultr(region="ord", size="vc2-4c-8gb", image="os:1743"))

if f.up()["ok"]:
    f.deploy("payload/")
    f.run("kresko mine --rpc-endpoint http://localhost:18232", role="miner", background="mine")
    f.run("<bench>", role="miner")            # ephemeral
    f.collect("/root/traces", role="miner")
    sys.exit(0 if f.status()["ok"] else 1)
sys.exit(1)
```

```bash
# CI wrapper — teardown never depends on the script succeeding
trap 'kresko down "ci-$GIT_SHA"' EXIT
uv run fleets/ci.py
kresko status "ci-$GIT_SHA" --json > result.json
```

## Snapshot bootstrap (sync from a state snapshot)

New capability, **default off**: bring a node up by hydrating zebrad's state DB
from a pre-built snapshot instead of syncing the whole chain over P2P from
genesis. Source defaults to `http://mainnet.zebra.legends.sh/`.

- **Scope:** the public-network path (mainnet/testnet observer/RPC nodes). It is
  meaningless for local-genesis private nets — those have their own genesis and
  no upstream chain to snapshot — so enabling it there is an error, not a no-op.
- **Default:** `False`. Opt-in per deploy (or per role).
- **API (plain Python):**
  ```python
  fleet.deploy("payload/", role="rpc", state_snapshot=True)   # uses the default mainnet URL
  fleet.deploy("payload/", state_snapshot="http://mainnet.zebra.legends.sh/<artifact>")
  # default:
  fleet.deploy("payload/")                                    # state_snapshot=False -> normal P2P sync
  ```
  `state_snapshot`: `False | True | "<url>"`. `True` resolves to the default URL
  (config-overridable via `config.toml` / `KRESKO_STATE_SNAPSHOT_URL`).
- **Mechanics (node-side, in node init before zebrad starts):** curl the snapshot
  tarball from the URL, verify, extract into zebrad's `state.cache_dir`, then start
  zebrad — it resumes from the snapshot height instead of height 0. This is a
  **public read mirror the node curls directly**; it does *not* go through the S3
  payload/presign path (that contract is for artifacts we own — this is an
  external public dataset).
- **Guardrails:** the snapshot's network must match the fleet's zebrad network;
  refuse a mainnet snapshot for a testnet/regtest node. Lives in the node-init
  step (`scripts/node_init_public.sh` today), with the Python `Fleet` layer only
  threading the enable flag + URL through.

## Phasing (clean break, one branch)

No backwards compat — the whole thing lands on a dedicated branch. Order chosen so
each step compiles/tests green before the next:

1. **`fleet.py` + `paths.fleet_dir()`.** Write `Fleet` (rename of `Experiment`):
   direct constructor, `<role>-<i>` naming, idempotent `up()` + tag-based adopt,
   merged `add()` / `run()`. Delete `experiment.py`, `runs.py`, `spec.py` in the
   same step (nothing dual-path).
2. **Slim CLI.** Replace the verb dispatcher with inspection/safety verbs incl.
   `kresko down <name>` off the asset store. Delete `run_experiment`, `cmd_run`,
   `cmd_runs`, `_build_experiment_parser`, `_apply_provider_overrides`.
3. **Snapshot bootstrap.** Add the `state_snapshot` deploy option (see below).
4. **Demote the explorer** to a payload.
5. **One-shot re-tag** the 4 live Vultr nodes to `fleet-<name>`; verify `up`
   adopts them with zero instance churn.
6. **Docs + tests + README** rewrite to the fleet model; drop `experiments/`
   templates and the Rust `init` template-copy.

## Open questions (smaller)

1. **Package rename `harness` → `kresko`?** `from kresko import Fleet` reads
   better than `from harness import Fleet`. Since this is a clean break, now is
   the cheap time to do it. Leaning yes.
2. **Keep an ad-hoc `kresko exec <fleet> -- <cmd>`?** Lets you run a one-off
   command without writing a script. Small, but it re-adds a CLI exec path.
3. **`archive` format / retention** — tar.gz of `fleets/<name>/`? Where to?
4. **Snapshot index format** — does `http://mainnet.zebra.legends.sh/` expose a
   predictable artifact path / latest pointer, or do we pin a full URL? (see below)
