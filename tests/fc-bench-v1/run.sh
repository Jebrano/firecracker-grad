#!/bin/sh
# V3
# this script will run the fio job on /vdb and write results to the mounted root disk

MODE=$1
TEST_DISK=/dev/vdb
RESULTS_FILE=/root/results.json

# Detect libaio
if fio --enghelp libaio > /dev/null 2>&1; then
    ENGINE="libaio"
    IODEPTH=32
else
    echo '{"warning":"libaio not available, using sync"}' > $RESULTS_FILE
    ENGINE="sync"
    IODEPTH=1
fi

echo "Running benchmark: $MODE with engine: $ENGINE" > /dev/console

case "$MODE" in

    rand_read)
        fio --name=randread \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=randread \
            --bs=4k \
            --direct=1 \
            --size=512M \
            --filename=$TEST_DISK \
            --output-format=json \
            --output=$RESULTS_FILE
        ;;

    seq_write)
        fio --name=seqwrite \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=write \
            --bs=128k \
            --direct=1 \
            --size=512M \
            --filename=$TEST_DISK \
            --output-format=json \
            --output=$RESULTS_FILE
        ;;

    rand_write)
        fio --name=randwrite \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=randwrite \
            --bs=4k \
            --direct=1 \
            --size=512M \
            --filename=$TEST_DISK \
            --output-format=json \
            --output=$RESULTS_FILE
        ;;

    mixed)
        fio --name=mixed \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=randrw \
            --rwmixread=70 \
            --bs=4k \
            --direct=1 \
            --size=512M \
            --filename=$TEST_DISK \
            --output-format=json \
            --output=$RESULTS_FILE
        ;;

    *)
        echo '{"error":"unknown benchmark mode"}' > $RESULTS_FILE
        ;;
esac

echo "Benchmark complete" > /dev/console
