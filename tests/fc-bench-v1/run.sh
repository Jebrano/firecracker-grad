#!/bin/sh
set -e

MODE=$1
RESULTS_DISK=/dev/vdb
TESTFILE=/tmp/fio_testfile

echo "=== Firecracker Benchmark: $MODE ===" >> $RESULTS_DISK
echo "timestamp=$(date +%s)" >> $RESULTS_DISK

case "$MODE" in

    rand_read)
        fio --name=randread \
            --ioengine=libaio \
            --iodepth=32 \
            --rw=randread \
            --bs=4k \
            --direct=1 \
            --size=256M \
            --filename=$TESTFILE \
            --output-format=json \
            >> $RESULTS_DISK
        ;;

    seq_write)
        fio --name=seqwrite \
            --ioengine=libaio \
            --iodepth=1 \
            --rw=write \
            --bs=128k \
            --direct=1 \
            --size=256M \
            --filename=$TESTFILE \
            --output-format=json \
            >> $RESULTS_DISK
        ;;

    rand_write)
        fio --name=randwrite \
            --ioengine=libaio \
            --iodepth=32 \
            --rw=randwrite \
            --bs=4k \
            --direct=1 \
            --size=256M \
            --filename=$TESTFILE \
            --output-format=json \
            >> $RESULTS_DISK
        ;;

    mixed)
        fio --name=mixed \
            --ioengine=libaio \
            --iodepth=16 \
            --rw=randrw \
            --rwmixread=70 \
            --bs=4k \
            --direct=1 \
            --size=256M \
            --filename=$TESTFILE \
            --output-format=json \
            >> $RESULTS_DISK
        ;;

    *)
        echo "Unknown benchmark mode: $MODE" >> $RESULTS_DISK
        ;;
esac

echo "=== END ===" >> $RESULTS_DISK


# Detect whether libaio is available
if fio --enghelp libaio > /dev/null 2>&1; then
    ENGINE="libaio"
    IODEPTH=32
else
    echo "WARNING: libaio not available, falling back to sync engine" >> $RESULTS_DISK
    ENGINE="sync"
    IODEPTH=1
fi

echo "=== Firecracker Benchmark: $MODE ===" >> $RESULTS_DISK
echo "timestamp=$(date +%s)" >> $RESULTS_DISK
echo "engine=$ENGINE" >> $RESULTS_DISK

case "$MODE" in

    rand_read)
        fio --name=randread \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=randread \
            --bs=4k \
            --direct=1 \
            --size=256M \
            --filename=$TESTFILE \
            --output-format=json \
            >> $RESULTS_DISK
        ;;

    seq_write)
        fio --name=seqwrite \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=write \
            --bs=128k \
            --direct=1 \
            --size=256M \
            --filename=$TESTFILE \
            --output-format=json \
            >> $RESULTS_DISK
        ;;

    rand_write)
        fio --name=randwrite \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=randwrite \
            --bs=4k \
            --direct=1 \
            --size=256M \
            --filename=$TESTFILE \
            --output-format=json \
            >> $RESULTS_DISK
        ;;

    mixed)
        fio --name=mixed \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=randrw \
            --rwmixread=70 \
            --bs=4k \
            --direct=1 \
            --size=256M \
            --filename=$TESTFILE \
            --output-format=json \
            >> $RESULTS_DISK
        ;;

    *)
        echo "Unknown benchmark mode: $MODE" >> $RESULTS_DISK
        ;;
esac

echo "=== END ===" >> $RESULTS_DISK

####

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
