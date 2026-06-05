#!/usr/bin/env python3
"""Mainnet Zakura fleet orchestration.

This script keeps the long-running mainnet Zakura workflow in one reusable
place while the Fleet API owns provisioning, deploy, run, collect, and down.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path
from typing import Any

from kresko import DigitalOcean, Fleet
from kresko.fleet import TRACE_COLLECTION_PATHS


DEFAULT_FLEET = "mainnet-zakura"
DEFAULT_SIZE = "s-4vcpu-8gb"
DEFAULT_IMAGE = "ubuntu-24-04-x64"
DEFAULT_REGIONS = {
    "asia": "sgp1",
    "us": "nyc3",
    "europe": "fra1",
}
PR17_HEAD_SHA = "d5649f8111eb19350a1818303586463b308af5fc"
DEFAULT_ZAKURA_ROOT = "/home/evan/src/valar/zakura"
DEFAULT_TRACE_FILTER = "info,zebrad=debug,zebra_network=debug,zebra_state=debug,zebra_rpc=debug"
DEFAULT_TRACE_LOG_FILE = "/root/logs/zakura-tracing.log"
FAST_BLOCK_SYNC_PEER_TARGET = 100
FAST_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT = 100


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Operate the mainnet Zakura fleet")
    parser.add_argument(
        "action",
        choices=[
            "plan",
            "up",
            "build",
            "payload",
            "deploy",
            "start",
            "status",
            "download-traces",
            "down",
        ],
    )
    parser.add_argument("--dry-run", action="store_true", help="plan without running remote mutations")
    parser.add_argument("--zebrad-binary", help="prebuilt Zakura zebrad binary for payload generation")
    parser.add_argument("--skip-build", action="store_true", help="payload action: do not build Zakura")
    args = parser.parse_args(argv)

    require_valar_do_token()
    fleet = make_fleet()

    if args.action == "plan":
        return print_result(plan_payload(fleet))
    if args.action == "up":
        return print_result(fleet.up(dry_run=args.dry_run))
    if args.action == "build":
        return print_result(build_zakura(fleet))
    if args.action == "payload":
        zebrad_binary = args.zebrad_binary
        provenance: dict[str, Any] = {}
        if not zebrad_binary and not args.skip_build:
            provenance = build_zakura(fleet)
            zebrad_binary = provenance["binary"]
        if not zebrad_binary:
            raise SystemExit("--zebrad-binary is required with --skip-build")
        return print_result(generate_payload(fleet, Path(zebrad_binary), provenance=provenance))
    if args.action == "deploy":
        return print_result(
            fleet.deploy(str(fleet.dir / "payload"), state_snapshot=True, dry_run=args.dry_run)
        )
    if args.action == "start":
        return print_result(
            fleet.run(
                "/root/kresko/payload/node_init.sh",
                background="zakura",
                log_path="/root/logs/node_init.log",
                dry_run=args.dry_run,
            )
        )
    if args.action == "status":
        return print_result(fleet.status())
    if args.action == "download-traces":
        return print_result(fleet.download_traces(dry_run=args.dry_run))
    if args.action == "down":
        return print_result(fleet.down(dry_run=args.dry_run))
    raise AssertionError(args.action)


def require_valar_do_token(env: dict[str, str] | None = None) -> None:
    target = os.environ if env is None else env
    valar = target.get("VALAR_DO_TOKEN")
    if not valar:
        raise SystemExit(
            "VALAR_DO_TOKEN is not set. The mainnet-zakura fleet uses only the "
            "valar DigitalOcean account and will not silently fall back to a "
            "different DIGITALOCEAN_TOKEN. Export VALAR_DO_TOKEN and retry."
        )
    # VALAR_DO_TOKEN is authoritative for this fleet: pin provisioning to the
    # valar DigitalOcean account, overriding any stale DIGITALOCEAN_TOKEN that
    # may be lingering in the shell.
    target["DIGITALOCEAN_TOKEN"] = valar


def make_fleet(env: dict[str, str] | None = None) -> Fleet:
    target = os.environ if env is None else env
    require_valar_do_token(target)

    fleet_name = target.get("KRESKO_FLEET", DEFAULT_FLEET)
    ssh = {
        "key_name": target.get("KRESKO_SSH_KEY_NAME", ""),
        "key_path": target.get("KRESKO_SSH_KEY_PATH", ""),
    }
    fleet = Fleet(fleet_name, tags=["zakura", "mainnet"], ssh=ssh)
    for prefix, region in regions(target).items():
        fleet.add(
            "node",
            count=1,
            name_prefix=prefix,
            provider=DigitalOcean(
                region=region,
                size=target.get("KRESKO_DO_SIZE", DEFAULT_SIZE),
                image=target.get("KRESKO_DO_IMAGE", DEFAULT_IMAGE),
                tags=["zakura", "mainnet"],
            ),
        )
    return fleet


def regions(env: dict[str, str] | None = None) -> dict[str, str]:
    target = os.environ if env is None else env
    if target.get("KRESKO_ZAKURA_REGIONS"):
        values = [v.strip() for v in target["KRESKO_ZAKURA_REGIONS"].split(",") if v.strip()]
        if len(values) != 3:
            raise ValueError("KRESKO_ZAKURA_REGIONS must contain exactly three comma-separated slugs")
        return dict(zip(("asia", "us", "europe"), values, strict=True))
    return {
        "asia": target.get("KRESKO_ASIA_REGION", DEFAULT_REGIONS["asia"]),
        "us": target.get("KRESKO_US_REGION", DEFAULT_REGIONS["us"]),
        "europe": target.get("KRESKO_EUROPE_REGION", DEFAULT_REGIONS["europe"]),
    }


def build_zakura(fleet: Fleet, env: dict[str, str] | None = None) -> dict[str, Any]:
    target = os.environ if env is None else env
    root = Path(target.get("ZAKURA_ROOT", DEFAULT_ZAKURA_ROOT)).expanduser()
    ref = target.get("ZAKURA_REF", PR17_HEAD_SHA)
    build_command = ["cargo", "xtask", "package", "ubuntu"]

    run(["git", "-C", str(root), "checkout", ref])
    patched_files = patch_zakura_fast_sync_defaults(root)
    run(build_command, cwd=root)
    commit = capture(["git", "-C", str(root), "rev-parse", "HEAD"])
    binary = root / "target" / "ubuntu" / "zebra"
    if not binary.exists():
        raise FileNotFoundError(binary)

    provenance = {
        "repo": str(root),
        "ref": ref,
        "commit": commit,
        "binary": str(binary),
        "binary_sha256": sha256_file(binary),
        "build_command": " ".join(build_command),
        "fallback_ref": ref == PR17_HEAD_SHA,
        "fast_block_sync_peer_target": FAST_BLOCK_SYNC_PEER_TARGET,
        "fast_block_sync_download_concurrency_limit": FAST_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT,
        "fast_block_sync_patched_files": [str(path) for path in patched_files],
    }
    (fleet.dir / "zakura-build.json").write_text(
        json.dumps(provenance, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    return provenance


def generate_payload(
    fleet: Fleet,
    zebrad_binary: Path,
    *,
    provenance: dict[str, Any] | None = None,
) -> dict[str, Any]:
    write_public_config(fleet)
    kresko_binary = find_kresko_binary()
    cmd = [
        str(kresko_binary),
        "genesis-public",
        "--zebrad-binary",
        str(zebrad_binary),
        "--kresko-binary",
        str(kresko_binary),
        "-d",
        str(fleet.dir),
    ]
    run(cmd)
    tune_payload_zebrad_configs(fleet.dir / "payload")
    append_tracing_env(fleet.dir / "payload" / "vars.sh")
    manifest = write_payload_manifest(fleet.dir / "payload" / "build", zebrad_binary, provenance)
    return {
        "ok": True,
        "stage": "payload",
        "fleet": fleet.name,
        "config": str(fleet.dir / "config.json"),
        "payload": str(fleet.dir / "payload"),
        "manifest": str(manifest),
        "trace_paths": TRACE_COLLECTION_PATHS,
        "fast_block_sync": fast_block_sync_payload(),
    }


def patch_zakura_fast_sync_defaults(root: Path) -> list[Path]:
    patches = [
        (
            root / "zebra-network" / "src" / "constants.rs",
            "pub const DEFAULT_PEERSET_INITIAL_TARGET_SIZE: usize = 25;",
            f"pub const DEFAULT_PEERSET_INITIAL_TARGET_SIZE: usize = {FAST_BLOCK_SYNC_PEER_TARGET};",
        ),
        (
            root / "zebrad" / "src" / "components" / "sync.rs",
            "download_concurrency_limit: 50,",
            f"download_concurrency_limit: {FAST_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT},",
        ),
    ]
    patched = []
    for path, old, new in patches:
        text = path.read_text(encoding="utf-8")
        if new in text:
            continue
        if old not in text:
            raise RuntimeError(f"cannot patch {path}: expected text not found")
        path.write_text(text.replace(old, new, 1), encoding="utf-8")
        patched.append(path)
    return patched


def tune_payload_zebrad_configs(payload_dir: Path) -> list[Path]:
    tuned = []
    for path in sorted(payload_dir.glob("*/zebrad.toml")):
        text = path.read_text(encoding="utf-8")
        next_text = set_toml_value(
            text,
            "network",
            "peerset_initial_target_size",
            str(FAST_BLOCK_SYNC_PEER_TARGET),
        )
        next_text = set_toml_value(
            next_text,
            "sync",
            "download_concurrency_limit",
            str(FAST_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT),
        )
        if next_text != text:
            path.write_text(next_text, encoding="utf-8")
            tuned.append(path)
    return tuned


def set_toml_value(text: str, section: str, key: str, value: str) -> str:
    lines = text.splitlines(keepends=True)
    current_section: str | None = None
    section_start: int | None = None
    insert_at: int | None = None

    for index, line in enumerate(lines):
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            if current_section == section and insert_at is None:
                insert_at = index
            current_section = stripped.strip("[]")
            if current_section == section:
                section_start = index
            continue
        if current_section == section and (
            stripped.startswith(f"{key} ") or stripped.startswith(f"{key}=")
        ):
            indent = line[: len(line) - len(line.lstrip())]
            ending = "\n" if line.endswith("\n") else ""
            lines[index] = f"{indent}{key} = {value}{ending}"
            return "".join(lines)

    if section_start is None:
        ending = "" if text.endswith("\n") or not text else "\n"
        return f"{text}{ending}[{section}]\n{key} = {value}\n"
    if insert_at is None:
        insert_at = len(lines)
    lines.insert(insert_at, f"{key} = {value}\n")
    return "".join(lines)


def fast_block_sync_payload() -> dict[str, int]:
    return {
        "peerset_initial_target_size": FAST_BLOCK_SYNC_PEER_TARGET,
        "download_concurrency_limit": FAST_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT,
    }


def write_public_config(fleet: Fleet) -> Path:
    assets = sorted(fleet.fleet_assets(), key=lambda item: item.get("name", ""))
    if not assets:
        raise RuntimeError("no fleet assets found; run the up action before payload")

    config = {
        "miners": [asset_to_instance(asset) for asset in assets],
        "chain_id": "mainnet-zakura",
        "experiment": fleet.name,
        "ssh_pub_key_path": fleet.ssh.get("public_key_path", ""),
        "ssh_key_name": fleet.ssh.get("key_name", ""),
        "ssh_key_path": fleet.ssh.get("key_path", ""),
        "provider": "digitalocean",
        "network_kind": "mainnet",
        "mining_mode": "generate",
        "equihash_params": "common",
        "local_genesis": None,
    }
    path = fleet.dir / "config.json"
    path.write_text(json.dumps(config, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return path


def asset_to_instance(asset: dict[str, Any]) -> dict[str, Any]:
    return {
        "node_type": "miner",
        "public_ip": asset.get("public_ip", ""),
        "private_ip": asset.get("private_ip", ""),
        "provider": asset.get("provider", "digitalocean"),
        "slug": asset.get("size", ""),
        "region": asset.get("region", ""),
        "name": asset.get("name", ""),
        "tags": list(asset.get("tags", [])),
        "tier": "full",
    }


def append_tracing_env(vars_path: Path, env: dict[str, str] | None = None) -> None:
    target = os.environ if env is None else env
    marker = "# Zakura tracing"
    text = vars_path.read_text(encoding="utf-8")
    if marker in text:
        return
    trace_filter = target.get("ZEBRA_TRACING__FILTER", DEFAULT_TRACE_FILTER)
    trace_log = target.get("ZEBRA_TRACING__LOG_FILE", DEFAULT_TRACE_LOG_FILE)
    with vars_path.open("a", encoding="utf-8") as handle:
        handle.write(
            "\n"
            f"{marker}\n"
            f"export ZEBRA_TRACING__FILTER=\"${{ZEBRA_TRACING__FILTER:-{trace_filter}}}\"\n"
            f"export ZEBRA_TRACING__LOG_FILE=\"${{ZEBRA_TRACING__LOG_FILE:-{trace_log}}}\"\n"
        )


def write_payload_manifest(
    build_dir: Path,
    zebrad_binary: Path,
    provenance: dict[str, Any] | None,
) -> Path:
    manifest = build_dir / "manifest.txt"
    fields = {
        "zebrad_source": str((provenance or {}).get("repo", zebrad_binary.parent)),
        "zebrad_ref": str((provenance or {}).get("ref", "")),
        "zebrad_commit": str((provenance or {}).get("commit", "")),
        "zebrad_sha256": sha256_file(build_dir / "zebrad"),
        "zebrad_build_command": str((provenance or {}).get("build_command", "")),
    }
    kresko_binary = build_dir / "kresko"
    if kresko_binary.exists():
        fields["kresko_source"] = str(repo_root())
        fields["kresko_sha256"] = sha256_file(kresko_binary)
    manifest.write_text(
        "".join(f"{key}={value}\n" for key, value in sorted(fields.items())),
        encoding="utf-8",
    )
    return manifest


def plan_payload(fleet: Fleet) -> dict[str, Any]:
    desired = fleet._desired()
    return {
        "ok": True,
        "stage": "plan",
        "fleet": fleet.name,
        "nodes": [
            {
                "name": node.name,
                "role": node.role,
                "provider": node.provider,
                "region": node.region,
                "size": node.size,
                "image": node.image,
                "tags": node.tags,
            }
            for node in desired
        ],
        "zakura_root": os.environ.get("ZAKURA_ROOT", DEFAULT_ZAKURA_ROOT),
        "zakura_ref": os.environ.get("ZAKURA_REF", PR17_HEAD_SHA),
        "trace_paths": TRACE_COLLECTION_PATHS,
        "fast_block_sync": fast_block_sync_payload(),
    }


def find_kresko_binary() -> Path:
    if os.environ.get("KRESKO_BINARY"):
        return Path(os.environ["KRESKO_BINARY"]).expanduser()
    root = repo_root()
    for candidate in (root / "target" / "release" / "kresko", root / "target" / "debug" / "kresko"):
        if candidate.exists():
            return candidate
    found = shutil.which("kresko")
    return Path(found) if found else Path("kresko")


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def run(cmd: list[str], *, cwd: Path | None = None) -> None:
    subprocess.run(cmd, cwd=str(cwd) if cwd else None, check=True)


def capture(cmd: list[str]) -> str:
    return subprocess.check_output(cmd, text=True).strip()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(64 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def print_result(payload: dict[str, Any]) -> int:
    print(json.dumps(payload, indent=2, sort_keys=True, default=str))
    return 0 if payload.get("ok", False) else 1


if __name__ == "__main__":
    raise SystemExit(main())
