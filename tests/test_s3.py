from __future__ import annotations

import subprocess

import pytest

from harness import s3


class RecordingRunner:
    """Records argv lists and replays canned results, keyed by the `aws` verb."""

    def __init__(self, results: dict[str, subprocess.CompletedProcess[str]]) -> None:
        self.results = results
        self.calls: list[list[str]] = []

    def __call__(self, cmd: list[str]) -> subprocess.CompletedProcess[str]:
        self.calls.append(list(cmd))
        verb = cmd[2] if len(cmd) > 2 else ""
        return self.results[verb]


def ok(stdout: str = "") -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(["aws"], 0, stdout, "")


def fail(stderr: str) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(["aws"], 1, "", stderr)


def test_from_env_requires_bucket():
    with pytest.raises(s3.S3Error):
        s3.S3Config.from_env(env={})


def test_from_env_reads_bucket_endpoint_region():
    config = s3.S3Config.from_env(
        env={
            "AWS_S3_BUCKET": "kresko",
            "AWS_S3_ENDPOINT": "https://nyc3.digitaloceanspaces.com",
            "AWS_DEFAULT_REGION": "nyc3",
        }
    )
    assert config.bucket == "kresko"
    assert config.endpoint == "https://nyc3.digitaloceanspaces.com"
    assert config.region == "nyc3"
    assert config.uri("explorer/x.tar.gz") == "s3://kresko/explorer/x.tar.gz"


def test_upload_builds_cp_command_with_endpoint():
    config = s3.S3Config(bucket="kresko", endpoint="https://ep.example")
    runner = RecordingRunner({"cp": ok()})

    uri = s3.upload("/tmp/x.tar.gz", "explorer/x.tar.gz", config=config, runner=runner)

    assert uri == "s3://kresko/explorer/x.tar.gz"
    assert runner.calls == [
        [
            "aws",
            "s3",
            "cp",
            "/tmp/x.tar.gz",
            "s3://kresko/explorer/x.tar.gz",
            "--endpoint-url",
            "https://ep.example",
        ]
    ]


def test_upload_without_endpoint_omits_flag():
    config = s3.S3Config(bucket="kresko")
    runner = RecordingRunner({"cp": ok()})

    s3.upload("/tmp/x.tar.gz", "k", config=config, runner=runner)

    assert "--endpoint-url" not in runner.calls[0]


def test_upload_raises_on_failure():
    config = s3.S3Config(bucket="kresko")
    runner = RecordingRunner({"cp": fail("Access Denied")})

    with pytest.raises(s3.S3Error, match="Access Denied"):
        s3.upload("/tmp/x.tar.gz", "k", config=config, runner=runner)


def test_presign_returns_url():
    config = s3.S3Config(bucket="kresko")
    runner = RecordingRunner({"presign": ok("https://kresko.example/k?sig=abc\n")})

    url = s3.presign("k", expires=600, config=config, runner=runner)

    assert url == "https://kresko.example/k?sig=abc"
    assert runner.calls[0] == [
        "aws",
        "s3",
        "presign",
        "s3://kresko/k",
        "--expires-in",
        "600",
    ]


def test_presign_raises_on_empty_url():
    config = s3.S3Config(bucket="kresko")
    runner = RecordingRunner({"presign": ok("   \n")})

    with pytest.raises(s3.S3Error, match="empty URL"):
        s3.presign("k", config=config, runner=runner)


def test_upload_and_presign_does_both():
    config = s3.S3Config(bucket="kresko")
    runner = RecordingRunner(
        {"cp": ok(), "presign": ok("https://kresko.example/k?sig=abc")}
    )

    url = s3.upload_and_presign("/tmp/x.tar.gz", "k", config=config, runner=runner)

    assert url == "https://kresko.example/k?sig=abc"
    assert [c[2] for c in runner.calls] == ["cp", "presign"]
