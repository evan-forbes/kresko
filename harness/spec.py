from __future__ import annotations

import importlib.util
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from harness.providers import experiment_tag, role_tag


@dataclass(frozen=True)
class NodeGroup:
    role: str
    count: int
    region: str
    size: str
    image: str = "ubuntu-24-04-x64"
    provider: str = "digitalocean"
    name_prefix: str | None = None
    tags: list[str] = field(default_factory=list)
    ssh_user: str | None = None
    provider_options: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class ExperimentSpec:
    name: str
    tags: list[str] = field(default_factory=list)
    ssh: dict[str, Any] = field(default_factory=dict)
    node_groups: list[NodeGroup] = field(default_factory=list)
    payload_paths: list[str] = field(default_factory=list)

    def expanded_nodes(self) -> list[dict[str, Any]]:
        nodes: list[dict[str, Any]] = []
        base_tags = ["kresko", experiment_tag(self.name), *self.tags]
        for group in self.node_groups:
            prefix = group.name_prefix or group.role
            tags = sorted(set([*base_tags, *group.tags, role_tag(group.role)]))
            for index in range(group.count):
                nodes.append(
                    {
                        "name": f"{prefix}-{index}",
                        "role": group.role,
                        "provider": group.provider,
                        "provider_id": "",
                        "region": group.region,
                        "size": group.size,
                        "image": group.image,
                        "public_ip": "",
                        "private_ip": "",
                        "ssh_user": group.ssh_user or self.ssh.get("user", "root"),
                        "provider_options": group.provider_options,
                        "tags": tags,
                        "status": "pending",
                    }
                )
        return nodes


def load_spec(path: str | Path = "spec.py") -> ExperimentSpec:
    path = Path(path)
    module_name = f"kresko_experiment_spec_{abs(hash(path.resolve()))}"
    spec = importlib.util.spec_from_file_location(module_name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"failed to load spec from {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    experiment = getattr(module, "EXPERIMENT", None) or getattr(module, "experiment", None)
    if not isinstance(experiment, ExperimentSpec):
        raise TypeError(f"{path} must define EXPERIMENT = ExperimentSpec(...)")
    return experiment
