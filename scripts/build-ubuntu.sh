#!/bin/bash
#
# Build Ubuntu 22.04-compatible binaries for zebrad and kresko using Docker.
#
# Usage:
#   ./scripts/build-ubuntu.sh                    # Build both zebrad and kresko
#   ./scripts/build-ubuntu.sh --zebrad-only      # Build only zebrad
#   ./scripts/build-ubuntu.sh --kresko-only      # Build only kresko
#
# Prerequisites:
#   - Docker (or podman with `alias docker=podman`)
#   - Zebra repo at ../zebra (relative to kresko root)
#
# Output:
#   build/ubuntu/zebrad    - Ubuntu 22.04-compatible zebrad binary
#   build/ubuntu/kresko    - Ubuntu 22.04-compatible kresko binary
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
KRESKO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ZEBRA_ROOT="$(cd "$KRESKO_ROOT/../zebra" && pwd)"

BUILD_ZEBRAD=true
BUILD_KRESKO=true
if [ "${1:-}" = "--zebrad-only" ]; then
    BUILD_KRESKO=false
elif [ "${1:-}" = "--kresko-only" ]; then
    BUILD_ZEBRAD=false
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

OUTPUT_DIR="$KRESKO_ROOT/build/ubuntu"
mkdir -p "$OUTPUT_DIR"

# Build the builder image (cached after first run).
echo "=== Building Docker image (cached) ==="
docker build -t "$FULL_IMAGE" -f - "$KRESKO_ROOT" <<'DOCKERFILE'
FROM ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    git \
    libclang-dev \
    pkg-config \
    protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /build
DOCKERFILE

if [ "$BUILD_ZEBRAD" = true ]; then
    echo "=== Building zebrad ==="
    docker run --rm \
        -v "$ZEBRA_ROOT:/build/zebra:ro" \
        -v "$OUTPUT_DIR:/output" \
        -v "kresko-cargo-registry:/root/.cargo/registry" \
        -v "kresko-cargo-git:/root/.cargo/git" \
        "$FULL_IMAGE" \
        bash -c '
            cp -r /build/zebra /tmp/zebra && cd /tmp/zebra &&
            cargo build --release --package zebrad --bin zebrad &&
            cp target/release/zebrad /output/zebrad &&
            echo "=== zebrad built successfully ==="
        '
    echo "Built: $OUTPUT_DIR/zebrad"
    file "$OUTPUT_DIR/zebrad"
fi

if [ "$BUILD_KRESKO" = true ]; then
    echo "=== Building kresko ==="
    docker run --rm \
        -v "$ZEBRA_ROOT:/build/zebra:ro" \
        -v "$KRESKO_ROOT:/build/kresko:ro" \
        -v "$OUTPUT_DIR:/output" \
        -v "kresko-cargo-registry:/root/.cargo/registry" \
        -v "kresko-cargo-git:/root/.cargo/git" \
        "$FULL_IMAGE" \
        bash -c '
            cp -r /build/zebra /tmp/zebra &&
            cp -r /build/kresko /tmp/kresko && cd /tmp/kresko &&
            cargo build --release &&
            cp target/release/kresko /output/kresko &&
            echo "=== kresko built successfully ==="
        '
    echo "Built: $OUTPUT_DIR/kresko"
    file "$OUTPUT_DIR/kresko"
fi

echo "=== Done ==="
echo "Ubuntu 22.04-compatible binaries in: $OUTPUT_DIR/"
ls -lh "$OUTPUT_DIR/"
