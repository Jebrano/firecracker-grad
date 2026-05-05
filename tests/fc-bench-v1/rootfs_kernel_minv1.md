This docmunet will go over everyting we need to install and build to have a minimal working `rootfs` and `kernel`

# Prerequisites on the Host Machine
we need to install all these libraries 
## First install these from apt
```bash
sudo apt update
sudo apt install -y \
    build-essential \
    git \
    unzip \
    wget \
    libncurses-dev \
    libssl-dev \
    bc \
    cpio \
    rsync \
    python3 \
    file \
    bison \
    flex \
    libelf-dev \
    zlib1g-dev \
    libssl-dev \
    libncurses-dev \
    dwarves \
    pkg-config
```
## Get Buildroot
- [ ] look more at why we use this tool, but for now its simple enough, we just clone the whole repo and extract it
```bash
wget https://buildroot.org/downloads/buildroot-2024.02.tar.gz
tar xf buildroot-2024.02.tar.gz
cd buildroot-2024.02
```
## Creating the Buildroot Config File
we use a config file directly that we store inside `configs/`, must be sure to have it version tracked in the main repo.
there is a default version in `firecracker_bench_defconfig`.
in the config we specifie the rootfs size and what the init will be, also if there are other program to include.

## Bnchmark init Script
we need to  inject some file into the `rootfs`, we can do it without mount the image, by using an overlay directory.

we make the following directories
```bash
mkdir -p board/firecracker-bench/rootfs_overlay/etc/init.d
mkdir -p board/firecracker-bench/rootfs_overlay/root/benchmarks
```
Now we create the init script that runs automatically on boot. Buildroot's Busybox init looks for scipts named `S??*` in `/etc/init.d/`:
Here we will make a script named "S99benchmark" that will be inside `init.d`.
look for its content in `board/firecracker-bench/rootfs_overlay/etc/init.d/S99benchmark`, dont forget to set the permission to execute `chmod +x board/firecracker-bench/rootfs_overlay/etc/init.d/S99benchmark`.

and then we need the script that interprets the mod passed via kernel cmdline and runs the appropriate `fio` job. it will found inside `board/firecracker-bench/rootfs_overlay/root/benchmarks/run.sh`, and dont forget to set the permission again.

we design it to write the JSON output directly to `/dev/vdb`, so we avoid needing a network connection or a shared filesystem to extract results. the host can read the raw block device after the guest is powerd off.

### Point Buildroot at the overlay
at this porint we will include the overlay in the `deconfig` file.

### Building it
the first build will take time because buildroot compiles its own cross-toolchain from scratch.
```bash
make firecracker_bench_defconfig
make -j$(nproc)
```
and when it finishes we find the rootfs iamge at `output/images/rootfs.ext4`.

### Resize and prepare the Image
we make sure its properly finished.
```bash
# Verify it looks correct
file output/images/rootfs.ext4
# Should say: Linux rev 1.0 ext4 filesystem data

# Run fsck to make sure it's clean
e2fsck -f output/images/rootfs.ext4

# Copy it to your working directory with a clear name
cp output/images/rootfs.ext4 ~/fc-bench/rootfs-baseline.ext4
```

and we create a small blank disk image for results, this is the `/dev/vdb` our script writes to:
```bash
dd if=/dev/zero of=~/fc-bench/results.ext4 bs=1M count=64
mkfs.ext4 ~/fc-bench/results.ext4
```
## Get a guest kernel
we need a kernel that firecracker can boot, we will buildone and keep in mind we will reconfig it to support landlock.
we can get the recommended config from firecracker own's repo
```bash
# Get the Firecracker repo if you don't have it
git clone https://github.com/firecracker-microvm/firecracker ~/firecracker

# Get the Linux kernel source matching their recommended version
wget https://cdn.kernel.org/pub/linux/kernel/v5.x/linux-5.10.245.tar.xz
tar xf linux-5.10.245.tar.xz
cd linux-5.10.245

# Apply Firecracker's guest kernel config
cp ~/firecracker/resources/guest_configs/microvm-kernel-x86_64-5.10.config .config

# Build just the kernel image (vmlinux)
make vmlinux -j$(nproc)

# Copy the result
cp vmlinux ~/fc-bench/vmlinux-5.10.245
```
## Write the Firecracker VM Config
we write the FC config JSON that refrences our rootfs and kernel into a file called `vm-config-bench.json`, it should be included in the directory that firecracker exits in.
notic the `boot_args` that will select what type of benchmark it will run.

### Test it
we have to make sure our VM can boot an run normally
```bash
./firecracker --no-api --config-file vm-config-bench.json
```
after the VM powers off, mount the results disk and check output
```bash
mkdir -p /tmp/results_mount
sudo mount ~/fc-bench/results.ext4 /tmp/results_mount
cat /tmp/results_mount  # or wherever fio wrote its output
sudo umount /tmp/results_mount
```
- [ ] here we need to work on the api cuz it keep blocking the console in suspended state.
we will see the `fio` results in json format.

# Problems
## FC keep looking for `eth0`
we add a line in `defconf` to disable DHCP discovery, but will we need overlay to disable it correctly, we make a new dir.

buildroot automatically generates an `/etc/network/interfaces` file that include `etc0` with DHCP by default, the `S40entwork` init sciprt will bring it up on everyboot and hangs waiting for a lease.
```bash
mkdir -p board/firecracker-bench/rootfs_overlay/etc/network

cat > board/firecracker-bench/rootfs_overlay/etc/network/interfaces << 'EOF'
# Minimal network config - only loopback
auto lo
iface lo inet loopback
EOF
```
the fix is to override that file through the overlay:
this will make the `s40network` sciprt only bring up `lo`.
then rebuild.

## `libaio` not installed
buildroot includes `fio` but doesn't automatically pull in the async I/O library it depends on. we modify the `defconf`, and the `run.sh` script.
While you're rebuilding, also update your run.sh to fall back to the sync engine if libaio ever fails to load, to make the tests more resilient.
- [ ] make sure the `run.sh` is proper, I have added new lines, don't forget permissions
we can mount the rootfs to make sure libaio is included.
```bash
# Mount the image and check
mkdir -p /tmp/rootfs_check
sudo mount -o loop output/images/rootfs.ext4 /tmp/rootfs_check
ls /tmp/rootfs_check/usr/lib/ | grep libaio
sudo umount /tmp/rootfs_check
```
## Diskspace shortage
Two problems here. First, `fio` is trying to write a 256MB test file to `/tmp` which lives on `vda` (the rootfs), and there isn't enough free space after the OS fills it. Second, writing `fio` test data and results to the same disk (`vdb`) was always going to cause a conflict.
The cleaner fix is to point `fio` directly at `/dev/vdb` as a raw block device — no filesystem needed, and it's actually better benchmark practice since it removes the guest filesystem layer from your measurements, making the I/O path more directly representative of what Landlock sees on the host side. Results then go to a small file on the rootfs instead.
- make vdb larger 
```bash
# 1GB raw disk - no filesystem needed, fio uses it directly
dd if=/dev/zero of=/mydata/fc-bench/bench-disk.raw bs=1M count=1024
```
- [ ]update the benchmark script, check `run.sh` again cus I have modified it again. + permissions
- [ ] update the init script to extract results `S99benchmark` The init script now needs to copy the results file from the rootfs to somewhere the host can read them after shutdown. The simplest way is to write it to serial output with a clear delimiter. + permission
- [ ] Update the firecracker `vm-config`. to point at the new disk and add a serial output file.


- rebuild and run: same as above but run firecacker with these parameters `./firecracker --no-api --config-file vm-config-bench.json \
    | tee /mydata/fc-bench/serial-output.txt`
- extract the resutls for the serial output
```bash
# Parse the results JSON out of the serial log
awk '/===RESULTS_START===/,/===RESULTS_END===/' \
    /mydata/fc-bench/serial-output.txt \
    | grep -v '===RESULTS' \
    > /mydata/fc-bench/results.json

# Pretty print to verify
python3 -m json.tool /mydata/fc-bench/results.json
```
now `vda` is the OS, `vdb` is a dedicated raw block device that fio hammers direcly and resutls comeback over the serial console.

## Fixing Firecracker API
The guest is shutting down correctly — "System halted" confirms that — but Firecracker itself doesn't exit when the guest powers off with --no-api. It just sits there waiting. The fix is to stop using --no-api and switch to the proper API socket approach, which gives Firecracker a clean way to know when to exit.

Instead of --no-api, run Firecracker with a socket and drive it through API calls. Replace your launch approach entirely with this wrapper script:

- [ ] make a new `run_benchmark.sh` script.+ dont forge permission
When the guest runs poweroff, Firecracker receives the KVM shutdown signal and exits the process cleanly — but only when it has an API socket open. With --no-api Firecracker has no shutdown handler registered, so it just stalls after the guest halts.

#### Run it
```bash
# Single benchmark
/mydata/fc-bench/run_benchmark.sh rand_read

# Or loop through all modes for a full measurement pass
for MODE in rand_read rand_write seq_write mixed; do
    echo "=== Running: $MODE ==="
    /mydata/fc-bench/run_benchmark.sh $MODE
    # Brief pause between runs to let the system settle
    sleep 2
done
```
#### Sanity Check that didnt work
```bash
# Temporarily change benchmark= to a no-op to test exit behaviour
curl -s -X PUT "http://localhost/boot-source" \
    --unix-socket /tmp/fc-bench.socket \
    -H "Content-Type: application/json" \
    -d '{
        "kernel_image_path": "/mydata/fc-bench/vmlinux-5.10.245",
        "boot_args": "console=ttyS0 reboot=k panic=1 pci=off"
    }'
```

### Problem still exits
The problem is that Firecracker doesn't automatically exit when the guest halts — it keeps running waiting for API commands. wait $FC_PID blocks forever because the process never exits on its own. The fix is to watch the serial output for your results delimiter and then explicitly kill Firecracker once the benchmark is done.
- [ ] Replaced the waiting section of your script. `run_benchmark.sh`

The key change is replacing wait $FC_PID with a polling loop that checks the serial output file for ===RESULTS_END=== every second. Once the marker appears, it means your benchmark script finished and printed its results, so we immediately kill Firecracker rather than waiting for it to exit on its own.
The progress dots every 10 seconds also tell you it isn't frozen — you'll see the current last line of serial output, which lets you distinguish between "fio is still running" and "something went wrong silently". For a 256MB rand_read benchmark, expect it to take 30–60 seconds on typical CloudLab hardware.

# Using Rust Client and Rest API
Firecracker ships its own OpenAPI spec at src/api_server/swagger/firecracker.yaml. You can generate a fully typed Python client from it:
```bash
# Install the generator
pip install openapi-python-client

# Generate from Firecracker's local spec
openapi-python-client generate \
    --path ~/firecracker/src/api_server/swagger/firecracker.yaml

# This creates a firecracker-client/ directory with a complete typed client
pip install ./firecracker-client
```
but we can use a Rust client
*make a new rust binary with*
- `Cargo.toml`
- `src/fc_client.rs`
- `src/main.rs`

### Build and run
```bash
cargo build --release

# Baseline
./target/release/fc-bench --mode rand-read --iterations 30 \
    --output /mydata/fc-bench/results-baseline.json

# With Landlock (once you have that binary ready)
./target/release/fc-bench --mode rand-read --iterations 30 \
    --landlock \
    --output /mydata/fc-bench/results-landlock.json
```
The --landlock flag just switches which binary gets launched — firecracker vs firecracker-landlock — so your comparison is a single flag flip with identical everything else. When you're ready to do the statistical comparison between the two result files that's a natural next step to add to this crate.
