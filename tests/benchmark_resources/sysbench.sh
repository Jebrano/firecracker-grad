#!/bin/sh
# cat > board/firecracker-bench/rootfs_overlay/root/benchmarks/run.sh << 'EOF'

MODE=$1
RESULTS_FILE=/root/results.json

echo "Running benchmark: $MODE" > /dev/console

case "$MODE" in

    # ── fio jobs ──────────────────────────────────────────────────

    rand_read|rand_write|seq_write|mixed)
        /root/benchmarks/fio_jobs.sh "$MODE"
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

    *)
        echo "Unknown mode: $MODE" > $RESULTS_FILE
        ;;
esac

echo "===RESULTS_START===" > /dev/ttyS0
cat $RESULTS_FILE         > /dev/ttyS0
echo "===RESULTS_END===" > /dev/ttyS0

poweroff
EOF
