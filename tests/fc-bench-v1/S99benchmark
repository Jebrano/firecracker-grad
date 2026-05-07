#!/bin/sh
# This bash script is for running the benchmarks, printing the results to console, then poweroff.

CMDLINE=$(cat /proc/cmdline)

case "$1" in
    start)
        for param in $CMDLINE; do
            case "$param" in
                benchmark=*)
                    MODE="${param#benchmark=}"
                    /root/benchmarks/run.sh "$MODE"
                    # Print results to serial with clear delimiters
                    # so the host harness can parse them out
                    echo "===RESULTS_START==="  > /dev/ttyS0
                    cat /root/results.json      > /dev/ttyS0
                    echo "===RESULTS_END==="    > /dev/ttyS0
                    poweroff
                    ;;
            esac
        done
        ;;
    stop)
        ;;
esac
