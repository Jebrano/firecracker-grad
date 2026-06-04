#!/bin/bash
# this is also to start the VM.


BENCHMARK_MODE=${1:-rand_read}
FC_BINARY=~/fc-bench/firecracker
KERNEL=~/fc-bench/vmlinux-5.10.245
ROOTFS=~/fc-bench/rootfs-baseline.ext4
BENCH_DISK=~/fc-bench/bench-disk.raw
API_SOCKET=/tmp/fc-bench.socket
SERIAL_OUT=~/fc-bench/serial-output.txt
RESULTS=~/fc-bench/results.json
TIMEOUT=120  # seconds before giving up

# Clean up leftovers
rm -f $API_SOCKET $SERIAL_OUT

# Start Firecracker, serial goes to file
$FC_BINARY \
    --api-sock $API_SOCKET \
    --log-path ~/fc-bench/fc.log \
    --level Info \
    > $SERIAL_OUT 2>&1 &

FC_PID=$!
echo "Firecracker PID: $FC_PID"

# Wait for API socket
echo "Waiting for API socket..."
for i in $(seq 1 20); do
    [ -S $API_SOCKET ] && break
    sleep 0.1
done

if [ ! -S $API_SOCKET ]; then
    echo "ERROR: API socket never appeared"
    kill $FC_PID 2>/dev/null
    exit 1
fi

echo "Configuring VM..."

curl -s -X PUT "http://localhost/boot-source" \
    --unix-socket $API_SOCKET \
    -H "Content-Type: application/json" \
    -d "{
        \"kernel_image_path\": \"$KERNEL\",
        \"boot_args\": \"console=ttyS0 reboot=k panic=1 pci=off benchmark=$BENCHMARK_MODE\"
    }"

curl -s -X PUT "http://localhost/drives/rootfs" \
    --unix-socket $API_SOCKET \
    -H "Content-Type: application/json" \
    -d "{
        \"drive_id\": \"rootfs\",
        \"path_on_host\": \"$ROOTFS\",
        \"is_root_device\": true,
        \"is_read_only\": false
    }"

curl -s -X PUT "http://localhost/drives/benchdisk" \
    --unix-socket $API_SOCKET \
    -H "Content-Type: application/json" \
    -d "{
        \"drive_id\": \"benchdisk\",
        \"path_on_host\": \"$BENCH_DISK\",
        \"is_root_device\": false,
        \"is_read_only\": false
    }"

curl -s -X PUT "http://localhost/machine-config" \
    --unix-socket $API_SOCKET \
    -H "Content-Type: application/json" \
    -d '{
        "vcpu_count": 1,
        "mem_size_mib": 512
    }'

echo "Starting VM..."
curl -s -X PUT "http://localhost/actions" \
    --unix-socket $API_SOCKET \
    -H "Content-Type: application/json" \
    -d '{"action_type": "InstanceStart"}'

# -------------------------------------------------------
# Watch serial output for completion marker, don't wait
# for Firecracker to exit on its own - it won't
# -------------------------------------------------------
echo "Waiting for benchmark to complete (timeout: ${TIMEOUT}s)..."

ELAPSED=0
DONE=0
while [ $ELAPSED -lt $TIMEOUT ]; do
    if [ -f $SERIAL_OUT ] && grep -q "===RESULTS_END===" $SERIAL_OUT 2>/dev/null; then
        DONE=1
        break
    fi
    sleep 1
    ELAPSED=$((ELAPSED + 1))

    # Print a dot every 10 seconds so you know it's alive
    if [ $((ELAPSED % 10)) -eq 0 ]; then
        echo "  ...still running (${ELAPSED}s elapsed)"
        # Show last line of serial output so you can see progress
        tail -1 $SERIAL_OUT 2>/dev/null
    fi
done

# Kill Firecracker now that we have what we need
echo "Stopping Firecracker (PID $FC_PID)..."
kill $FC_PID 2>/dev/null
wait $FC_PID 2>/dev/null

if [ $DONE -eq 0 ]; then
    echo "ERROR: Benchmark timed out after ${TIMEOUT}s"
    echo "--- Serial output so far ---"
    cat $SERIAL_OUT
    exit 1
fi

echo "Benchmark completed in ${ELAPSED}s"

# Extract results JSON from between the delimiters
awk '/===RESULTS_START===/,/===RESULTS_END===/' $SERIAL_OUT \
    | grep -v '===RESULTS' \
    > $RESULTS

if [ -s $RESULTS ]; then
    echo "Results captured:"
    python3 -m json.tool $RESULTS
else
    echo "WARNING: No results found between delimiters"
    echo "--- Full serial output ---"
    cat $SERIAL_OUT
fi
