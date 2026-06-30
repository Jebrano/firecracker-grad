#!/bin/sh
# Metadata stress benchmark — measures Landlock's per-open overhead.
# Pre-format /dev/vdb as ext4 on the host before running.
#
# Usage: file_ops.sh [num_files] [file_size_kb]
#   num_files     — how many files to create (default: 5000)
#   file_size_kb  — each file's size in KB (default: 4)

NUM_FILES=${1:-5000}
FILE_SIZE_KB=${2:-4}
TEST_DISK="/dev/vdb"
MOUNT_POINT="/mnt/bench"

# ── helpers ────────────────────────────────────────────────────────

# Read monotonic uptime in seconds (fractional)
now_s() { awk '{print $1+0}' /proc/uptime; }

# Run a phase and return elapsed seconds (printed to stdout)
run_phase() {
    phase_name="$1"
    shift
    t0=$(now_s)
    "$@"
    t1=$(now_s)
    awk "BEGIN {printf \"%.3f\", $t1 - $t0}"
}

# ── mount the pre-formatted ext4 disk ──────────────────────────────

mkdir -p "$MOUNT_POINT"
mount "$TEST_DISK" "$MOUNT_POINT" || {
    echo '{"error":"failed to mount benchmark disk"}' > /dev/ttyS0
    exit 1
}

# Clean up any leftover files from a previous run
rm -rf "$MOUNT_POINT"/file_* 2>/dev/null

# ── phases ─────────────────────────────────────────────────────────

create_phase() {
    for i in $(seq 1 "$NUM_FILES"); do
        dd if=/dev/zero of="$MOUNT_POINT/file_$i" bs=1024 count="$FILE_SIZE_KB" 2>/dev/null
    done
    sync
}

stat_phase() {
    for i in $(seq 1 "$NUM_FILES"); do
        stat "$MOUNT_POINT/file_$i" > /dev/null
    done
}

read_phase() {
    for i in $(seq 1 "$NUM_FILES"); do
        cat "$MOUNT_POINT/file_$i" > /dev/null
    done
}

unlink_phase() {
    for i in $(seq 1 "$NUM_FILES"); do
        rm "$MOUNT_POINT/file_$i"
    done
    sync
}

create_s=$(run_phase "create" create_phase)
stat_s=$(run_phase "stat"   stat_phase)
read_s=$(run_phase "read"   read_phase)
unlink_s=$(run_phase "unlink" unlink_phase)

# ── output results ─────────────────────────────────────────────────

total_s=$(awk "BEGIN {printf \"%.3f\", $create_s + $stat_s + $read_s + $unlink_s}")

cat <<JSON_END > /dev/ttyS0
{
  "benchmark": "file_ops",
  "num_files": $NUM_FILES,
  "file_size_kb": $FILE_SIZE_KB,
  "create_s": $create_s,
  "stat_s": $stat_s,
  "read_s": $read_s,
  "unlink_s": $unlink_s,
  "total_s": $total_s
}
JSON_END

echo "===FIO_JSON_END===" > /dev/ttyS0

umount "$MOUNT_POINT" 2>/dev/null
