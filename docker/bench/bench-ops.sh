#!/bin/bash
# wp_cache_* latency and throughput per backend and worker count, inside a
# real WordPress. Each worker is a separate PHP process (like FPM workers).
#   bench-ops.sh [seconds] [workers list] [backends...]
set -euo pipefail
SECS="${1:-10}"
WORKERS="${2:-1,2,4,8,16,32}"
shift 2 || true
read -r -a BACKENDS <<< "${*:-rust redis memcached}"
IFS=',' read -r -a NS <<< "$WORKERS"

cpu_usec() { awk '/^usage_usec/ {print $2}' /sys/fs/cgroup/cpu.stat; }

echo "wp_cache_* benchmark · real WordPress + WooCommerce values · ${SECS}s per run · $(nproc) CPUs"
echo "latency = one wp_cache_get/set call, hrtime around the call; the in-request cache is emptied at every simulated request"
echo
printf "%-10s %7s %11s %9s %9s %9s %9s %9s %9s %9s\n" backend workers "ops/s" "get p50" "get p95" "get p99" "set p50" "set p95" "set p99" "CPU/op"
for b in "${BACKENDS[@]}"; do
  bash /bench/use-backend.sh "$b"
  for n in "${NS[@]}"; do
    rm -rf /tmp/ops-*.json /tmp/ops-barrier
    mkdir -p /tmp/ops-barrier
    for i in $(seq 1 "$n"); do
      BENCH_BARRIER=/tmp/ops-barrier BENCH_WORKERS=$n BENCH_SECONDS=$SECS BENCH_OUT=/tmp/ops-$i.json BENCH_SEED=$i \
        wp --allow-root --path=/var/www/html --skip-plugins=elementor eval-file /bench/bench-ops.php >/dev/null 2>&1 &
    done
    # CPU is sampled over the measured window only: from the moment every
    # worker has passed the barrier.
    while [ "$(find /tmp/ops-barrier -name 'ready-*' | wc -l)" -lt "$n" ]; do sleep 0.01; done
    c0=$(cpu_usec)
    wait
    c1=$(cpu_usec)
    php /bench/merge-ops.php "$b" "$n" "$SECS" "$((c1 - c0))" /tmp/ops-*.json
  done
done
