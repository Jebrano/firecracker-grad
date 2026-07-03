#!/usr/bin/env bash
# Drives the open-bench harness under your standard isolation protocol,
# interleaving conditions cycle-by-cycle (rather than running each
# condition as one long block) so residual per-core cache/TLB/branch-
# predictor state from whichever condition ran previously doesn't
# systematically favor one condition over another. Pools samples across
# cycles, then runs a Mann-Whitney U test on every pair.
#
# Usage: sudo ./run_bench.sh [cycles] [iterations_per_cycle] [warmup_per_cycle]
#
# Requires root (for chroot).

set -euo pipefail

CYCLES="${1:-100}"
ITER_PER_CYCLE="${2:-200}"
WARMUP_PER_CYCLE="${3:-50}"
CORE_RANGE="2-3"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$SCRIPT_DIR/target/release/open-bench"
WORKDIR="$(mktemp -d /tmp/open-bench.XXXXXX)"
OUT="results.jsonl"

echo "[*] workdir: $WORKDIR"
echo "[*] cycles=$CYCLES iterations/cycle=$ITER_PER_CYCLE warmup/cycle=$WARMUP_PER_CYCLE core=$CORE_RANGE"
echo "[*] total samples per condition: $((CYCLES * ITER_PER_CYCLE))"

if [[ ! -x "$BIN" ]]; then
  echo "[!] $BIN not found — run 'cargo build --release' first"
  exit 1
fi

# --- isolation, matching your fio protocol ---------------------------------
echo "[*] pinning CPU governor to performance"
sudo cpupower frequency-set -g performance >/dev/null

echo "[*] disabling turbo"
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo >/dev/null 2>&1 || \
  echo 0 | sudo tee /sys/devices/system/cpu/cpufreq/boost >/dev/null 2>&1 || \
  echo "[!] could not find a turbo-disable knob on this platform, continuing anyway"

echo "[*] dropping caches"
sync
echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null

# add any background-service suppression you already use for fio here
# systemctl stop <noisy-service> ...

# --- fixture setup -----------------------------------------------------
mkdir -p "$WORKDIR/allowed"
echo "microbenchmark target file" > "$WORKDIR/allowed/target.bin"

mkdir -p "$WORKDIR/jail"
cp "$WORKDIR/allowed/target.bin" "$WORKDIR/jail/target.bin"

BASELINE_RAW="$WORKDIR/allowed/baseline_raw.txt"
LANDLOCK_RAW="$WORKDIR/allowed/landlock_raw.txt"
CHROOT_RAW="$WORKDIR/jail/chroot_raw.txt"   # written post-chroot: path is relative to jail root

: > "$BASELINE_RAW"
: > "$LANDLOCK_RAW"
: > "$CHROOT_RAW"
: > "$OUT"

# --- interleaved cycles -----------------------------------------------
for i in $(seq 1 "$CYCLES"); do
  printf "\r[*] cycle %d/%d" "$i" "$CYCLES"

  taskset -c "$CORE_RANGE" "$BIN" baseline \
    --target-dir "$WORKDIR/allowed" \
    --target-name target.bin \
    --iterations "$ITER_PER_CYCLE" --warmup "$WARMUP_PER_CYCLE" \
    --raw-out "$BASELINE_RAW" --raw-append \
    >> "$OUT"

  sudo taskset -c "$CORE_RANGE" "$BIN" landlock \
    --target-dir "$WORKDIR/allowed" \
    --target-name target.bin \
    --allow-root "$WORKDIR/allowed" \
    --iterations "$ITER_PER_CYCLE" --warmup "$WARMUP_PER_CYCLE" \
    --raw-out "$LANDLOCK_RAW" --raw-append \
    >> "$OUT"

  sudo taskset -c "$CORE_RANGE" "$BIN" chroot \
    --jail-root "$WORKDIR/jail" \
    --target-name target.bin \
    --iterations "$ITER_PER_CYCLE" --warmup "$WARMUP_PER_CYCLE" \
    --raw-out /chroot_raw.txt --raw-append \
    >> "$OUT"
done
echo

echo "[*] done. per-cycle summaries in $OUT, pooled raw samples in $WORKDIR"

# --- statistical comparison ---------------------------------------------
echo
echo "[*] baseline vs landlock:"
"$BIN" analyze --a "$BASELINE_RAW" --a-label baseline --b "$LANDLOCK_RAW" --b-label landlock \
  | python3 -m json.tool

echo
echo "[*] baseline vs chroot:"
"$BIN" analyze --a "$BASELINE_RAW" --a-label baseline --b "$CHROOT_RAW" --b-label chroot \
  | python3 -m json.tool

echo
echo "[*] landlock vs chroot:"
"$BIN" analyze --a "$LANDLOCK_RAW" --a-label landlock --b "$CHROOT_RAW" --b-label chroot \
  | python3 -m json.tool

# --- optional: attribute cost to the LSM hook specifically ----------------
# Requires the security_file_open tracepoint/probe to exist on your kernel.
# Uncomment after: sudo perf probe --add security_file_open
#
# echo "[*] perf stat pass (landlock)"
# sudo perf stat -e probe:security_file_open \
#   taskset -c "$CORE_RANGE" "$BIN" landlock \
#     --target-dir "$WORKDIR/allowed" \
#     --target-name target.bin \
#     --allow-root "$WORKDIR/allowed" \
#     --iterations "$ITER_PER_CYCLE" --warmup "$WARMUP_PER_CYCLE" \
#     > /dev/null
