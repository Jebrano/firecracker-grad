#!/bin/sh
# This bash script is for running the benchmarks, printing the results to console, then poweroff.

CMDLINE=$(cat /proc/cmdline)

case "$1" in
    start)
        for param in $CMDLINE; do
            case "$param" in
                benchmark=*)
                    MODE="${param#benchmark=}"
                    echo "===RESULTS_START==="  > /dev/ttyS0
                    /root/benchmarks/fio.sh "$MODE"
                    # Print results to serial with clear delimiters
                    # so the host harness can parse them out
                    echo "===RESULTS_END==="    > /dev/ttyS0
                    poweroff
                    ;;
            esac
        done
        ;;
    stop)
        ;;
esac
