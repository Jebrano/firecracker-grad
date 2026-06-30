#!/bin/bash
# pre-bench.sh — run before every benchmark session (baseline and Landlock)
# Must be run as root or with sudo

set -euo pipefail

echo "=== Hardware Info ==="

# CPU model and count
lscpu | grep -E "Model name|CPU\(s\)|Thread|Core|Socket|NUMA"

# Kernel version (determines Landlock ABI)
uname -r

# Check block device type (ROTA=0 means SSD/NVMe, ROTA=1 means HDD)
echo ""
echo "=== Block Device Check ==="
lsblk -d -o NAME,ROTA,SIZE,MODEL,TRAN
# Inside the guest, also verify: cat /sys/block/vdb/queue/rotational

# Verify you're on the right node (ctx fingerprint comes from first rand_write run)
echo ""
echo "=== Node Identity ==="
echo "Hostname: $(hostname)"
echo "CPU: $(grep 'model name' /proc/cpuinfo | head -1 | cut -d: -f2 | xargs)"

echo ""
echo "=== Applying Isolation ==="

# Stop background services
sudo systemctl stop docker snapd unattended-upgrades \
    multipathd ModemManager apport 2>/dev/null || true

# Kill any residual Firecracker or build processes
sudo pkill -f "firecracker|cargo|rustc" 2>/dev/null || true

# Drop page cache — do this as close to VM boot as possible
sync && echo 3 | sudo tee /proc/sys/vm/drop_caches

# CPU governor
sudo cpupower frequency-set -g performance
echo "Governor: $(cpupower frequency-info -p | grep 'The governor')"

# Disable turbo boost (Intel)
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo
echo "Turbo boost disabled: $(cat /sys/devices/system/cpu/intel_pstate/no_turbo)"

# Disable ASLR
echo 0 | sudo tee /proc/sys/kernel/randomize_va_space

# Disable KSM and swap (reduce memory noise)
echo 0 | sudo tee /sys/kernel/mm/ksm/run 2>/dev/null || true
sudo swapoff -a 2>/dev/null || true

echo ""
echo "=== Verification ==="
echo "Governor:    $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"
echo "No turbo:    $(cat /sys/devices/system/cpu/intel_pstate/no_turbo)"
echo "ASLR:        $(cat /proc/sys/kernel/randomize_va_space)"
echo "Swap:        $(swapon --show 2>/dev/null || echo 'none')"

echo ""
echo "=== Ready. Run harness with: taskset -c 2-3 ==="
