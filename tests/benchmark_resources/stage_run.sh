#!/usr/bin/env bash
# Stages kernel/rootfs/bench-disk for one benchmark run and prints the values your
# harness needs -- kernel_image_path, rootfs_path, benchdisk_path, and
# api_sock_host_path (the last one is identical in form for both backends by
# construction: <base_dir>/firecracker/<id>/root/run/firecracker.socket).
#
# Usage:
#   ./stage-run.sh <chroot|landlock> <fc-bench-dir> <id> <mode>
#
# Example:
#   ./stage-run.sh landlock ./fc-bench run-0001 rand_read
set -euo pipefail

BACKEND="${1:?usage: $0 <chroot|landlock> <fc-bench-dir> <id> <mode>}"
FCBENCH="$(cd "${2:?}" && pwd)"   # resolve to an absolute path up front
ID="${3:?}"
MODE="${4:?}"

case "${BACKEND}" in
    chroot)   BASE_DIR="${FCBENCH}/run/chroot" ;;
    landlock) BASE_DIR="${FCBENCH}/run/landlock" ;;
    *) echo "unknown backend '${BACKEND}' (expected chroot|landlock)" >&2; exit 1 ;;
esac

# Both jailer binaries build the same chroot_dir shape from --chroot-base-dir,
# --exec-file, and --id: <base_dir>/<exec_file_name>/<id>/root. Since --exec-file is
# always fc-bench/bin/firecracker for both, the exec_file_name segment is always
# "firecracker" regardless of backend.
JAIL_ROOT="${BASE_DIR}/firecracker/${ID}/root"
mkdir -p "${JAIL_ROOT}"

# Hardlinking (not copying) avoids doubling your image storage per run; falls back to
# a copy if images/ and run/ end up on different filesystems.
link_or_copy() {
    ln -f "$1" "$2" 2>/dev/null || cp "$1" "$2"
}

link_or_copy "${FCBENCH}/images/vmlinux" "${JAIL_ROOT}/vmlinux"
link_or_copy "${FCBENCH}/images/rootfs.ext4" "${JAIL_ROOT}/rootfs.ext4"
link_or_copy "${FCBENCH}/images/bench-${MODE}.ext4" "${JAIL_ROOT}/bench-${MODE}.ext4"

# This is the one thing that actually differs: the chroot jailer's Firecracker sees
# "/" remapped to the jail root (post-pivot_root), so it must be given in-jail paths.
# The Landlock jailer never remaps "/", so it needs the real host path -- which is
# exactly JAIL_ROOT, since that's where we just staged the files.
case "${BACKEND}" in
    chroot)
        echo "kernel_image_path=/vmlinux"
        echo "rootfs_path=/rootfs.ext4"
        echo "benchdisk_path=/bench-${MODE}.ext4"
        ;;
    landlock)
        echo "kernel_image_path=${JAIL_ROOT}/vmlinux"
        echo "rootfs_path=${JAIL_ROOT}/rootfs.ext4"
        echo "benchdisk_path=${JAIL_ROOT}/bench-${MODE}.ext4"
        ;;
esac

# Identical shape for both backends: for chroot, /run is created+chowned inside the
# jail by FOLDER_HIERARCHY and firecracker's --api-sock /run/firecracker.socket lands
# there post-pivot_root; for Landlock, build_api_sock_arg() rewrites that same
# argument to this exact absolute host path before exec. Same --api-sock flag value
# works unmodified for both binaries.
echo "api_sock_host_path=${JAIL_ROOT}/run/firecracker.socket"
echo "jail_root=${JAIL_ROOT}"
