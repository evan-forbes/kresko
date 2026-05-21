#!/bin/bash
#
# Build Ubuntu 22.04-compatible binaries for zebrad and kresko using Docker.
#
# Usage:
#   ./scripts/build-ubuntu.sh                         # Build both zebra and kresko
#   ./scripts/build-ubuntu.sh --zebrad-only           # Build only zebra
#   ./scripts/build-ubuntu.sh --kresko-only           # Build only kresko
#   ./scripts/build-ubuntu.sh --output-dir build/ubuntu
#
# Prerequisites:
#   - Docker with BuildKit support
#   - NU7 Zebra repo at ../nu7-testnet, or set NU7_ZEBRA_ROOT
#   - Zebra repo at ../zebra for zebra-jsonl-trace, or set ZEBRA_ROOT
#
# Output:
#   target/ubuntu/zebra     - Ubuntu 22.04-compatible zebra binary
#   target/ubuntu/kresko    - Ubuntu 22.04-compatible kresko binary
#
set -euo pipefail

usage() {
    cat <<'USAGE'
Build Ubuntu 22.04-compatible binaries for zebra and kresko using Docker.

Usage:
  ./scripts/build-ubuntu.sh
  ./scripts/build-ubuntu.sh --zebrad-only
  ./scripts/build-ubuntu.sh --kresko-only
  ./scripts/build-ubuntu.sh --output-dir build/ubuntu

Environment:
  NU7_ZEBRA_ROOT  NU7 Zebra worktree, defaults to ../nu7-testnet
  ZEBRA_ROOT      Zebra worktree used for zebra-jsonl-trace, defaults to ../zebra

Output:
  target/ubuntu/zebra
  target/ubuntu/kresko
USAGE
}

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
KRESKO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
NU7_ZEBRA_ROOT="$(cd "${NU7_ZEBRA_ROOT:-$KRESKO_ROOT/../nu7-testnet}" && pwd)"
ZEBRA_ROOT="$(cd "${ZEBRA_ROOT:-$KRESKO_ROOT/../zebra}" && pwd)"

if [ -L "$ZEBRA_ROOT/zebra-jsonl-trace" ]; then
    resolved_trace="$(readlink -f "$ZEBRA_ROOT/zebra-jsonl-trace")"
    resolved_zebra_root="$(cd "$(dirname "$resolved_trace")" && pwd)"
    if [ "$resolved_zebra_root" != "$ZEBRA_ROOT" ] && [ -f "$resolved_zebra_root/Cargo.toml" ]; then
        echo "Note: zebra-jsonl-trace is a symlink outside ZEBRA_ROOT; using $resolved_zebra_root for Docker."
        ZEBRA_ROOT="$resolved_zebra_root"
    fi
fi

if [ ! -f "$ZEBRA_ROOT/zebra-jsonl-trace/Cargo.toml" ]; then
    echo "Error: missing zebra-jsonl-trace at $ZEBRA_ROOT/zebra-jsonl-trace" >&2
    echo "Set ZEBRA_ROOT to a Zebra worktree containing zebra-jsonl-trace." >&2
    exit 1
fi

BUILD_ZEBRAD=true
BUILD_KRESKO=true
OUTPUT_DIR="$KRESKO_ROOT/target/ubuntu"

while [ "$#" -gt 0 ]; do
    case "$1" in
        --zebrad-only|--zebra-only)
            BUILD_KRESKO=false
            ;;
        --kresko-only)
            BUILD_ZEBRAD=false
            ;;
        --output-dir)
            if [ "$#" -lt 2 ]; then
                echo "Error: --output-dir requires a path" >&2
                exit 2
            fi
            OUTPUT_DIR="$2"
            shift
            ;;
        --output-dir=*)
            OUTPUT_DIR="${1#*=}"
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Error: unknown argument: $1" >&2
            exit 2
            ;;
    esac
    shift
done

if [ "$OUTPUT_DIR" != "${OUTPUT_DIR#/}" ]; then
    OUTPUT_DIR="$(mkdir -p "$OUTPUT_DIR" && cd "$OUTPUT_DIR" && pwd)"
else
    OUTPUT_DIR="$(mkdir -p "$KRESKO_ROOT/$OUTPUT_DIR" && cd "$KRESKO_ROOT/$OUTPUT_DIR" && pwd)"
fi

if ! command -v docker &>/dev/null; then
    echo "Error: docker is not installed."
    echo "Install Docker: https://docs.docker.com/engine/install/"
    echo "  or on Arch: sudo pacman -S docker && sudo systemctl start docker"
    exit 1
fi

IMAGE_NAME="kresko-builder"
IMAGE_TAG="ubuntu2204"
FULL_IMAGE="${IMAGE_NAME}:${IMAGE_TAG}"

mkdir -p "$OUTPUT_DIR"

if [ "$BUILD_ZEBRAD" = true ]; then
    echo "=== Building zebra with NU7 Zebra Dockerfile ==="
    tmp_output="$(mktemp -d)"
    DOCKER_BUILDKIT=1 docker build \
        -f "$NU7_ZEBRA_ROOT/docker/ubuntu-package.Dockerfile" \
        --output "type=local,dest=$tmp_output" \
        "$NU7_ZEBRA_ROOT"
    install -m 755 "$tmp_output/zebra" "$OUTPUT_DIR/zebra"
    rm -rf "$tmp_output"
    echo "Built: $OUTPUT_DIR/zebra"
    file "$OUTPUT_DIR/zebra"
fi

if [ "$BUILD_KRESKO" = true ]; then
    echo "=== Building Kresko Docker image (cached) ==="
    docker build \
        -t "$FULL_IMAGE" \
        -f "$KRESKO_ROOT/docker/ubuntu-builder.Dockerfile" \
        "$KRESKO_ROOT"

    echo "=== Building kresko ==="
    docker run --rm \
        -e CARGO_TARGET_DIR=/tmp/kresko-target \
        -e CXXFLAGS="-include cstdint" \
        -e RUSTFLAGS='--cfg zcash_unstable="nu7" --cfg zcash_unstable="zip235"' \
        -e HOST_UID="$(id -u)" \
        -e HOST_GID="$(id -g)" \
        -v "$KRESKO_ROOT:/workspace/kresko:ro" \
        -v "$NU7_ZEBRA_ROOT:/workspace/nu7-testnet:ro" \
        -v "$ZEBRA_ROOT:/workspace/zebra:ro" \
        -v "$OUTPUT_DIR:/output" \
        -v "kresko-cargo-registry:/root/.cargo/registry" \
        -v "kresko-cargo-git:/root/.cargo/git" \
        -v "kresko-target:/tmp/kresko-target" \
        "$FULL_IMAGE" \
        bash -eu -c '
            cd /workspace/kresko
            cargo build --locked --release --bin kresko
            install -m 755 "$CARGO_TARGET_DIR/release/kresko" /output/kresko
            chown "$HOST_UID:$HOST_GID" /output/kresko
            echo "=== kresko built successfully ==="
        '
    echo "Built: $OUTPUT_DIR/kresko"
    file "$OUTPUT_DIR/kresko"
fi

echo "=== Done ==="
echo "Ubuntu 22.04-compatible binaries in: $OUTPUT_DIR/"
ls -lh "$OUTPUT_DIR/"
