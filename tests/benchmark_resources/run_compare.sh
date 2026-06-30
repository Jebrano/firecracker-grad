#!/bin/bash
# run_compare.sh — run file_ops benchmark on all three configurations
# and print a comparison table.
#
# Usage:
#   ./run_compare.sh [--kernel KERNEL_PATH] [--iterations N] [--num-files N]
#
# Prerequisites:
#   - Pre-formatted ext4 image at /users/Jubranoo/fc-bench/bench-fs.ext4
#     (create with: dd if=/dev/zero of=bench-fs.ext4 bs=1M count=512
#                   mkfs.ext4 bench-fs.ext4)
#   - All binaries built (firecracker, firecracker-landlock, jailer, landlock-jailer)
#   - Rootfs at /users/Jubranoo/fc-bench/rootfs-baseline.ext4
#   - Kernel image available

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BASE="/users/Jubranoo/fc-bench"

# ── defaults ───────────────────────────────────────────────────────
KERNEL="${KERNEL:-$BASE/vmlinux.bin}"
ITERATIONS="${ITERATIONS:-1}"
NUM_FILES="${NUM_FILES:-5000}"
FILE_OPS_MODE="file_ops:num_files=$NUM_FILES"

# ── parse args ──────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --kernel)      KERNEL="$2"; shift 2 ;;
        --iterations)  ITERATIONS="$2"; shift 2 ;;
        --num-files)   NUM_FILES="$2"; FILE_OPS_MODE="file_ops:num_files=$NUM_FILES"; shift 2 ;;
        *) echo "Unknown arg: $1"; exit 1 ;;
    esac
done

OUTDIR="$BASE/comparisons"
mkdir -p "$OUTDIR"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)

# ── ensure bench-fs.ext4 exists ─────────────────────────────────────
if [ ! -f "$BASE/bench-fs.ext4" ]; then
    echo "Creating pre-formatted ext4 benchmark disk..."
    dd if=/dev/zero of="$BASE/bench-fs.ext4" bs=1M count=512 status=none
    mkfs.ext4 -F "$BASE/bench-fs.ext4" > /dev/null 2>&1
    echo "  Created: $BASE/bench-fs.ext4 (512 MB ext4)"
fi

# ── helpers ─────────────────────────────────────────────────────────

run_bench() {
    local label="$1"; shift
    local out="$OUTDIR/${label}_${TIMESTAMP}.json"

    echo ""
    echo "============================================================"
    echo "  $label"
    echo "============================================================"

    cargo run -p "$@" -- \
        --mode file-ops \
        --kernel "$KERNEL" \
        --iterations "$ITERATIONS" \
        --output "$out" \
        2>&1 | sed 's/^/  /'

    echo "  → saved: $out"
    echo "$out"
}

# ── run all three ───────────────────────────────────────────────────

echo "=== File Ops Benchmark: Landlock Overhead Comparison ==="
echo "  Mode:      $FILE_OPS_MODE"
echo "  Iterations: $ITERATIONS"
echo "  Num files:  $NUM_FILES"
echo "  Kernel:     $KERNEL"

RES1=$(run_bench "base"        "fc-bench")
RES2=$(run_bench "jailed"      "def-jailer-bench" -- --uid 1000 --gid 1000)
RES3=$(run_bench "landlock"    "def-jailer-bench" -- --landlock --uid 1000 --gid 1000)

# ── comparison ──────────────────────────────────────────────────────

echo ""
echo "============================================================"
echo "  Comparison"
echo "============================================================"

extract_field() {
    local file="$1" field="$2"
    python3 -c "
import json, sys
with open('$file') as f:
    data = json.load(f)
if data:
    r = data[0]  # first (only) iteration
    val = r.get('fio', {}).get('$field', 'N/A')
    print(val)
" 2>/dev/null || echo "N/A"
}

# Extract timing from each result
BASE_TOTAL=$(extract_field "$RES1" "total_s")
JAILED_TOTAL=$(extract_field "$RES2" "total_s")
LANDLOCK_TOTAL=$(extract_field "$RES3" "total_s")

echo ""
echo "  ┌──────────────┬──────────┬──────────┬──────────┐"
echo "  │ Phase        │ Base (s) │ Jailed (s) │ Landlock (s) │"
echo "  ├──────────────┼──────────┼──────────┼──────────┤"

for phase in create_s stat_s read_s unlink_s total_s; do
    B=$(extract_field "$RES1" "$phase")
    J=$(extract_field "$RES2" "$phase")
    L=$(extract_field "$RES3" "$phase")
    printf "  │ %-12s │ %8s │ %8s │ %8s │\n" "$phase" "$B" "$J" "$L"
done

echo "  └──────────────┴──────────┴──────────┴──────────┘"

# ── overhead calculation ────────────────────────────────────────────
calc_overhead() {
    python3 -c "
base = float('$1')
other = float('$2')
if base > 0:
    pct = ((other - base) / base) * 100
    print(f'{pct:+.1f}%')
else:
    print('N/A')
" 2>/dev/null || echo "N/A"
}

JAILED_OH=$(calc_overhead "$BASE_TOTAL" "$JAILED_TOTAL")
LANDLOCK_OH=$(calc_overhead "$BASE_TOTAL" "$LANDLOCK_TOTAL")

echo ""
echo "  Jailer overhead (vs base):        $JAILED_OH"
echo "  Landlock overhead (vs base):      $LANDLOCK_OH"

echo ""
echo "  Results saved to: $OUTDIR/"
