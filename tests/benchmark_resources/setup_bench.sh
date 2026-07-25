#!/usr/bin/env bash
# Drives the setup-bench binary (idea #1: isolation setup/teardown phase
# timing), interleaving chroot/landlock cycle-by-cycle to avoid residual
# per-core cache/TLB/branch-predictor state biasing whichever condition
# ran last, then runs Mann-Whitney U on the pooled totals via open-bench.
#
# This replaces an earlier version of this script that invoked
# jailer/landlock-jailer directly with a --bench-setup-only flag. That flag
# never shipped in the final design -- it was replaced by a dedicated
# setup-bench binary (see setup_bench.rs) that reuses jailer's own
# Env::run_setup_only() without adding any benchmark-only flag to the
# production jailer/landlock-jailer binaries or their shared CLI. If you
# still have the old script lying around, replace it with this one.
#
# Usage: sudo ./setup_bench.sh [cycles] <path-to-real-firecracker-binary> [chroot-base-dir]
#
# Requires root (chroot, and Landlock's chown/rule setup touch real paths).

set -euo pipefail

CYCLES="${1:-100}"
EXEC_FILE="${2:?usage: $0 [cycles] <path-to-real-firecracker-binary> [chroot-base-dir]}"
CHROOT_BASE="${3:-/srv/jailer-bench}"
CORE_RANGE="2-3"
JAILER_UID="${JAILER_UID:-6000}"
JAILER_GID="${JAILER_GID:-6000}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SETUP_BENCH_BIN="$SCRIPT_DIR/setup-bench"
OPEN_BENCH_BIN="${OPEN_BENCH_BIN:-$SCRIPT_DIR/open-bench}"

for bin in "$SETUP_BENCH_BIN" "$OPEN_BENCH_BIN"; do
  if [[ ! -x "$bin" ]]; then
    echo "[!] not found or not executable: $bin"
    echo "    (build the jailer crate with 'cargo build --release', and"
    echo "     open-bench per its own instructions, or set OPEN_BENCH_BIN)"
    exit 1
  fi
done

mkdir -p "$CHROOT_BASE"
WORKDIR="$(mktemp -d /tmp/setup-bench.XXXXXX)"
echo "[*] workdir: $WORKDIR"
echo "[*] cycles=$CYCLES exec_file=$EXEC_FILE chroot_base=$CHROOT_BASE uid=$JAILER_UID gid=$JAILER_GID"

# --- isolation, matching your fio / open-bench protocol ---------------------
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

CHROOT_JSONL="$WORKDIR/chroot.jsonl"
LANDLOCK_JSONL="$WORKDIR/landlock.jsonl"
: > "$CHROOT_JSONL"
: > "$LANDLOCK_JSONL"

echo
echo "[*] setup-bench: $CYCLES interleaved cycles"
for i in $(seq 1 "$CYCLES"); do
  printf "\r[*] cycle %d/%d" "$i" "$CYCLES"

  id_chroot="setup-chroot-$i-$RANDOM"
  line=$(sudo taskset -c "$CORE_RANGE" "$SETUP_BENCH_BIN" \
    --isolation chroot --mode setup \
    --id "$id_chroot" --exec-file "$EXEC_FILE" \
    --uid "$JAILER_UID" --gid "$JAILER_GID" \
    --chroot-base-dir "$CHROOT_BASE") || line=""
  [[ -n "$line" ]] && echo "$line" >> "$CHROOT_JSONL"
  sudo rm -rf "${CHROOT_BASE:?}/$exec_file_name/$id_chroot"

  id_landlock="setup-landlock-$i-$RANDOM"
  line=$(sudo taskset -c "$CORE_RANGE" "$SETUP_BENCH_BIN" \
    --isolation landlock --mode setup \
    --id "$id_landlock" --exec-file "$EXEC_FILE" \
    --uid "$JAILER_UID" --gid "$JAILER_GID" \
    --chroot-base-dir "$CHROOT_BASE") || line=""
  [[ -n "$line" ]] && echo "$line" >> "$LANDLOCK_JSONL"
  sudo rm -rf "${CHROOT_BASE:?}/$exec_file_name/$id_landlock"
done
echo
echo "[*] done. $(wc -l < "$CHROOT_JSONL") chroot samples, $(wc -l < "$LANDLOCK_JSONL") landlock samples"
echo "    per-cycle JSON in $CHROOT_JSONL / $LANDLOCK_JSONL"

# Pulls a numeric field out of every JSON line into a flat one-value-per-line
# file, without a jq dependency -- works since the value always immediately
# follows "field":digits in setup-bench's hand-rolled JSON output.
extract_field() {
  local field="$1"
  grep -o "\"${field}\":[0-9]*" | cut -d: -f2
}

echo
echo "[*] total_setup_ns (chroot vs landlock)"
extract_field total_setup_ns < "$CHROOT_JSONL"   > "$WORKDIR/chroot_total.txt"
extract_field total_setup_ns < "$LANDLOCK_JSONL" > "$WORKDIR/landlock_total.txt"
if [[ -s "$WORKDIR/chroot_total.txt" && -s "$WORKDIR/landlock_total.txt" ]]; then
  "$OPEN_BENCH_BIN" analyze \
    --a "$WORKDIR/chroot_total.txt" --a-label chroot_total_setup \
    --b "$WORKDIR/landlock_total.txt" --b-label landlock_total_setup \
    | python3 -m json.tool
else
  echo "[!] no successful samples on one or both sides -- check stderr from"
  echo "    setup-bench above (rerun a single cycle without taskset/sudo"
  echo "    redirection to see it directly)"
fi

# --- per-phase comparisons ---------------------------------------------
# Phase names differ between conditions (chroot has copy_exec_to_chroot/
# chroot_pivot/folder_hierarchy_setup/mknod_*; landlock has
# chown_jail_root/landlock_ruleset_create/landlock_add_rules/
# landlock_restrict_self) -- see env.rs's setup_isolation for the full
# list. Compare any individual phase like this:
echo
echo "[*] example per-phase extraction (edit phase names as needed):"
echo "    grep -o '\"chroot_pivot\":[0-9]*' $CHROOT_JSONL | cut -d: -f2 > chroot_pivot.txt"
echo "    grep -o '\"landlock_restrict_self\":[0-9]*' $LANDLOCK_JSONL | cut -d: -f2 > landlock_restrict.txt"
echo "    $OPEN_BENCH_BIN analyze --a chroot_pivot.txt --a-label chroot_pivot --b landlock_restrict.txt --b-label landlock_restrict"

echo
echo "[*] done. raw files in $WORKDIR"
