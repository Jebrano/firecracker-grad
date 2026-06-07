#!/bin/sh

MODE=$1
ITERATIONS=${2:-30}

echo "Starting $ITERATIONS iterations of $MODE" > /dev/console

# Detect engine once, not per iteration
if fio --enghelp libaio > /dev/null 2>&1; then
    ENGINE="libaio"
    IODEPTH=32
else
    ENGINE="sync"
    IODEPTH=1
fi


case "$MODE" in

    rand_read)
        fio --name=randread \
            --ioengine=$ENGINE \
            --iodepth=$IODEPTH \
            --rw=randread \
            --bs=4k \
            --direct=1 \
            --size=512M \
            --filename=/dev/vdb \
            --output-format=json \
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
            --size=512M \
            --filename=/dev/vdb \
            --output-format=json \
            --time_based \
            --runtime=10 \
            2>/dev/null > /dev/ttyS0
        ;;

    seq_write)
        fio --name=seqwrite \
            --ioengine=$ENGINE \
            --iodepth=1 \
            --rw=write \
            --bs=128k \
            --direct=1 \
            --size=512M \
            --filename=/dev/vdb \
            --output-format=json \
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
            --size=512M \
            --filename=/dev/vdb \
            --output-format=json \
            --time_based \
            --runtime=10 \
            2>/dev/null > /dev/ttyS0
        ;;
    *)
        echo "{\"error\": \"unknown mode: $MODE\"}" > /dev/ttyS0
        ;;
esac


# echo "===ALL_DONE===" > /dev/ttyS0
poweroff
