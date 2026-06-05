#!/usr/bin/env python3
"""Reference fleet script — the whole authoring surface is `Fleet` methods.

Run it directly (so pyinfra and the other deps are present)::

    uv run examples/fleet_smoke.py up
    uv run examples/fleet_smoke.py deploy
    uv run examples/fleet_smoke.py smoke
    uv run examples/fleet_smoke.py down

There is no framework: this is plain Python. Spin up nodes, ship a payload,
run something, read measurements, tear down. A long-running network just omits
`down()`; a CI job names the fleet after the commit and tears it down in a
trap. See the CI example at the bottom.
"""

from __future__ import annotations

import os
import sys

from kresko import DigitalOcean, Fleet


def build() -> Fleet:
    fleet = Fleet(
        os.environ.get("KRESKO_FLEET", "smoke"),
        ssh={
            "user": os.environ.get("KRESKO_SSH_USER", "root"),
            "key_path": os.environ.get("KRESKO_SSH_KEY_PATH", "~/.ssh/id_ed25519"),
            "key_name": os.environ.get("KRESKO_SSH_KEY_NAME", ""),
        },
    )
    fleet.add(
        "miner",
        count=int(os.environ.get("KRESKO_MINER_COUNT", "1")),
        provider=DigitalOcean(
            region=os.environ.get("KRESKO_DO_REGION", "nyc3"),
            size=os.environ.get("KRESKO_DO_SIZE", "s-1vcpu-1gb"),
            image=os.environ.get("KRESKO_DO_IMAGE", "ubuntu-24-04-x64"),
        ),
        payload=["payload"],
    )
    return fleet


def main(argv: list[str]) -> int:
    action = argv[0] if argv else "plan"
    fleet = build()

    if action == "plan":
        result = fleet.plan()
    elif action == "up":
        result = fleet.up()
    elif action == "deploy":
        result = fleet.deploy("payload", role="miner")
    elif action == "smoke":
        # Long-running task: detached tmux session named "smoke".
        result = fleet.run(
            "bash -lc 'while true; do date; sleep 30; done'",
            role="miner",
            background="smoke",
            log_path="/root/smoke.log",
        )
    elif action == "status":
        result = fleet.status()
    elif action == "collect":
        result = fleet.collect("/root/logs", role="miner")
    elif action == "down":
        result = fleet.down()
    else:
        print(f"unknown action {action!r}", file=sys.stderr)
        return 2

    import json

    print(json.dumps(result, indent=2, sort_keys=True, default=str))
    return 0 if result.get("ok", False) else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))


# --- CI shape (for reference) ------------------------------------------------
#
#   # fleets/ci.py
#   import os, sys
#   from kresko import Fleet, Vultr
#
#   f = Fleet(f"ci-{os.environ['GIT_SHA']}", ssh={"key_name": "kresko-key"})
#   f.add("miner", count=4, provider=Vultr(region="ord", size="vc2-4c-8gb", image="os:1743"))
#   if f.up()["ok"]:
#       f.deploy("payload/")
#       f.run("kresko mine ...", role="miner", background="mine")
#       f.collect("/root/traces", role="miner")
#       sys.exit(0 if f.status()["ok"] else 1)
#   sys.exit(1)
#
#   # CI wrapper — teardown never depends on the script succeeding:
#   #   trap 'uv run kresko-fleet down "ci-$GIT_SHA"' EXIT
#   #   uv run fleets/ci.py
