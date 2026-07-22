#!/usr/bin/env bash
# guest_assets_bench.sh — measure the time to copy guest assets (kernel,
# rootfs) into the jail root as part of the isolation setup.  This is the
# cost that chroot *must* pay (because after chroot() the process can't see
# the host filesystem) but Landlock does not.
#
# For the chroot condition we:
#   1. time-stamp before copy
#   2. copy --kernel and --rootfs into the jail root
#   3. invoke setup-bench (which runs Env::setup_isolation)
#   4. emit a single JSON line with: copy_kernel_ns, copy_rootfs_ns,
#      total_copy_ns, plus everything setup-bench already measures
#      (total_setup_ns, and per-phase breakdown under "phases")
#
# For the landlock condition we:
#   1. validate that --kernel / --rootfs exist on the host (no copy needed)
#   2. invoke setup-bench
#   3. emit the same JSON shape but with all copy_*_ns fields set to 0
#      (Landlock pays no copy cost)
#
# Usage:
#   sudo ./guest_assets_bench.sh \
#       --setup-bench <path>       \
#       --exec-file   <path>       \
#       --kernel      <path>       \
#       [--rootfs     <path>]      \
#       [--cycles     N]           \
#       [--chroot-base <dir>]      \
#       [--uid N] [--gid N]

set -euo pipefail

# ── defaults ───────────────────────────────────────────────────────────
CYCLES=100
CHROOT_BASE="${CHROOT_BASE:-/srv/jailer-bench}"
JAILER_UID="${JAILER_UID:-123}"
JAILER_GID="${JAILER_GID:-100}"
CORE_RANGE="2-3"
ROOTFS=""

usage() {
    cat <<EOF
Usage: sudo $0 \\
    --setup-bench <path-to-setup-bench-binary> \\
    --exec-file   <path-to-firecracker-binary>   \\
    --kernel      <path-to-kernel-image>          \\
    [--rootfs     <path-to-rootfs-image>]         \\
    [--cycles     N] [--chroot-base <dir>]        \\
    [--uid N] [--gid N]
EOF
    exit 1
}

# ── parse args ──────────────────────────────────────────────────────────
SETUP_BENCH=""
EXEC_FILE=""
KERNEL=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --setup-bench)  SETUP_BENCH="$2";  shift 2 ;;
        --exec-file)    EXEC_FILE="$2";    shift 2 ;;
        --kernel)       KERNEL="$2";       shift 2 ;;
        --rootfs)       ROOTFS="$2";       shift 2 ;;
        --cycles)       CYCLES="$2";       shift 2 ;;
        --chroot-base)  CHROOT_BASE="$2";  shift 2 ;;
        --uid)          JAILER_UID="$2";   shift 2 ;;
        --gid)          JAILER_GID="$2";   shift 2 ;;
        -h|--help)      usage ;;
        *) echo "Unknown arg: $1"; usage ;;
    esac
done

# ── validate required args ──────────────────────────────────────────────
if [[ -z "$SETUP_BENCH" || -z "$EXEC_FILE" || -z "$KERNEL" ]]; then
    echo "[!] --setup-bench, --exec-file, and --kernel are required"
    usage
fi

for f in "$SETUP_BENCH" "$EXEC_FILE" "$KERNEL"; do
    if [[ ! -f "$f" ]]; then
        echo "[!] file not found: $f"
        exit 1
    fi
done

if [[ -n "$ROOTFS" && ! -f "$ROOTFS" ]]; then
    echo "[!] file not found: $ROOTFS"
    exit 1
fi

# ── preamble ────────────────────────────────────────────────────────────
echo "[*] guest_assets_bench.sh"
echo "    cycles=$CYCLES exec_file=$EXEC_FILE kernel=$KERNEL"
[[ -n "$ROOTFS" ]] && echo "    rootfs=$ROOTFS"
echo "    chroot_base=$CHROOT_BASE uid=$JAILER_UID gid=$JAILER_GID"

mkdir -p "$CHROOT_BASE"
WORKDIR="$(mktemp -d /tmp/guest-assets-bench.XXXXXX)"
echo "[*] workdir: $WORKDIR"

CHROOT_JSONL="$WORKDIR/chroot.jsonl"
LANDLOCK_JSONL="$WORKDIR/landlock.jsonl"
: > "$CHROOT_JSONL"
: > "$LANDLOCK_JSONL"

exec_file_name="$(basename "$EXEC_FILE")"
kernel_file_name="$(basename "$KERNEL")"
rootfs_file_name="$(basename "${ROOTFS:-rootfs.ext4}")"
kernel_size=$(stat -c%s "$KERNEL" 2>/dev/null || echo 0)
rootfs_size=0
[[ -n "$ROOTFS" ]] && rootfs_size=$(stat -c%s "$ROOTFS" 2>/dev/null || echo 0)

echo "[*] kernel: $kernel_file_name ($kernel_size bytes)"
[[ -n "$ROOTFS" ]] && echo "[*] rootfs: $rootfs_file_name ($rootfs_size bytes)"

# ── isolation prep ──────────────────────────────────────────────────────
echo "[*] pinning CPU governor to performance"
sudo cpupower frequency-set -g performance >/dev/null 2>&1 || true

echo "[*] disabling turbo"
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo >/dev/null 2>&1 || \
  echo 0 | sudo tee /sys/devices/system/cpu/cpufreq/boost >/dev/null 2>&1 || true

echo "[*] dropping caches"
sync
echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null

# ── helper: high-resolution monotonic ns since boot ─────────────────────
mono_ns() {
    local ns
    if ns=$(awk '/now at/ {print $3; exit}' /proc/timer_list 2>/dev/null); then
        echo "$ns"
    else
        echo $(($(date +%s%N)))
    fi
}

# ── helper: emit one combined JSON line ─────────────────────────────────
emit_json() {
    local condition="$1" copy_kernel_ns="$2" copy_rootfs_ns="$3"
    local total_copy_ns="$4" setup_json="$5"

    local injected
    injected="${setup_json%\}}"
    injected="${injected},\"copy_kernel_ns\":${copy_kernel_ns}"
    injected="${injected},\"copy_rootfs_ns\":${copy_rootfs_ns}"
    injected="${injected},\"total_copy_ns\":${total_copy_ns}"
    injected="${injected}}"
    echo "$injected"
}

# ── main loop ───────────────────────────────────────────────────────────
echo
echo "[*] running $CYCLES interleaved cycles"

for i in $(seq 1 "$CYCLES"); do
    printf "\r[*] cycle %d/%d" "$i" "$CYCLES"

    # ── chroot condition ────────────────────────────────────────────────
    id_chroot="ga-chroot-$i-$RANDOM"
    jail_root="${CHROOT_BASE:?}/${exec_file_name}/$id_chroot/root"

    sudo mkdir -p "$jail_root"

    # Time the kernel copy (must happen before setup-bench, which does the
    # chroot and can't see outside afterwards).
    t0=$(mono_ns)
    sudo cp "$KERNEL" "$jail_root/$kernel_file_name"
    t1=$(mono_ns)
    copy_kernel_ns=$((t1 - t0))
    sudo chown "${JAILER_UID}:${JAILER_GID}" "$jail_root/$kernel_file_name"

    copy_rootfs_ns=0
    if [[ -n "$ROOTFS" ]]; then
        t0=$(mono_ns)
        sudo cp "$ROOTFS" "$jail_root/$rootfs_file_name"
        t1=$(mono_ns)
        copy_rootfs_ns=$((t1 - t0))
        sudo chown "${JAILER_UID}:${JAILER_GID}" "$jail_root/$rootfs_file_name"
    fi
    total_copy_ns=$((copy_kernel_ns + copy_rootfs_ns))

    # Now run setup-bench for the chroot isolation.
    line=$(sudo taskset -c "$CORE_RANGE" "$SETUP_BENCH" \
        --isolation chroot \
        --id "$id_chroot" --exec-file "$EXEC_FILE" \
        --uid "$JAILER_UID" --gid "$JAILER_GID" \
        --chroot-base-dir "$CHROOT_BASE" \
        2>/dev/null) || line=""

    if [[ -n "$line" ]]; then
        emit_json "chroot" "$copy_kernel_ns" "$copy_rootfs_ns" \
                  "$total_copy_ns" "$line" >> "$CHROOT_JSONL"
    fi

    sudo rm -rf "${CHROOT_BASE:?}/${exec_file_name}/$id_chroot"

    # ── landlock condition ──────────────────────────────────────────────
    id_landlock="ga-landlock-$i-$RANDOM"

    # Landlock: no copy needed. setup-bench applies the Landlock ruleset;
    # kernel/rootfs are accessed at their original host paths.
    line=$(sudo taskset -c "$CORE_RANGE" "$SETUP_BENCH" \
        --isolation landlock \
        --id "$id_landlock" --exec-file "$EXEC_FILE" \
        --uid "$JAILER_UID" --gid "$JAILER_GID" \
        --chroot-base-dir "$CHROOT_BASE" \
        2>/dev/null) || line=""

    if [[ -n "$line" ]]; then
        emit_json "landlock" "0" "0" "0" "$line" >> "$LANDLOCK_JSONL"
    fi

    sudo rm -rf "${CHROOT_BASE:?}/${exec_file_name}/$id_landlock"
done

echo
echo "[*] chroot:   $(wc -l < "$CHROOT_JSONL") samples"
echo "[*] landlock: $(wc -l < "$LANDLOCK_JSONL") samples"

# ── per-field extraction helper (no jq dependency) ──────────────────────
extract_field() {
    local field="$1"
    grep -o "\"${field}\":[0-9]*" | cut -d: -f2
}

# ── summary ─────────────────────────────────────────────────────────────
OPEN_BENCH_BIN="${OPEN_BENCH_BIN:-$SCRIPT_DIR/../open-bench/target/release/open-bench}"

analyze_field() {
    local field="$1" label="$2"
    echo
    echo "--- $label ($field) ---"

    extract_field "$field" < "$CHROOT_JSONL"   > "$WORKDIR/chroot_${label}.txt"
    extract_field "$field" < "$LANDLOCK_JSONL" > "$WORKDIR/landlock_${label}.txt"

    if [[ -s "$WORKDIR/chroot_${label}.txt" && -s "$WORKDIR/landlock_${label}.txt" ]]; then
        chroot_mean=$(awk '{s+=$1;n++} END {if(n) printf "%.0f", s/n}' "$WORKDIR/chroot_${label}.txt")
        landlock_mean=$(awk '{s+=$1;n++} END {if(n) printf "%.0f", s/n}' "$WORKDIR/landlock_${label}.txt")
        echo "    chroot mean:   ${chroot_mean} ns"
        echo "    landlock mean: ${landlock_mean} ns"

        if [[ -x "$OPEN_BENCH_BIN" ]]; then
            "$OPEN_BENCH_BIN" analyze \
                --a "$WORKDIR/chroot_${label}.txt" --a-label "chroot_${label}" \
                --b "$WORKDIR/landlock_${label}.txt" --b-label "landlock_${label}" \
                | python3 -m json.tool 2>/dev/null || true
        fi
    else
        echo "    [!] insufficient samples on one or both sides"
    fi
}

# ── analyze key fields ──────────────────────────────────────────────────
analyze_field "copy_kernel_ns"   "copy_kernel"
analyze_field "copy_rootfs_ns"   "copy_rootfs"
analyze_field "total_copy_ns"    "total_copy"
analyze_field "total_setup_ns"   "isolation_setup"

# Grand total: copy + isolation setup. For chroot this includes the copy
# cost; for landlock the copy cost is always 0.
echo
echo "[*] computing grand totals (total_copy_ns + total_setup_ns) per sample …"
paste <(extract_field total_copy_ns < "$CHROOT_JSONL") \
      <(extract_field total_setup_ns < "$CHROOT_JSONL") \
    | awk '{print $1 + $2}' > "$WORKDIR/chroot_grand_total.txt"
paste <(extract_field total_copy_ns < "$LANDLOCK_JSONL") \
      <(extract_field total_setup_ns < "$LANDLOCK_JSONL") \
    | awk '{print $1 + $2}' > "$WORKDIR/landlock_grand_total.txt"

if [[ -s "$WORKDIR/chroot_grand_total.txt" && -s "$WORKDIR/landlock_grand_total.txt" ]]; then
    chroot_gt=$(awk '{s+=$1;n++} END {if(n) printf "%.0f", s/n}' "$WORKDIR/chroot_grand_total.txt")
    landlock_gt=$(awk '{s+=$1;n++} END {if(n) printf "%.0f", s/n}' "$WORKDIR/landlock_grand_total.txt")
    echo
    echo "--- grand_total (copy + isolation) ---"
    echo "    chroot grand total mean:   ${chroot_gt} ns"
    echo "    landlock grand total mean: ${landlock_gt} ns"

    if [[ -x "$OPEN_BENCH_BIN" ]]; then
        "$OPEN_BENCH_BIN" analyze \
            --a "$WORKDIR/chroot_grand_total.txt" --a-label "chroot_grand_total" \
            --b "$WORKDIR/landlock_grand_total.txt" --b-label "landlock_grand_total" \
            | python3 -m json.tool 2>/dev/null || true
    fi
fi

echo
echo "[*] done. raw JSONL files in $WORKDIR"
