
## Prompt:
about setting up my development environment, I want to write the test and scripts on my local laptop and build/run them on the cloudlab instance, do I have to install all the building tools and binary like rust/rust analyzer and such every time i need to instantiate my instance? if I want to ssh into the source code folder inside the cloudlab instance using zed editor, keep in my I already setup a development repo on github.

## Response:
No, you don't have to reinstall manually every time. CloudLab has two mechanisms that together solve this completely: **startup scripts** that run automatically on every new instance, and **persistent datasets** that survive experiment termination. Combined with Zed's SSH remote, your workflow becomes: spin up instance → everything is ready → open Zed → start working.

* * *

### Part 1: Persistent Storage for Your Toolchain

The first problem is that your Rust toolchain, built binaries, and large files like kernel images and rootfs shouldn't be reinstalled or rebuilt every time. CloudLab lets you create a persistent dataset that gets mounted on every new experiment.

Go to the CloudLab portal, navigate to **Experiments → Create Dataset**, and create a dataset named something like `fc-dev`. Choose your cluster (Utah if that's what you're using) and allocate around 50GB. This storage survives experiment termination indefinitely.

Then in your experiment profile, mount it at `/mydata` — which you're already using. Everything under `/mydata` will persist across instances automatically.

* * *

### Part 2: The Startup Script

CloudLab lets you attach a startup script to your experiment profile. This runs automatically every time a node boots. Write it once, never manually install anything again.

Create this file in your GitHub repo as `cloudlab/setup.sh`:

bash

```bash
#!/bin/bash
set -e

LOG=/mydata/setup.log
exec > >(tee -a $LOG) 2>&1

echo "=== CloudLab setup started at $(date) ==="

# ── System packages ───────────────────────────────────────────────

sudo apt-get update -qq
sudo apt-get install -y \
    build-essential git curl wget \
    libssl-dev libelf-dev zlib1g-dev \
    libncurses-dev dwarves pkg-config \
    bc cpio rsync bison flex \
    python3 python3-pip \
    unzip file \
    linux-tools-common \
    linux-tools-$(uname -r) \
    e2fsprogs

# ── Rust ─────────────────────────────────────────────────────────
# Install to /mydata so it persists across experiments

export RUSTUP_HOME=/mydata/rustup
export CARGO_HOME=/mydata/cargo

if [ ! -f "$CARGO_HOME/bin/cargo" ]; then
    echo "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y \
            --no-modify-path \
            --default-toolchain stable
else
    echo "Rust already installed, skipping"
fi

# Make Rust available system-wide for all SSH sessions
cat > /etc/profile.d/rust.sh << 'EOF'
export RUSTUP_HOME=/mydata/rustup
export CARGO_HOME=/mydata/cargo
export PATH=$CARGO_HOME/bin:$PATH
EOF
chmod +x /etc/profile.d/rust.sh
source /etc/profile.d/rust.sh

# ── Rust Analyzer ────────────────────────────────────────────────

if [ ! -f "$CARGO_HOME/bin/rust-analyzer" ]; then
    echo "Installing rust-analyzer..."
    rustup component add rust-analyzer rust-src
fi

# ── KVM setup ────────────────────────────────────────────────────

sudo modprobe kvm kvm_intel || sudo modprobe kvm kvm_amd || true
sudo chmod 666 /dev/kvm
# Make this survive reboots
echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666"' \
    | sudo tee /etc/udev/rules.d/99-kvm.rules

# ── CPU performance mode ──────────────────────────────────────────

sudo cpupower frequency-set -g performance || true
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo || true
echo 0 | sudo tee /proc/sys/kernel/randomize_va_space

# ── Clone / update your repo ──────────────────────────────────────

REPO_DIR=/mydata/fc-landlock

if [ ! -d "$REPO_DIR/.git" ]; then
    echo "Cloning repository..."
    git clone https://github.com/YOUR_USERNAME/YOUR_REPO.git $REPO_DIR
else
    echo "Repo exists, pulling latest..."
    git -C $REPO_DIR pull
fi

# ── Build the benchmark harness ───────────────────────────────────

cd $REPO_DIR/fc-bench
source /etc/profile.d/rust.sh
cargo build --release 2>&1

# ── Create workspace dirs if they don't exist ─────────────────────

mkdir -p /mydata/fc-bench

echo "=== Setup complete at $(date) ==="
```

Commit this to your repo:

bash

```bash
git add cloudlab/setup.sh
git commit -m "Add CloudLab setup script"
git push
```

Then in the CloudLab experiment profile, set the startup command to:

```
/bin/bash -c "curl -sSf https://raw.githubusercontent.com/YOUR_USERNAME/YOUR_REPO/main/cloudlab/setup.sh | bash"
```

This bootstraps itself — it pulls and runs the script from GitHub, which then clones the full repo. On subsequent instances, if `/mydata` already has Rust installed it skips that step, so re-instantiation is fast.

* * *

### Part 3: Zed SSH Remote

Zed has built-in SSH remote development. First set up your SSH config on your laptop so you don't have to remember CloudLab's hostname every time:

```
# ~/.ssh/config

Host cloudlab
    HostName YOUR_NODE.cloudlab.us    # update each new experiment
    User YOUR_CLOUDLAB_USERNAME
    IdentityFile ~/.ssh/id_ed25519    # your CloudLab key
    ForwardAgent yes
    ServerAliveInterval 60
    ServerAliveCountMax 10
```

The only thing that changes between experiments is the hostname — one line to update. Then in Zed:

```
cmd + shift + p  →  "ssh"  →  "Connect to SSH Host"
→  cloudlab
→  Open Folder  →  /mydata/fc-landlock
```

Zed automatically installs its server component on the remote, sets up rust-analyzer using whatever is on the remote's `PATH`, and gives you full IDE features — completions, go-to-definition, inline errors — all running on the CloudLab node against the actual build environment.

* * *

### Part 4: Daily Workflow

Once this is all set up, your day-to-day loop is:

```
1. Instantiate CloudLab experiment
   └── Startup script runs automatically (~3 min first time, ~30s after)

2. Update ~/.ssh/config with new hostname
   └── (30 seconds)

3. Open Zed → Connect to SSH Host → cloudlab
   └── Full IDE, editing files on /mydata/fc-landlock

4. Write code in Zed, build in the integrated terminal:
   cd /mydata/fc-landlock/fc-bench
   cargo build --release

5. Commit and push from either laptop or CloudLab terminal:
   git push origin main

6. Terminate experiment when done
   └── /mydata persists, your work is safe
```

The key insight is that by installing Rust to `/mydata/cargo` instead of the default `~/.cargo`, the toolchain lives on the persistent dataset rather than the ephemeral home directory. The first time you ever set this up takes maybe 10 minutes. Every experiment after that, the startup script sees Rust already installed and skips it — the whole setup completes in under a minute.

## Prompt:
what if I dont want to have persistant storage and no startup scripts, and right now I only need to write the benchmarking scripts, can I write them locally and for each run it will upload it and run it ?

## Response:
Yes, that's actually the simplest possible setup. You write code locally in Zed, and a single command syncs and runs it on CloudLab. No persistent storage, no startup scripts.

### One-Time Setup on a Fresh CloudLab Instance

Each time you get a new instance, SSH in and run this once:

bash

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# System deps
sudo apt-get install -y libssl-dev pkg-config

# KVM
sudo modprobe kvm kvm_intel
sudo chmod 666 /dev/kvm

# Clone your repo
git clone https://github.com/YOUR_USERNAME/YOUR_REPO.git ~/fc-landlock
```

This takes about 3 minutes. You only do it when you get a new node.

* * *

### The Daily Loop

From your laptop, just push and run:

bash

```bash
# Write code locally in Zed, then when ready to test:

# 1. Push to GitHub
git push origin main

# 2. Pull and run on CloudLab in one command
ssh cloudlab "
    cd ~/fc-landlock/fc-bench &&
    git pull &&
    cargo run --release -- --mode rand-read --iterations 5
"
```

Or if you want results back on your laptop automatically:

bash

```bash
# Run benchmark and copy results back
ssh cloudlab "
    cd ~/fc-landlock/fc-bench &&
    git pull &&
    cargo run --release -- --mode rand-read --iterations 5 \
        --output ~/results.json
" && scp cloudlab:~/results.json ./results/$(date +%Y%m%d-%H%M%S).json
```

* * *

### Make It a Single Command

Add this to your laptop's shell config (`~/.zshrc` or `~/.bashrc`) so you just type `bench`:

bash

```bash
bench() {
    local MODE=${1:-rand-read}
    local ITER=${2:-10}
    local TIMESTAMP=$(date +%Y%m%d-%H%M%S)

    echo "Pushing to GitHub..."
    git push origin main

    echo "Running on CloudLab: mode=$MODE iterations=$ITER"
    ssh cloudlab "
        cd ~/fc-landlock/fc-bench &&
        git pull --quiet &&
        cargo run --release --quiet -- \
            --mode $MODE \
            --iterations $ITER \
            --output ~/results.json
    "

    echo "Fetching results..."
    mkdir -p ./results
    scp -q cloudlab:~/results.json ./results/${MODE}-${TIMESTAMP}.json
    echo "Saved to results/${MODE}-${TIMESTAMP}.json"
}
```

Then from anywhere in your repo:

bash

```bash
bench rand-read 30
bench seq-write 10
```

That's the entire workflow — write locally, `bench`, get results back as a local JSON file. No infrastructure beyond a working SSH connection and your GitHub repo.

## Prompt:
wont installing my whole development environment effect the benchmarking results?

## Response:
Yes, absolutely it will. You identified a real problem. Everything you install on the CloudLab node — Rust, the compiler, build tools — shares the same CPU, memory, and I/O with Firecracker during benchmarks, introducing noise.

The solution is simple: **separate the build machine from the benchmark machine**. CloudLab lets you request multiple nodes in a single experiment, so you use one node to build and one node that stays completely clean just for running benchmarks.

* * *

### The Setup

In your CloudLab profile, request two nodes:

```
build-node   ← git pull, cargo build, all your tools live here
bench-node   ← nothing installed, only runs Firecracker
```

They share a private experiment network so you can copy binaries between them easily.

* * *

### The Workflow

bash

```bash
bench() {
    local MODE=${1:-rand-read}
    local ITER=${2:-10}
    local TIMESTAMP=$(date +%Y%m%d-%H%M%S)

    # 1. Push latest code
    echo "Pushing..."
    git push origin main

    # 2. Build on the build node
    echo "Building on build-node..."
    ssh build-node "
        cd ~/fc-landlock/fc-bench &&
        git pull --quiet &&
        cargo build --release --quiet
    "

    # 3. Copy the binary to the bench node
    #    Uses the private experiment network, not the internet
    echo "Deploying to bench-node..."
    ssh build-node "
        scp ~/fc-landlock/fc-bench/target/release/fc-bench \
            bench-node:~/fc-bench
    "

    # 4. Run benchmark on the completely clean bench node
    echo "Benchmarking..."
    ssh bench-node "
        cd ~/fc-bench &&
        ./fc-bench --mode $MODE --iterations $ITER \
            --output ~/results.json
    "

    # 5. Fetch results to your laptop
    echo "Fetching results..."
    mkdir -p ./results
    scp -q bench-node:~/results.json \
        ./results/${MODE}-${TIMESTAMP}.json

    echo "Done → results/${MODE}-${TIMESTAMP}.json"
}
```

* * *

### What Goes on Each Node

**build-node** — install everything freely:

bash

```bash
# Rust, cargo, rust-analyzer, git, build deps
# None of this matters for performance
```

**bench-node** — absolute minimum:

bash

```bash
# One-time only:
sudo modprobe kvm kvm_intel
sudo chmod 666 /dev/kvm
sudo cpupower frequency-set -g performance
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo
echo 0 | sudo tee /proc/sys/kernel/randomize_va_space

# Copy over from build-node:
# - fc-bench binary
# - firecracker binary
# - vmlinux
# - rootfs.ext4
# - bench-disk.raw
```

Nothing else ever touches bench-node. No compiler, no package manager running in the background, no git operations — just your benchmark binary and Firecracker.

This also makes your results more defensible during your graduation defence, since you can state clearly that the benchmark machine ran nothing except the experiment itself.

## Prompt:
also what if i dont want to use two seperate nodes, can't I reset the node to a minimal state?

## Response:
Yes, a single node works fine as long as you separate the two phases in time rather than space. The concern with one node was the Rust compiler and build tools running concurrently with Firecracker during measurements. If you finish all building first and then reset to a clean state before measuring, you get the same result.

### The Reset Script

Run this once after all building is done and before any benchmark iteration:

bash

```bash
#!/bin/bash
# reset_bench.sh - call this before every measurement session

set -e

echo "=== Resetting node to minimal benchmark state ==="

# ── 1. Kill everything non-essential ─────────────────────────────

# Stop build-time services you don't need during benchmarks
sudo systemctl stop docker          2>/dev/null || true
sudo systemctl stop snapd           2>/dev/null || true
sudo systemctl stop unattended-upgrades 2>/dev/null || true
sudo systemctl stop multipathd      2>/dev/null || true
sudo systemctl stop ModemManager    2>/dev/null || true
sudo systemctl stop apport          2>/dev/null || true

# Kill any leftover Firecracker or cargo processes from previous runs
sudo pkill -f firecracker           2>/dev/null || true
sudo pkill -f "cargo"               2>/dev/null || true
sudo pkill -f "rustc"               2>/dev/null || true

# ── 2. Drop all caches ────────────────────────────────────────────
# Forces kernel to release page cache, dentries, inodes.
# Prevents a cached warm run from making your first iteration
# look faster than subsequent ones.

sync
echo 3 | sudo tee /proc/sys/vm/drop_caches > /dev/null

# ── 3. CPU configuration ──────────────────────────────────────────

# Performance governor - no frequency scaling during measurements
sudo cpupower frequency-set -g performance 2>/dev/null || true

# Disable turbo boost - prevents variable clock speeds between iterations
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo \
    2>/dev/null || true

# Disable address space randomisation - reduces measurement variance
echo 0 | sudo tee /proc/sys/kernel/randomize_va_space

# ── 4. Pin benchmark to isolated CPUs ────────────────────────────
# Reserve CPUs 0-1 for the OS and everything else.
# Firecracker and the harness run on CPUs 2-3 exclusively.
# Adjust based on your CloudLab node's CPU count.

BENCH_CPUS="2-3"
OS_CPUS="0-1"

# Move all existing tasks to OS CPUs
for pid in $(ls /proc | grep -E '^[0-9]+$'); do
    sudo taskset -a -p \
        $(cat /sys/devices/system/cpu/cpu0/cpumap/cpumap 2>/dev/null || echo 3) \
        $pid 2>/dev/null || true
done

echo "Benchmark CPUs reserved: $BENCH_CPUS"

# ── 5. Verify state ───────────────────────────────────────────────

echo ""
echo "=== Verification ==="
echo "CPU governor:    $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)"
echo "Turbo boost off: $(cat /sys/devices/system/cpu/intel_pstate/no_turbo)"
echo "ASLR disabled:   $(cat /proc/sys/kernel/randomize_va_space)"
echo "Page cache:      $(free -h | grep Mem | awk '{print $6}') cached"
echo "Load average:    $(uptime | awk -F'load average:' '{print $2}')"
echo ""
echo "=== Running processes (non-kernel) ==="
ps aux --no-header | grep -v '\[' | awk '{print $11}' | sort -u | head -20
echo ""
echo "Reset complete. Safe to benchmark."
```

### Running Firecracker on the Reserved CPUs

When you launch Firecracker via your jailer, pin it to the reserved CPUs using `taskset`. Add this to your `start_firecracker` function in the harness:

rust

```rust
fn start_firecracker(cfg: &Config) -> Result<Child> {
    let serial_file = fs::File::create(&cfg.serial_out)?;
    let log_file    = fs::File::create(&cfg.fc_log)?;

    // Pin Firecracker to CPUs 2-3, away from OS noise on 0-1
    let child = Command::new("taskset")
        .args([
            "--cpu-list", "2-3",
            cfg.fc_binary.to_str().unwrap(),
            "--api-sock", cfg.socket.to_str().unwrap(),
            "--log-path", cfg.fc_log.to_str().unwrap(),
            "--level",    "Info",
        ])
        .stdout(serial_file)
        .stderr(log_file)
        .spawn()?;

    Ok(child)
}
```

### The Single-Node Workflow

Your daily loop becomes:

bash

```bash
# Phase 1 — build (do this as much as you need, no restrictions)
cd ~/fc-landlock/fc-bench
git pull
cargo build --release

# Build rootfs, kernel, etc. as needed

# ─────────────────────────────────────────────────────────────────
# Phase 2 — benchmark (run reset first, then don't touch the build)
# ─────────────────────────────────────────────────────────────────
sudo bash ~/fc-landlock/scripts/reset_bench.sh

# Verify load is low before starting
uptime   # want load average < 0.1

# Run measurements
taskset --cpu-list 2-3 \
    ./target/release/fc-bench \
        --mode rand-read \
        --iterations 30 \
        --output /mydata/results/baseline-rand-read.json
```

### One Honest Tradeoff to Note in Your Thesis

Two nodes gives you guaranteed hardware separation. One node with CPU pinning gives you logical separation — the OS and background processes are on CPUs 0-1, Firecracker is on CPUs 2-3, but they still share the L3 cache and memory bus. Under high I/O load, bus contention from background processes can occasionally spike your measurements.

In practice, after the reset script runs, background activity drops to near zero and the shared bus is not a real problem. But mention it in your methodology section — something like "measurements were taken on a single node with CPU affinity isolation and all non-essential services stopped, with shared L3 cache acknowledged as a minor source of variance." That one sentence shows your committee you understand the tradeoff and chose it deliberately rather than overlooked it.
