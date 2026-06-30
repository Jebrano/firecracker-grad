#!/bin/sh
# V7 - with fio built-in logs
# this script will run the fio job on /vdb and write results to the mounted root disk
# We might want to test different engines.
#
MODE=$1
TEST_DISK=/dev/vdb
ITERS=${2:-30}
FORMAT=${3:-json}
ENGINE="io_uring"
IODEPTH=32
LOG_PREFIX="/tmp/fio_bench"

# Parse colon-delimited overrides from MODE (e.g. "rand_read:iters=50")
case "$MODE" in
    *:*)
        MODE_BASE="${MODE%%:*}"
        PARAMS="${MODE#*:}"
        OLD_IFS="$IFS"; IFS=':'
        for pair in $PARAMS; do
            case "$pair" in
                iters=*) ITERS="${pair#iters=}" ;;
            esac
        done
        IFS="$OLD_IFS"
        MODE="$MODE_BASE"
        ;;
esac

# common fio log flags - write time-series logs to /tmp
LOG_FLAGS="--write_iops_log=${LOG_PREFIX}_iops --write_bw_log=${LOG_PREFIX}_bw --write_lat_log=${LOG_PREFIX}_lat"

# fio section
case "$MODE" in

    rand_read)
        fio --name=randread \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=randread \
            --bs=4k \
            --direct=1 \
            --filename=$TEST_DISK \
            --loop="$ITERS" \
            --output-format="$FORMAT" \
            $LOG_FLAGS \
            2>/dev/null > /dev/ttyS0
        ;;

    seq_write)
        fio --name=seqwrite \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=write \
            --bs=128k \
            --direct=1 \
            --filename=$TEST_DISK \
            --output-format="$FORMAT" \
            --loop="$ITERS" \
            $LOG_FLAGS \
            2>/dev/null > /dev/ttyS0
        ;;

    rand_write)
        fio --name=randwrite \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=randwrite \
            --bs=4k \
            --direct=1 \
            --filename=$TEST_DISK \
            --output-format="$FORMAT" \
            --loop="$ITERS" \
            $LOG_FLAGS \
            2>/dev/null > /dev/ttyS0
        ;;

    mixed)
        fio --name=mixed \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=randrw \
            --rwmixread=70 \
            --bs=4k \
            --direct=1 \
            --filename=$TEST_DISK \
            --output-format="$FORMAT" \
            --loop="$ITERS" \
            $LOG_FLAGS \
            2>/dev/null > /dev/ttyS0
        ;;
    *)
        echo '{"error":"unknown benchmark mode"}' > /dev/ttyS0
        exit 1
        ;;
esac

# ── output fio built-in logs after the JSON ────────────────────────
echo "===FIO_JSON_END===" > /dev/ttyS0

# Helper: output a log file (if it exists) wrapped in markers
output_log() {
    tag="$1"
    glob="$2"
    for f in $glob; do
        [ -f "$f" ] || continue
        echo "===FIO_LOG ${tag}===" > /dev/ttyS0
        cat "$f" > /dev/ttyS0
        echo "===FIO_LOG_END===" > /dev/ttyS0
    done
}

output_log "iops" "/tmp/fio_bench_iops*.log"
output_log "bw"   "/tmp/fio_bench_bw*.log"
output_log "lat"  "/tmp/fio_bench_lat*.log"
output_log "clat" "/tmp/fio_bench_clat*.log"
output_log "slat" "/tmp/fio_bench_slat*.log"
