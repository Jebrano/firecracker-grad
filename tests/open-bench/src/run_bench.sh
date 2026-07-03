#!/usr/bin/env bash
# Drives the open-bench harness under your standard isolation protocol and
# emits one JSON line per condition to results.jsonl.
#
# Usage: sudo ./run_bench.sh [iterations] [warmup]
#
# Requires root (for chroot) and CAP_SYS_PTRACE if you enable the perf
# section at the bottom.

set -euo pipefail

ITERATIONS="${1:-20000}"
WARMUP="${2:-2000}"
CORE_RANGE="2-3"          # matches your existing taskset convention
BIN="./target/release/open-bench" # We need to change this
WORKDIR="$(mktemp -d /tmp/open-bench.XXXXXX)"
OUT="results.jsonl"

echo "[*] workdir: $WORKDIR"
echo "[*] iterations=$ITERATIONS warmup=$WARMUP core=$CORE_RANGE"

# --- isolation, matching your fio protocol ----------------------------
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

# --- fixture setup ------------------------------------------------------
mkdir -p "$WORKDIR/allowed"
echo "microbenchmark target file" > "$WORKDIR/allowed/target.bin"

mkdir -p "$WORKDIR/jail"
cp "$WORKDIR/allowed/target.bin" "$WORKDIR/jail/target.bin"

# emptying out result.json
: > "$OUT"

# --- baseline ---
taskset -c "$CORE_RANGE" "$BIN" baseline \
  --target-dir "$WORKDIR/allowed" \
  --target-name target.bin \
  --iterations "$ITERATIONS" --warmup "$WARMUP" \
  --raw-out "$WORKDIR/allowed/baseline_raw.txt" \
  >> "$OUT"

# --- landlock ---
sudo taskset -c "$CORE_RANGE" "$BIN" landlock \
  --target-dir "$WORKDIR/allowed" \
  --target-name target.bin \
  --allow-root "$WORKDIR/allowed" \
  --iterations "$ITERATIONS" --warmup "$WARMUP" \
  --raw-out "$WORKDIR/allowed/landlock_raw.txt" \
  >> "$OUT"

# --- chroot ---
sudo taskset -c "$CORE_RANGE" "$BIN" chroot \
  --jail-root "$WORKDIR/jail" \
  --target-name target.bin \
  --iterations "$ITERATIONS" --warmup "$WARMUP" \
  --raw-out /chroot_raw.txt \
  >> "$OUT"


echo "[*] done. summaries in $OUT, raw samples in $WORKDIR"
echo
cat "$OUT" | python3 -m json.tool --compact 2>/dev/null || cat "$OUT"

# --- optional: attribute cost to the LSM hook specifically ----------------
# Uncomment to also capture perf counters for the landlock condition.
# Requires perf and the security_file_open tracepoint to exist on 6.17.
# Don't forget to add it to probe `sudo perf probe --add security_file_open`
#
echo "[*] perf stat pass (landlock)"
sudo perf stat -e probe:security_file_open \
  taskset -c "$CORE_RANGE" "$BIN" landlock \
    --target-dir "$WORKDIR/allowed" \
    --target-name target.bin \
    --allow-root "$WORKDIR/allowed" \
    --iterations "$ITERATIONS" --warmup "$WARMUP" \
    > /dev/null
