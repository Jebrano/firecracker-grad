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

# Disable turbo boost. Some of our tests are performance tests, and we want minimum variability wrt processor frequency
# See also https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/processor_state_control.html
echo 1 |sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo &> /dev/null

# Force the CPU to continuously stay in the highest, non-turbo P-state. The P-state will determine the
# CPU's clock frequency.
# https://www.kernel.org/doc/html/v4.12/admin-guide/pm/intel_pstate.html
echo 100 |sudo tee /sys/devices/system/cpu/intel_pstate/min_perf_pct &> /dev/null
echo 100 |sudo tee /sys/devices/system/cpu/intel_pstate/max_perf_pct &> /dev/null

# The governor is a linux component that can adjust CPU frequency. "performance" tells it to always run CPUs at
# their maximum safe frequency. It seems to be the default for Amazon Linux, but it doesn't hurt to make this explicit.
# See also https://wiki.archlinux.org/title/CPU_frequency_scaling
echo performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor &> /dev/null

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
