#!/bin/sh
# cat > board/firecracker-bench/rootfs_overlay/root/benchmarks/run.sh << 'EOF'

MODE=$1
RESULTS_DISK=/dev/vdb
RESULTS_FILE=/root/results.json

echo "Running benchmark: $MODE" > /dev/console

case "$MODE" in

    # ── fio jobs ──────────────────────────────────────────────────

    rand_read|rand_write|seq_write|mixed)
        run_fio "$MODE"
        ;;

    # ── sysbench CPU ──────────────────────────────────────────────

    sb_cpu_light)
        sysbench cpu \
            --cpu-max-prime=2000 \
            --threads=1 \
            --time=60 \
            --report-interval=0 \
            run > $RESULTS_FILE
        ;;

    sb_cpu_heavy)
        sysbench cpu \
            --cpu-max-prime=20000 \
            --threads=1 \
            --time=60 \
            --report-interval=0 \
            run > $RESULTS_FILE
        ;;

    sb_cpu_multi)
        sysbench cpu \
            --cpu-max-prime=10000 \
            --threads=4 \
            --time=60 \
            --report-interval=0 \
            run > $RESULTS_FILE
        ;;

    # ── sysbench memory ───────────────────────────────────────────

    sb_mem_read_seq)
        sysbench memory \
            --memory-block-size=1M \
            --memory-total-size=100G \
            --memory-oper=read \
            --memory-access-mode=seq \
            --threads=1 \
            --time=60 \
            run > $RESULTS_FILE
        ;;

    sb_mem_write_seq)
        sysbench memory \
            --memory-block-size=1M \
            --memory-total-size=100G \
            --memory-oper=write \
            --memory-access-mode=seq \
            --threads=1 \
            --time=60 \
            run > $RESULTS_FILE
        ;;

    sb_mem_read_rnd)
        sysbench memory \
            --memory-block-size=4K \
            --memory-total-size=100G \
            --memory-oper=read \
            --memory-access-mode=rnd \
            --threads=1 \
            --time=60 \
            run > $RESULTS_FILE
        ;;

    # ── sysbench threads ──────────────────────────────────────────

    sb_threads_low)
        sysbench threads \
            --threads=4 \
            --thread-yields=100 \
            --thread-locks=2 \
            --time=60 \
            run > $RESULTS_FILE
        ;;

    sb_threads_high)
        sysbench threads \
            --threads=8 \
            --thread-yields=1000 \
            --thread-locks=8 \
            --time=60 \
            run > $RESULTS_FILE
        ;;

    # ── sysbench mutex ────────────────────────────────────────────

    sb_mutex_low)
        sysbench mutex \
            --mutex-num=1 \
            --mutex-locks=50000 \
            --mutex-loops=10000 \
            --threads=2 \
            --time=60 \
            run > $RESULTS_FILE
        ;;

    sb_mutex_high)
        sysbench mutex \
            --mutex-num=4 \
            --mutex-locks=50000 \
            --mutex-loops=10000 \
            --threads=8 \
            --time=60 \
            run > $RESULTS_FILE
        ;;

    *)
        echo "Unknown mode: $MODE" > $RESULTS_FILE
        ;;
esac

echo "===RESULTS_START===" > /dev/ttyS0
cat $RESULTS_FILE         > /dev/ttyS0
echo "===RESULTS_END===" > /dev/ttyS0

poweroff
EOF
