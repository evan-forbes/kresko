#!/usr/bin/env python3
"""Seven-node mainnet Zebra fleet bootstrapped from a verified state snapshot."""

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
from kresko.env import load_experiment_env
from kresko.fleet import TRACE_COLLECTION_PATHS


DEFAULT_FLEET = "mainnet-zebra-snapshot"
DEFAULT_SIZE = "so1_5-4vcpu-32gb"
DEFAULT_IMAGE = "ubuntu-24-04-x64"
DEFAULT_ZEBRA_ROOT = "/home/evan/src/valar/zebra"
DEFAULT_SNAPSHOT_ARCHIVE = "zebra-mainnet-20260612T230854Z-3375822.tar.zst"
DEFAULT_SNAPSHOT_SHA256 = "834f8e41c129f90f6c6ce4fd964704cbeeaca2dde87ae9023854645e3a407440"
DEFAULT_SNAPSHOT_URLS = [
    "https://zebra-snapshots.nyc3.cdn.digitaloceanspaces.com/mainnet/"
    "zebra-mainnet-20260612T230854Z-3375822.tar.zst",
    "https://zebra-snapshots-ams3.ams3.digitaloceanspaces.com/mainnet/"
    "zebra-mainnet-20260612T230854Z-3375822.tar.zst",
    "https://zebra-snapshots-sgp1.sgp1.digitaloceanspaces.com/mainnet/"
    "zebra-mainnet-20260612T230854Z-3375822.tar.zst",
]
DEFAULT_NODE_SPECS = [
    ("us-east", "nyc3", "so1_5-4vcpu-32gb-intel"),
    ("us-west", "sfo3", "so1_5-4vcpu-32gb-intel"),
    ("canada", "tor1", "so1_5-4vcpu-32gb-intel"),
    ("europe-west", "lon1", "so1_5-4vcpu-32gb"),
    ("europe-central", "ams3", "so1_5-4vcpu-32gb-intel"),
    ("asia-south", "blr1", "so1_5-4vcpu-32gb"),
    ("asia-pacific", "syd1", "so1_5-4vcpu-32gb"),
]
DEFAULT_TRACE_FILTER = "info,zebrad=debug,zebra_network=debug,zebra_state=debug,zebra_rpc=debug"
DEFAULT_TRACE_LOG_FILE = "/root/logs/zebra-tracing.log"
FAST_BLOCK_SYNC_PEER_TARGET = 100
FAST_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT = 100
ZAKURA_P2P_PORT = 8234
ZAKURA_MESSAGE_RATE_PER_SECOND = 4000
ZAKURA_ONLY_NODE_NAMES = {"asia-pacific-0", "europe-central-0"}
ZAKURA_NODE_IDENTITIES = {
    "us-east-0": (
        "3761696e6e65742d7a656272612d736e617073686f743a75732d656173742d95",
        "9ec67ad6834bc2ca0d659c240e042d3446c37cabcc092b527d459c87d938b4a4",
    ),
    "us-west-0": (
        "24727a7f7f76853e8b767383723e847f7281847980854b86843e887684853ee4",
        "bd3dc5d2a3d44c6bf90e364bf446231dbf9737e38a562ccf9e91ea631ea59b22",
    ),
    "canada-0": (
        "d5838b909087964f9c878494834f95908392958a91965c8583908386834f52a5",
        "14ab98fa0c4b07d40119e1dbc9f3c36d20c8f226ae5ba4216218a2034f148e57",
    ),
    "europe-west-0": (
        "5c33fcc2a198a760ad9895a59460a6a194a3a69ba2a76d98a8a5a2a39860aa3d",
        "681d21b18644cd82ec13256a97f92bec1fff815683ef6f65dc7c993f098a4fe5",
    ),
    "europe-central-0": (
        "591d1b1702d8cc71bea9a6b6a571b7b2a5b4b7acb3b87ea9b9b6b3b4a971a70c",
        "058b3f20dc9bef7bb447f94d7663d793cfbc036720f97e52d7f13661b21818e1",
    ),
    "asia-south-0": (
        "25343bc3c3bac982cfbab7c7b682c8c3b6c5c8bdc4c98fb6c8beb682c8c4ca6c",
        "291323d78eb7186c3fa225ef5e305e95363e0ef06d42dca91bd4ef0254aed1ae",
    ),
    "asia-pacific-0": (
        "4508064742cbda93e0cbc8d8c793d9d4c7d6d9ced5daa0c7d9cfc793d6c7c96a",
        "85e425233a68697d4be91dd5d542305a8a327cd06d992d53c0913cef2fa75084",
    ),
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Operate the snapshot-bootstrapped mainnet Zebra fleet")
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
    parser.add_argument("--zebrad-binary", help="prebuilt Zebra zebrad binary for payload generation")
    parser.add_argument("--skip-build", action="store_true", help="payload action: do not build Zebra")
    args = parser.parse_args(argv)

    load_default_env()
    require_valar_do_token()
    fleet = make_fleet()

    if args.action == "plan":
        return print_result(plan_payload(fleet))
    if args.action == "up":
        return print_result(fleet.up(dry_run=args.dry_run))
    if args.action == "build":
        return print_result(build_zebra(fleet))
    if args.action == "payload":
        zebrad_binary = args.zebrad_binary
        provenance: dict[str, Any] = {}
        if not zebrad_binary and not args.skip_build:
            provenance = build_zebra(fleet)
            zebrad_binary = provenance["binary"]
        if not zebrad_binary:
            raise SystemExit("--zebrad-binary is required with --skip-build")
        return print_result(generate_payload(fleet, Path(zebrad_binary), provenance=provenance))
    if args.action == "deploy":
        return print_result(fleet.deploy(str(fleet.dir / "payload"), dry_run=args.dry_run))
    if args.action == "start":
        return print_result(
            fleet.run(
                "bash /root/kresko/payload/node_init.sh",
                background="zebra",
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
            "VALAR_DO_TOKEN is not set. This mainnet fleet uses the valar "
            "DigitalOcean account and will not fall back to DIGITALOCEAN_TOKEN."
        )
    target["DIGITALOCEAN_TOKEN"] = valar


def make_fleet(env: dict[str, str] | None = None) -> Fleet:
    target = os.environ if env is None else env
    if env is None:
        load_default_env()
    require_valar_do_token(target)

    ssh = {
        "key_name": target.get("KRESKO_SSH_KEY_NAME", ""),
        "key_path": target.get("KRESKO_SSH_KEY_PATH", ""),
    }
    fleet = Fleet(target.get("KRESKO_FLEET", DEFAULT_FLEET), tags=["zebra", "mainnet"], ssh=ssh)
    for prefix, region, default_size in node_specs(target):
        fleet.add(
            "node",
            count=1,
            name_prefix=prefix,
            provider=DigitalOcean(
                region=region,
                size=target.get("KRESKO_DO_SIZE", default_size),
                image=target.get("KRESKO_DO_IMAGE", DEFAULT_IMAGE),
                tags=["zebra", "mainnet", "snapshot"],
            ),
        )
    return fleet


def load_default_env() -> None:
    load_experiment_env(experiment_root=repo_root())


def node_specs(env: dict[str, str] | None = None) -> list[tuple[str, str, str]]:
    target = os.environ if env is None else env
    raw = target.get("KRESKO_ZEBRA_SNAPSHOT_REGIONS", "")
    if not raw:
        return list(DEFAULT_NODE_SPECS)
    specs: list[tuple[str, str, str]] = []
    for item in [part.strip() for part in raw.split(",") if part.strip()]:
        if ":" not in item:
            raise ValueError(
                "KRESKO_ZEBRA_SNAPSHOT_REGIONS entries must be prefix:region[:size]"
            )
        parts = [part.strip() for part in item.split(":")]
        if len(parts) not in (2, 3):
            raise ValueError(
                "KRESKO_ZEBRA_SNAPSHOT_REGIONS entries must be prefix:region[:size]"
            )
        prefix, region = parts[:2]
        size = parts[2] if len(parts) == 3 else target.get("KRESKO_DO_SIZE", DEFAULT_SIZE)
        if not prefix or not region:
            raise ValueError(
                "KRESKO_ZEBRA_SNAPSHOT_REGIONS entries must be prefix:region[:size]"
            )
        specs.append((prefix, region, size))
    if len(specs) != 7:
        raise ValueError("KRESKO_ZEBRA_SNAPSHOT_REGIONS must contain exactly seven entries")
    return specs


def build_zebra(fleet: Fleet, env: dict[str, str] | None = None) -> dict[str, Any]:
    target = os.environ if env is None else env
    root = Path(target.get("ZEBRA_ROOT", DEFAULT_ZEBRA_ROOT)).expanduser()
    build_command = ["cargo", "xtask", "package", "ubuntu"]

    run(build_command, cwd=root)
    commit = capture(["git", "-C", str(root), "rev-parse", "HEAD"])
    binary = root / "target" / "ubuntu" / "zebra"
    if not binary.exists():
        raise FileNotFoundError(binary)

    provenance = {
        "repo": str(root),
        "commit": commit,
        "binary": str(binary),
        "binary_sha256": sha256_file(binary),
        "build_command": " ".join(build_command),
        "fast_block_sync_peer_target": FAST_BLOCK_SYNC_PEER_TARGET,
        "fast_block_sync_download_concurrency_limit": FAST_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT,
    }
    (fleet.dir / "zebra-build.json").write_text(
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
    node_kresko_binary = find_node_kresko_binary()
    cmd = [
        str(kresko_binary),
        "genesis-public",
        "--zebrad-binary",
        str(zebrad_binary),
        "--kresko-binary",
        str(node_kresko_binary),
        "-d",
        str(fleet.dir),
    ]
    run(cmd)
    shutil.copyfile(
        repo_root() / "scripts" / "node_init_public.sh",
        fleet.dir / "payload" / "node_init.sh",
    )
    tune_payload_zebrad_configs(fleet.dir / "payload", fleet_assets=fleet.fleet_assets())
    append_snapshot_env(fleet.dir / "payload" / "vars.sh")
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
        "snapshot_archive": snapshot_archive(),
        "snapshot_sha256": snapshot_sha256(),
        "snapshot_urls": snapshot_urls(),
        "fast_block_sync": fast_block_sync_payload(),
    }


def write_public_config(fleet: Fleet) -> Path:
    assets = sorted(fleet.fleet_assets(), key=lambda item: item.get("name", ""))
    if not assets:
        raise RuntimeError("no fleet assets found; run the up action before payload")

    config = {
        "miners": [asset_to_instance(asset) for asset in assets],
        "chain_id": "mainnet-zebra-snapshot",
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


def append_snapshot_env(vars_path: Path, env: dict[str, str] | None = None) -> None:
    marker = "# Zebra mainnet state snapshot"
    text = vars_path.read_text(encoding="utf-8")
    if marker in text:
        return
    with vars_path.open("a", encoding="utf-8") as handle:
        handle.write(
            "\n"
            f"{marker}\n"
            f"export KRESKO_STATE_SNAPSHOT_ARCHIVE=\"{snapshot_archive(env)}\"\n"
            f"export KRESKO_STATE_SNAPSHOT_SHA256=\"{snapshot_sha256(env)}\"\n"
            f"export KRESKO_STATE_SNAPSHOT_URLS=\"{snapshot_urls_shell(env)}\"\n"
        )


def append_tracing_env(vars_path: Path, env: dict[str, str] | None = None) -> None:
    target = os.environ if env is None else env
    marker = "# Zebra tracing"
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


def tune_payload_zebrad_configs(
    payload_dir: Path,
    *,
    fleet_assets: list[dict[str, Any]] | None = None,
) -> list[Path]:
    assets = sorted(fleet_assets or [], key=lambda item: item.get("name", ""))
    node_by_payload_dir = {
        payload_dir_name(asset["name"]): asset
        for asset in assets
        if asset.get("name") and asset.get("public_ip")
    }
    zakura_bootstrap = {
        asset["name"]: (
            f"{ZAKURA_NODE_IDENTITIES[asset['name']][1]}@"
            f"{asset['public_ip']}:{ZAKURA_P2P_PORT}"
        )
        for asset in assets
        if asset.get("name") in ZAKURA_NODE_IDENTITIES and asset.get("public_ip")
    }

    tuned = []
    for path in sorted(payload_dir.glob("*/zebrad.toml")):
        asset = node_by_payload_dir.get(path.parent.name)
        if not asset:
            raise RuntimeError(f"no fleet asset found for payload directory {path.parent.name}")
        node_name = asset["name"]
        if node_name not in ZAKURA_NODE_IDENTITIES:
            raise RuntimeError(f"no Zakura node identity configured for {node_name}")

        bootstrap_peers = [
            peer
            for peer_name, peer in sorted(zakura_bootstrap.items())
            if peer_name != node_name
        ]
        text = path.read_text(encoding="utf-8")
        next_text = set_toml_value(
            text,
            "network",
            "peerset_initial_target_size",
            str(FAST_BLOCK_SYNC_PEER_TARGET),
        )
        zakura_only = node_name in ZAKURA_ONLY_NODE_NAMES
        if zakura_only:
            next_text = set_toml_array(next_text, "network", "initial_mainnet_peers", [])
        next_text = set_toml_value(next_text, "network", "v2_p2p", "true")
        next_text = set_toml_value(
            next_text,
            "network",
            "legacy_p2p",
            "false" if zakura_only else "true",
        )
        next_text = set_toml_value(
            next_text,
            "network",
            "zakura_node_secret_key",
            f'"{ZAKURA_NODE_IDENTITIES[node_name][0]}"',
        )
        next_text = set_toml_value(
            next_text,
            "network.zakura",
            "listen_addr",
            f'"0.0.0.0:{ZAKURA_P2P_PORT}"',
        )
        next_text = set_toml_value(
            next_text,
            "network.zakura",
            "trace_dir",
            '"/root/traces/zakura"',
        )
        next_text = set_toml_value(
            next_text,
            "network.zakura",
            "message_rate_per_second",
            str(ZAKURA_MESSAGE_RATE_PER_SECOND),
        )
        next_text = set_toml_array(
            next_text,
            "network.zakura",
            "bootstrap_peers",
            bootstrap_peers,
        )
        if zakura_only:
            next_text = set_toml_value(
                next_text,
                "network.zakura.block_sync",
                "replace_legacy_syncer",
                "true",
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


def payload_dir_name(node_name: str) -> str:
    return "-".join(node_name.split("-")[:2])


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


def set_toml_array(text: str, section: str, key: str, values: list[str]) -> str:
    rendered = [f"{key} = [\n"]
    rendered.extend(f'    "{value}",\n' for value in values)
    rendered.append("]\n")
    return set_toml_block(text, section, key, rendered)


def set_toml_block(text: str, section: str, key: str, block: list[str]) -> str:
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
            end = index + 1
            if stripped.endswith("["):
                while end < len(lines) and lines[end].strip() != "]":
                    end += 1
                if end < len(lines):
                    end += 1
            lines[index:end] = block
            return "".join(lines)

    if section_start is None:
        ending = "" if text.endswith("\n") or not text else "\n"
        return f"{text}{ending}[{section}]\n{''.join(block)}"
    if insert_at is None:
        insert_at = len(lines)
    lines[insert_at:insert_at] = block
    return "".join(lines)


def write_payload_manifest(
    build_dir: Path,
    zebrad_binary: Path,
    provenance: dict[str, Any] | None,
) -> Path:
    manifest = build_dir / "manifest.txt"
    fields = {
        "zebrad_source": str((provenance or {}).get("repo", zebrad_binary.parent)),
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
        "zebra_root": os.environ.get("ZEBRA_ROOT", DEFAULT_ZEBRA_ROOT),
        "trace_paths": TRACE_COLLECTION_PATHS,
        "snapshot_archive": snapshot_archive(),
        "snapshot_sha256": snapshot_sha256(),
        "snapshot_urls": snapshot_urls(),
        "fast_block_sync": fast_block_sync_payload(),
    }


def fast_block_sync_payload() -> dict[str, int]:
    return {
        "peerset_initial_target_size": FAST_BLOCK_SYNC_PEER_TARGET,
        "download_concurrency_limit": FAST_BLOCK_SYNC_DOWNLOAD_CONCURRENCY_LIMIT,
    }


def snapshot_archive(env: dict[str, str] | None = None) -> str:
    target = os.environ if env is None else env
    return target.get("KRESKO_STATE_SNAPSHOT_ARCHIVE", DEFAULT_SNAPSHOT_ARCHIVE)


def snapshot_sha256(env: dict[str, str] | None = None) -> str:
    target = os.environ if env is None else env
    return target.get("KRESKO_STATE_SNAPSHOT_SHA256", DEFAULT_SNAPSHOT_SHA256)


def snapshot_urls(env: dict[str, str] | None = None) -> list[str]:
    target = os.environ if env is None else env
    raw = target.get("KRESKO_STATE_SNAPSHOT_URLS", "")
    if raw:
        return [url.strip() for url in raw.split() if url.strip()]
    return list(DEFAULT_SNAPSHOT_URLS)


def snapshot_urls_shell(env: dict[str, str] | None = None) -> str:
    return " ".join(snapshot_urls(env))


def find_kresko_binary() -> Path:
    if os.environ.get("KRESKO_BINARY"):
        return Path(os.environ["KRESKO_BINARY"]).expanduser()
    root = repo_root()
    for candidate in (root / "target" / "release" / "kresko", root / "target" / "debug" / "kresko"):
        if candidate.exists():
            return candidate
    found = shutil.which("kresko")
    return Path(found) if found else Path("kresko")


def find_node_kresko_binary() -> Path:
    """kresko binary shipped to (and run on) the Ubuntu nodes.

    ``kresko genesis-public`` copies ``--kresko-binary`` into the payload that
    ``node_init_public.sh`` installs and runs on each node, so it must match the
    node OS regardless of the operator's host. When the operator host differs
    from the nodes (e.g. a macOS operator -> Ubuntu nodes), this must be the
    Linux build (``make ubuntu`` -> target/ubuntu/kresko), not the host binary
    that runs ``genesis-public`` locally.

    Resolution: ``KRESKO_NODE_BINARY`` -> ``target/ubuntu/kresko`` -> the host
    binary (operator OS == node OS, the common all-Linux case, behaviour
    unchanged).
    """
    if os.environ.get("KRESKO_NODE_BINARY"):
        return Path(os.environ["KRESKO_NODE_BINARY"]).expanduser()
    ubuntu = repo_root() / "target" / "ubuntu" / "kresko"
    if ubuntu.exists():
        return ubuntu
    return find_kresko_binary()


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
