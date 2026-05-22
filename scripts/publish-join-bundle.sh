#!/usr/bin/env bash
set -Eeuo pipefail

# Generate a NU7 join bundle from an experiment run directory, compress it, and
# attach it to a kresko GitHub release. This gives joiners a stable --bundle-url
# (https://github.com/<repo>/releases/download/<tag>/nu7-join-bundle.tar.gz) to
# pass to scripts/join-nu7-testnet.sh.
#
# The bundle is run-specific (it embeds the genesis/seed blocks and the live
# bootstrap-peer IPs from the run), so this is an operator step run from the run
# dir — it cannot be produced by the release build CI.
#
# Requires: a built kresko, tar, sha256sum, and (unless --skip-upload) the gh CLI
# authenticated with write access to the release repo.

RUN_DIR=""
TAG="v0.1.0"
REPO="valargroup/kresko"
NAME="nu7-join-bundle"
ARCHIVE=""
OUT_DIR=""
KRESKO_BIN="${KRESKO_BIN:-}"
JOIN_SCRIPT=""
SKIP_UPLOAD=0
SKIP_SCRIPT_UPLOAD=0
FORWARD=()

usage() {
    cat <<'USAGE'
Usage: publish-join-bundle.sh --run-dir DIR [options] [-- <extra join-bundle args>]

Generates a join bundle, compresses it to <name>.tar.gz (+ .sha256), validates
it, and uploads it to a kresko GitHub release.

Options:
  --run-dir DIR        Kresko experiment run directory (config.json + payload). Required.
  --tag TAG            Release tag to attach the bundle to. Default: v0.1.0
  --repo OWNER/NAME    Release repo. Default: valargroup/kresko
  --name BASENAME      Archive base name. Default: nu7-join-bundle
  --archive PATH       Output tarball path. Default: ./<name>.tar.gz
  --out DIR            Directory to generate the bundle into. Default: a temp dir.
  --kresko-bin PATH    kresko binary to use. Default: kresko on PATH, else target/release/kresko.
  --join-script PATH   Join script to upload. Default: scripts/join-nu7-testnet.sh.
  --skip-upload        Build and validate the tarball but do not upload (e.g. to host elsewhere).
  --skip-script-upload Upload only the bundle assets, not join-nu7-testnet.sh.
  -h, --help           Show this help.

Anything after `--` is forwarded to `kresko join-bundle` (e.g. --zebra-release-tag).
USAGE
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --run-dir)     RUN_DIR="${2:-}";     [ -n "$RUN_DIR" ]     || { echo "missing value for --run-dir" >&2; exit 2; }; shift 2 ;;
        --tag)         TAG="${2:-}";         [ -n "$TAG" ]         || { echo "missing value for --tag" >&2; exit 2; }; shift 2 ;;
        --repo)        REPO="${2:-}";        [ -n "$REPO" ]        || { echo "missing value for --repo" >&2; exit 2; }; shift 2 ;;
        --name)        NAME="${2:-}";        [ -n "$NAME" ]        || { echo "missing value for --name" >&2; exit 2; }; shift 2 ;;
        --archive)     ARCHIVE="${2:-}";     [ -n "$ARCHIVE" ]     || { echo "missing value for --archive" >&2; exit 2; }; shift 2 ;;
        --out)         OUT_DIR="${2:-}";     [ -n "$OUT_DIR" ]     || { echo "missing value for --out" >&2; exit 2; }; shift 2 ;;
        --kresko-bin)  KRESKO_BIN="${2:-}";  [ -n "$KRESKO_BIN" ]  || { echo "missing value for --kresko-bin" >&2; exit 2; }; shift 2 ;;
        --join-script) JOIN_SCRIPT="${2:-}"; [ -n "$JOIN_SCRIPT" ] || { echo "missing value for --join-script" >&2; exit 2; }; shift 2 ;;
        --skip-upload) SKIP_UPLOAD=1; shift ;;
        --skip-script-upload) SKIP_SCRIPT_UPLOAD=1; shift ;;
        --)            shift; FORWARD=("$@"); break ;;
        -h|--help)     usage; exit 0 ;;
        *)             echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

[ -n "$RUN_DIR" ] || { echo "missing --run-dir" >&2; usage >&2; exit 2; }
[ -d "$RUN_DIR" ] || { echo "run dir not found: $RUN_DIR" >&2; exit 1; }

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || { echo "required command not found: $1" >&2; exit 1; }
}
require_cmd tar
require_cmd sha256sum
[ "$SKIP_UPLOAD" -eq 1 ] || require_cmd gh

if [ -z "$KRESKO_BIN" ]; then
    if command -v kresko >/dev/null 2>&1; then
        KRESKO_BIN="kresko"
    elif [ -x "target/release/kresko" ]; then
        KRESKO_BIN="target/release/kresko"
    else
        echo "kresko binary not found; build it or pass --kresko-bin" >&2
        exit 1
    fi
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ARCHIVE="${ARCHIVE:-$PWD/$NAME.tar.gz}"
JOIN_SCRIPT="${JOIN_SCRIPT:-$SCRIPT_DIR/join-nu7-testnet.sh}"

[ -f "$JOIN_SCRIPT" ] || { echo "join script not found: $JOIN_SCRIPT" >&2; exit 1; }

if [ -z "$OUT_DIR" ]; then
    OUT_DIR="$(mktemp -d)"
    trap 'rm -rf "$OUT_DIR"' EXIT
fi

echo "generating join bundle from $RUN_DIR"
"$KRESKO_BIN" join-bundle --run-dir "$RUN_DIR" --out "$OUT_DIR" ${FORWARD[@]+"${FORWARD[@]}"}

echo "compressing -> $ARCHIVE"
tar -C "$OUT_DIR" -czf "$ARCHIVE" .

echo "validating bundle with join-nu7-testnet.sh --dry-run"
bash "$SCRIPT_DIR/join-nu7-testnet.sh" --bundle-url "$ARCHIVE" --dry-run

echo "writing checksum"
( cd "$(dirname "$ARCHIVE")" && sha256sum "$(basename "$ARCHIVE")" > "$(basename "$ARCHIVE").sha256" )

if [ "$SKIP_UPLOAD" -eq 1 ]; then
    echo "skip-upload: bundle ready at"
    echo "  $ARCHIVE"
    echo "  $ARCHIVE.sha256"
    exit 0
fi

echo "uploading to $REPO release $TAG"
assets=("$ARCHIVE" "$ARCHIVE.sha256")
if [ "$SKIP_SCRIPT_UPLOAD" -eq 0 ]; then
    assets+=("$JOIN_SCRIPT")
fi
gh release upload "$TAG" "${assets[@]}" --clobber --repo "$REPO"

echo "done. join with:"
echo "  --bundle-url https://github.com/$REPO/releases/download/$TAG/$(basename "$ARCHIVE")"
