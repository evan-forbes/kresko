#!/usr/bin/env bash
#
# Benchmark common (200,9) and regtest (48,5) Equihash solver rates, then run
# a Monte Carlo matrix that changes only the Equihash parameter set.
#
# Usage:
#   ./scripts/compare-equihash-params.sh
#   ./scripts/compare-equihash-params.sh --bench-seconds 120 --seeds 1..100 --release
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
KRESKO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BENCH_SECONDS=60
MINERS="10,20,40,60,80"
TARGET_SPACING=75
BLOCKS=10000
PROPAGATION_DELAYS="0.5,1,2,5,10"
POW_PROFILE="mainnet"
SEEDS="1..100"
OUTPUT_DIR=""
KRESKO_BIN=""
USE_RELEASE=false

usage() {
    cat <<'EOF'
Usage: compare-equihash-params.sh [options]

Options:
  --bench-seconds SECS        Minimum seconds per Equihash benchmark (default: 60)
  --miners LIST               Miner counts, comma-separated (default: 10,20,40,60,80)
  --target-spacing SECS       Target block spacing seconds (default: 75)
  --blocks COUNT              Canonical blocks per simulation run (default: 10000)
  --propagation-delays LIST   Propagation delays, comma-separated (default: 0.5,1,2,5,10)
  --pow-profile PROFILE       DAA profile: mainnet or responsive (default: mainnet)
  --seeds LIST_OR_RANGE       Seeds like 1,2,3 or 1..100 (default: 1..100)
  --output-dir DIR            Output directory (default: data/pow-param-compare-<timestamp>)
  --kresko-bin PATH           Use an already-built kresko binary instead of cargo run
  --release                   Use cargo run --release when --kresko-bin is not set
  -h, --help                  Show this help
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --bench-seconds)
            BENCH_SECONDS="$2"
            shift 2
            ;;
        --miners)
            MINERS="$2"
            shift 2
            ;;
        --target-spacing)
            TARGET_SPACING="$2"
            shift 2
            ;;
        --blocks)
            BLOCKS="$2"
            shift 2
            ;;
        --propagation-delays)
            PROPAGATION_DELAYS="$2"
            shift 2
            ;;
        --pow-profile)
            POW_PROFILE="$2"
            shift 2
            ;;
        --seeds)
            SEEDS="$2"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="$2"
            shift 2
            ;;
        --kresko-bin)
            KRESKO_BIN="$2"
            shift 2
            ;;
        --release)
            USE_RELEASE=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ -z "$OUTPUT_DIR" ]; then
    OUTPUT_DIR="$KRESKO_ROOT/data/pow-param-compare-$(date +%Y%m%d-%H%M%S)"
fi

mkdir -p "$OUTPUT_DIR"

run_kresko() {
    if [ -n "$KRESKO_BIN" ]; then
        "$KRESKO_BIN" "$@"
    elif [ "$USE_RELEASE" = true ]; then
        cargo run --release --quiet -- "$@"
    else
        cargo run --quiet -- "$@"
    fi
}

extract_sol_rate() {
    local params="$1"
    local log_path="$2"
    awk -v params="$params" '
        $0 ~ "matrix input: --sol-per-sec " params "=" {
            split($0, parts, params "=")
            print parts[2]
        }
    ' "$log_path" | tail -n 1
}

cd "$KRESKO_ROOT"

echo "Output directory: $OUTPUT_DIR"
echo "Benchmarking common Equihash (200,9) for at least ${BENCH_SECONDS}s..."
run_kresko pow-bench \
    --equihash-params common \
    --min-seconds "$BENCH_SECONDS" \
    | tee "$OUTPUT_DIR/pow-bench-common.log"

echo "Benchmarking regtest Equihash (48,5) for at least ${BENCH_SECONDS}s..."
run_kresko pow-bench \
    --equihash-params regtest \
    --min-seconds "$BENCH_SECONDS" \
    | tee "$OUTPUT_DIR/pow-bench-regtest.log"

COMMON_SOL_PER_SEC="$(extract_sol_rate common "$OUTPUT_DIR/pow-bench-common.log")"
REGTEST_SOL_PER_SEC="$(extract_sol_rate regtest "$OUTPUT_DIR/pow-bench-regtest.log")"

if [ -z "$COMMON_SOL_PER_SEC" ] || [ -z "$REGTEST_SOL_PER_SEC" ]; then
    echo "failed to extract sol/s from benchmark logs in $OUTPUT_DIR" >&2
    exit 1
fi

SOL_PER_SEC="common=${COMMON_SOL_PER_SEC},regtest=${REGTEST_SOL_PER_SEC}"
CSV_PATH="$OUTPUT_DIR/pow-sim-equihash-param-compare.csv"

cat >"$OUTPUT_DIR/run-config.env" <<EOF
BENCH_SECONDS=$BENCH_SECONDS
MINERS=$MINERS
TARGET_SPACING=$TARGET_SPACING
BLOCKS=$BLOCKS
PROPAGATION_DELAYS=$PROPAGATION_DELAYS
POW_PROFILE=$POW_PROFILE
SEEDS=$SEEDS
COMMON_SOL_PER_SEC=$COMMON_SOL_PER_SEC
REGTEST_SOL_PER_SEC=$REGTEST_SOL_PER_SEC
EOF

echo "Running matrix with --sol-per-sec $SOL_PER_SEC"
run_kresko pow-simulate-matrix \
    --equihash-params common,regtest \
    --sol-per-sec "$SOL_PER_SEC" \
    --miners "$MINERS" \
    --target-spacing "$TARGET_SPACING" \
    --blocks "$BLOCKS" \
    --propagation-delays "$PROPAGATION_DELAYS" \
    --pow-profile "$POW_PROFILE" \
    --seeds "$SEEDS" \
    --csv "$CSV_PATH" \
    | tee "$OUTPUT_DIR/pow-simulate-matrix.log"

echo "CSV: $CSV_PATH"
echo "Config: $OUTPUT_DIR/run-config.env"
