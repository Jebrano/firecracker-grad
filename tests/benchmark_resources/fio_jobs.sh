#!/bin/sh
# V4
# this script will run the fio job on /vdb and write results to the mounted root disk
# We might want to test different engines.
#
MODE=$1
TEST_DISK=/dev/vdb
ITERS=${2:-30}
FORMAT=${3:-json}
ENGINE="libaio"
IODEPTH=32

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
            --time_based \
            --runtime=10 \
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
            --time_based \
            --runtime=10 \
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
            --time_based \
            --runtime=10 \
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
            --time_based \
            --runtime=10 \
            2>/dev/null > /dev/ttyS0
        ;;
    *)
        echo '{"error":"unknown benchmark mode"}' > /dev/ttyS0
        ;;
esac
