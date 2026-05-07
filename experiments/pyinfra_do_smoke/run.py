"""DigitalOcean smoke experiment.

Reference experiment showing the standard run model. Invoked as
`kresko run pyinfra_do_smoke <action>`. Supports the default lifecycle
verbs (plan, up, deploy, run, collect, down) plus `smoke` and `rust-help`.

For automation that bypasses the CLI entirely::

    from kresko_py import open_run
    from experiments.pyinfra_do_smoke.run import build_experiment

    with open_run("pyinfra_do_smoke", name="auto-001"):
        exp = build_experiment()
        up = exp.up()
        if up["succeeded"]:
            exp.deploy()
            exp.run_tmux("smoke", "bash -lc 'while true; do date; sleep 30; done'",
                         log_path="/root/smoke.log")
"""

from __future__ import annotations

import os
import sys

from kresko_py import DigitalOcean, Experiment, node_type, run_experiment


def _env_int(name: str, default: int) -> int:
    return int(os.environ.get(name, str(default)))


def build_experiment() -> Experiment:
    miner = node_type(
        role="miner",
        provider=DigitalOcean(
            region=os.environ.get("KRESKO_DO_REGION", "nyc3"),
            size=os.environ.get("KRESKO_DO_SIZE", "s-1vcpu-1gb"),
            image=os.environ.get("KRESKO_DO_IMAGE", "ubuntu-24-04-x64"),
        ),
        payload=["payload"],
    )

    experiment = Experiment.current(
        ssh={
            "user": os.environ.get("KRESKO_SSH_USER", "root"),
            "key_path": os.environ.get("KRESKO_SSH_KEY_PATH", "~/.ssh/id_ed25519"),
            "public_key_path": os.environ.get(
                "KRESKO_SSH_PUB_KEY_PATH", "~/.ssh/id_ed25519.pub"
            ),
            "key_name": os.environ.get("KRESKO_SSH_KEY_NAME", ""),
        },
    )
    experiment.add(miner, count=_env_int("KRESKO_MINER_COUNT", 1))
    return experiment


def smoke_action(exp: Experiment, args) -> dict:
    return exp.run_tmux(
        "smoke",
        os.environ.get(
            "KRESKO_SMOKE_COMMAND",
            "bash -lc 'while true; do date; sleep 30; done'",
        ),
        role=args.role,
        log_path="/root/smoke.log",
        dry_run=args.dry_run,
    )


def rust_help_action(exp: Experiment, args) -> dict:
    proc = exp.shell(["kresko", "--help"], check=False, log_name="rust-kresko-help")
    return {"stage": "rust-help", "ok": proc.returncode == 0, "returncode": proc.returncode}


if __name__ == "__main__":
    sys.exit(
        run_experiment(
            build_experiment,
            extra_actions={"smoke": smoke_action, "rust-help": rust_help_action},
        )
    )
