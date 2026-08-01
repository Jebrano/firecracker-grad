#!/usr/bin/env bash
# Interleaved driver for the two benchmarks that don't have their own driver
# script yet: setup-bench's multi-file-open mode (idea #5) and
# fleet-churn-bench (idea #2). Purely additive: does not modify env.rs,
# landlock.rs, setup_bench.rs, fleet_churn_bench.rs, Cargo.toml, or either
# of the existing driver scripts (run_bench.sh, setup_bench.sh). It only
# invokes those binaries the same way you would by hand, just interleaved
# and pooled across many cycles, with the statistics handed off to
# open-bench's already-validated Mann-Whitney U analyzer.
#
# Usage: sudo ./multi_file_and_fleet_bench.sh [cycles] <path-to-real-firecracker-binary> [chroot-base-dir]
#
# Requires root (chroot, and Landlock rule setup touches real device paths).

set -euo pipefail

CYCLES="${1:-100}"
EXEC_FILE="${2:?usage: $0 [cycles] <path-to-real-firecracker-binary> [chroot-base-dir]}"
CHROOT_BASE="${3:-/srv/jailer-bench}"
CORE_RANGE="3"
JAILER_UID="${JAILER_UID:-123}"
JAILER_GID="${JAILER_GID:-100}"
FLEET_TIMEOUT_MS="${FLEET_TIMEOUT_MS:-2000}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SETUP_BENCH_BIN="$SCRIPT_DIR/setup-bench"
JAILER_BIN="$SCRIPT_DIR/jailer"
LANDLOCK_BIN="$SCRIPT_DIR/landlock-jailer"
FLEET_BIN="$SCRIPT_DIR/fleet-churn-bench"
OPEN_BENCH_BIN="${OPEN_BENCH_BIN:-$SCRIPT_DIR/open-bench}"

for bin in "$SETUP_BENCH_BIN" "$JAILER_BIN" "$LANDLOCK_BIN" "$FLEET_BIN" "$OPEN_BENCH_BIN"; do
  if [[ ! -x "$bin" ]]; then
    echo "[!] not found or not executable: $bin"
    echo "    (build the jailer crate with 'cargo build --release', and"
    echo "     open-bench per its own instructions, or set OPEN_BENCH_BIN)"
    exit 1
  fi
done

mkdir -p "$CHROOT_BASE"
WORKDIR="$(mktemp -d /tmp/extra-bench.XXXXXX)"
echo "[*] workdir: $WORKDIR"
echo "[*] cycles=$CYCLES exec_file=$EXEC_FILE chroot_base=$CHROOT_BASE uid=$JAILER_UID gid=$JAILER_GID"

# --- isolation, matching your fio / open-bench / setup_bench protocol ------
echo "[*] pinning CPU governor to performance"
sudo cpupower frequency-set -g performance >/dev/null

echo "[*] disabling turbo"
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo >/dev/null 2>&1 || \
  echo 0 | sudo tee /sys/devices/system/cpu/cpufreq/boost >/dev/null 2>&1 || \
  echo "[!] could not find a turbo-disable knob on this platform, continuing anyway"

echo "[*] dropping caches"
sync
echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null

exec_file_name="$(basename "$EXEC_FILE")"

##############################################################################
# PART A: setup-bench --mode multi-file-open, interleaved
##############################################################################
MFO_CHROOT_JSONL="$WORKDIR/mfo_chroot.jsonl"
MFO_LANDLOCK_JSONL="$WORKDIR/mfo_landlock.jsonl"
: > "$MFO_CHROOT_JSONL"
: > "$MFO_LANDLOCK_JSONL"

echo
echo "[*] multi-file-open: $CYCLES interleaved cycles"
for i in $(seq 1 "$CYCLES"); do
  printf "\r[*] multi-file-open cycle %d/%d" "$i" "$CYCLES"

  id_chroot="mfo-chroot-$i-$RANDOM"
  line=$(sudo taskset -c "$CORE_RANGE" "$SETUP_BENCH_BIN" \
    --isolation chroot --mode multi-file-open \
    --id "$id_chroot" --exec-file "$EXEC_FILE" \
    --uid "$JAILER_UID" --gid "$JAILER_GID" \
    --chroot-base-dir "$CHROOT_BASE" \
    -- --api-sock /run/api.sock) || line=""
  [[ -n "$line" ]] && echo "$line" >> "$MFO_CHROOT_JSONL"
  sudo rm -rf "${CHROOT_BASE:?}/$exec_file_name/$id_chroot"

  id_landlock="mfo-landlock-$i-$RANDOM"
  line=$(sudo taskset -c "$CORE_RANGE" "$SETUP_BENCH_BIN" \
    --isolation landlock --mode multi-file-open \
    --id "$id_landlock" --exec-file "$EXEC_FILE" \
    --uid "$JAILER_UID" --gid "$JAILER_GID" \
    --chroot-base-dir "$CHROOT_BASE" \
    -- --api-sock /run/api.sock) || line=""
  [[ -n "$line" ]] && echo "$line" >> "$MFO_LANDLOCK_JSONL"
  sudo rm -rf "${CHROOT_BASE:?}/$exec_file_name/$id_landlock"
done
echo
echo "[*] multi-file-open: $(wc -l < "$MFO_CHROOT_JSONL") chroot samples, $(wc -l < "$MFO_LANDLOCK_JSONL") landlock samples"

# Pulls a numeric field out of every JSON line in a file into a flat
# one-value-per-line file, without a jq dependency. Works for both
# top-level fields (e.g. total_setup_ns) and phase/file_opens_ns entries,
# since the value always immediately follows "field":digits.
extract_field() {
  local field="$1"
  grep -o "\"${field}\":[0-9]*" | cut -d: -f2
}

echo
echo "[*] multi-file-open: total_setup_ns (chroot vs landlock)"
extract_field total_setup_ns < "$MFO_CHROOT_JSONL"   > "$WORKDIR/mfo_chroot_total.txt"
extract_field total_setup_ns < "$MFO_LANDLOCK_JSONL" > "$WORKDIR/mfo_landlock_total.txt"
if [[ -s "$WORKDIR/mfo_chroot_total.txt" && -s "$WORKDIR/mfo_landlock_total.txt" ]]; then
  "$OPEN_BENCH_BIN" analyze \
    --a "$WORKDIR/mfo_chroot_total.txt" --a-label chroot_total_setup \
    --b "$WORKDIR/mfo_landlock_total.txt" --b-label landlock_total_setup \
    | python3 -m json.tool
fi

# file_opens_ns keys are full paths and differ between conditions (chroot:
# "/kernel.img"; landlock: the real host path ending in ".../kernel.img"),
# except /dev/kvm and /dev/net/tun, which are identical absolute strings
# under both -- see setup_bench.rs's doc comment. Suffix-matching on the
# filename handles all six uniformly without needing to special-case those
# two.
echo
echo "[*] multi-file-open: per-file open latency (chroot vs landlock)"
for suffix in "kernel.img" "rootfs.ext4" "metrics.fifo" "firecracker.log" "dev/kvm" "dev/net/tun"; do
  label="$(echo "$suffix" | tr '/' '_')"
  grep -o "\"[^\"]*${suffix}\":[0-9]*" "$MFO_CHROOT_JSONL"   | cut -d: -f2 > "$WORKDIR/mfo_chroot_${label}.txt"
  grep -o "\"[^\"]*${suffix}\":[0-9]*" "$MFO_LANDLOCK_JSONL" | cut -d: -f2 > "$WORKDIR/mfo_landlock_${label}.txt"
  if [[ -s "$WORKDIR/mfo_chroot_${label}.txt" && -s "$WORKDIR/mfo_landlock_${label}.txt" ]]; then
    echo "--- $suffix ---"
    "$OPEN_BENCH_BIN" analyze \
      --a "$WORKDIR/mfo_chroot_${label}.txt" --a-label "chroot_${label}" \
      --b "$WORKDIR/mfo_landlock_${label}.txt" --b-label "landlock_${label}" \
      | python3 -m json.tool
  else
    echo "--- $suffix: no samples on one or both sides, skipping ---"
  fi
done

##############################################################################
# PART B: fleet-churn-bench, interleaved
##############################################################################
# fleet-churn-bench's own --cycles loop runs one condition as a block, so
# true interleaving happens here instead: invoke it with --cycles 1,
# alternating conditions, across many invocations. This costs an extra
# process spawn of fleet-churn-bench itself per sample, which is immaterial
# at this benchmark's scale (socket-ready latency is already ms-order, see
# the earlier discussion of why black-box timing is fine here).
FCB_CHROOT_TOTAL="$WORKDIR/fcb_chroot_total.txt"
FCB_LANDLOCK_TOTAL="$WORKDIR/fcb_landlock_total.txt"
: > "$FCB_CHROOT_TOTAL"
: > "$FCB_LANDLOCK_TOTAL"

echo
echo "[*] fleet-churn-bench: $CYCLES interleaved cycles"
for i in $(seq 1 "$CYCLES"); do
  printf "\r[*] fleet-churn cycle %d/%d" "$i" "$CYCLES"

  line=$(sudo taskset -c "$CORE_RANGE" "$FLEET_BIN" \
    --jailer-bin "$JAILER_BIN" --condition chroot \
    --exec-file "$EXEC_FILE" --uid "$JAILER_UID" --gid "$JAILER_GID" \
    --chroot-base-dir "$CHROOT_BASE" --cycles 1 --timeout-ms "$FLEET_TIMEOUT_MS")
  ready=$(echo "$line" | grep -o '"socket_ready_ns":[0-9]*' | cut -d: -f2 || true)
  [[ -n "$ready" ]] && echo "$ready" >> "$FCB_CHROOT_TOTAL"

  line=$(sudo taskset -c "$CORE_RANGE" "$FLEET_BIN" \
    --jailer-bin "$LANDLOCK_BIN" --condition landlock \
    --exec-file "$EXEC_FILE" --uid "$JAILER_UID" --gid "$JAILER_GID" \
    --chroot-base-dir "$CHROOT_BASE" --cycles 1 --timeout-ms "$FLEET_TIMEOUT_MS")
  ready=$(echo "$line" | grep -o '"socket_ready_ns":[0-9]*' | cut -d: -f2 || true)
  [[ -n "$ready" ]] && echo "$ready" >> "$FCB_LANDLOCK_TOTAL"
done
echo
echo "[*] fleet-churn-bench: $(wc -l < "$FCB_CHROOT_TOTAL") chroot samples, $(wc -l < "$FCB_LANDLOCK_TOTAL") landlock samples"
echo "    (fewer than --cycles means some cycles errored/timed out --"
echo "     check --uid/--gid have /dev/kvm group membership if this is 0)"

echo
echo "[*] fleet-churn-bench: socket-ready latency (chroot vs landlock)"
if [[ -s "$FCB_CHROOT_TOTAL" && -s "$FCB_LANDLOCK_TOTAL" ]]; then
  "$OPEN_BENCH_BIN" analyze \
    --a "$FCB_CHROOT_TOTAL" --a-label chroot_socket_ready \
    --b "$FCB_LANDLOCK_TOTAL" --b-label landlock_socket_ready \
    | python3 -m json.tool
else
  echo "[!] no successful samples on one or both sides, skipping analyze"
fi

echo
echo "[*] done. raw files in $WORKDIR"
