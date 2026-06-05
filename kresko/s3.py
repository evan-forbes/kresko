"""Operator-side S3 upload + presigned-URL generation via the `aws` CLI.

We deliver payloads to nodes by uploading to S3 and handing the node a
short-lived presigned URL to `curl` — never scp/rsync. This module is the
operator-side helper: it shells out to the `aws` CLI (already required for the
rest of the S3 workflow) so the kresko doesn't take a boto3 dependency.

Config is read from the environment, which `load_experiment_env` populates
from the repo/experiment `.env` (and `scripts/vars.sh` documents the keys):

    AWS_S3_BUCKET     required — target bucket
    AWS_S3_ENDPOINT   optional — custom endpoint (DO Spaces, R2, MinIO, …)
    AWS_DEFAULT_REGION / AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY
                      consumed by the `aws` CLI directly

The presigned URL is generated locally and is the only thing that crosses the
wire to the node, so credentials never leave the operator's machine.
"""

from __future__ import annotations

import os
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Mapping

# A runner takes an argv list and returns a finished process. Injected in tests
# so command construction can be asserted without shelling out to `aws`.
Runner = Callable[[list[str]], "subprocess.CompletedProcess[str]"]

__all__ = [
    "S3Config",
    "S3Error",
    "presign",
    "upload",
    "upload_and_presign",
]


class S3Error(RuntimeError):
    """Raised when S3 is misconfigured or an `aws` invocation fails."""


def _default_runner(cmd: list[str]) -> "subprocess.CompletedProcess[str]":
    return subprocess.run(cmd, text=True, capture_output=True, check=False)


@dataclass(frozen=True)
class S3Config:
    bucket: str
    endpoint: str = ""
    region: str = ""

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None) -> "S3Config":
        env = env if env is not None else os.environ
        bucket = (env.get("AWS_S3_BUCKET") or "").strip()
        if not bucket:
            raise S3Error(
                "AWS_S3_BUCKET is not set; S3 delivery requires a bucket. "
                "Set AWS_S3_BUCKET (and AWS creds / AWS_S3_ENDPOINT as needed) "
                "in your repo or experiment .env."
            )
        return cls(
            bucket=bucket,
            endpoint=(env.get("AWS_S3_ENDPOINT") or "").strip(),
            region=(env.get("AWS_DEFAULT_REGION") or "").strip(),
        )

    def uri(self, key: str) -> str:
        return f"s3://{self.bucket}/{key.lstrip('/')}"

    def endpoint_args(self) -> list[str]:
        return ["--endpoint-url", self.endpoint] if self.endpoint else []


def upload(
    local_path: str | Path,
    key: str,
    *,
    config: S3Config | None = None,
    runner: Runner = _default_runner,
) -> str:
    """Upload `local_path` to `s3://<bucket>/<key>`; return the s3:// URI."""
    config = config or S3Config.from_env()
    cmd = [
        "aws",
        "s3",
        "cp",
        str(local_path),
        config.uri(key),
        *config.endpoint_args(),
    ]
    result = runner(cmd)
    if result.returncode != 0:
        raise S3Error(
            f"`aws s3 cp` failed ({result.returncode}): {(result.stderr or '').strip()}"
        )
    return config.uri(key)


def presign(
    key: str,
    *,
    expires: int = 3600,
    config: S3Config | None = None,
    runner: Runner = _default_runner,
) -> str:
    """Return a presigned GET URL for `s3://<bucket>/<key>` valid `expires` seconds."""
    config = config or S3Config.from_env()
    cmd = [
        "aws",
        "s3",
        "presign",
        config.uri(key),
        "--expires-in",
        str(expires),
        *config.endpoint_args(),
    ]
    result = runner(cmd)
    if result.returncode != 0:
        raise S3Error(
            f"`aws s3 presign` failed ({result.returncode}): {(result.stderr or '').strip()}"
        )
    url = (result.stdout or "").strip()
    if not url:
        raise S3Error("`aws s3 presign` returned an empty URL")
    return url


def upload_and_presign(
    local_path: str | Path,
    key: str,
    *,
    expires: int = 3600,
    config: S3Config | None = None,
    runner: Runner = _default_runner,
) -> str:
    """Upload `local_path` then return a presigned GET URL for it."""
    config = config or S3Config.from_env()
    upload(local_path, key, config=config, runner=runner)
    return presign(key, expires=expires, config=config, runner=runner)
