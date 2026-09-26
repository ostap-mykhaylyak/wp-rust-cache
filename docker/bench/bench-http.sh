#!/bin/bash
# End-to-end: nginx + PHP-FPM + WordPress with each object cache in turn,
# same URLs, same load.
#   bench-http.sh SCENARIO [seconds] [connections] [backends...]
# SCENARIO: full = WooCommerce + Elementor active, all URLs
#           core = those plugins deactivated, posts/categories/home only
set -euo pipefail
SCENARIO="${1:-full}"
SECONDS_RUN="${2:-30}"
CONN="${3:-32}"
shift 3 || true
read -r -a BACKENDS <<< "${*:-none rust redis memcached}"
WP="wp --allow-root --path=/var/www/html --skip-plugins=elementor"

export BENCH_URLS=/var/www/html/bench-urls.txt
if [ "$SCENARIO" = core ]; then
  wp --allow-root --path=/var/www/html plugin deactivate woocommerce elementor --quiet >/dev/null 2>&1 || true
  grep -E '^/$|^/post-|^/category/' /var/www/html/bench-urls.txt > /var/www/html/bench-urls-core.txt
  export BENCH_URLS=/var/www/html/bench-urls-core.txt
  trap 'wp --allow-root --path=/var/www/html plugin activate woocommerce elementor --quiet >/dev/null 2>&1' EXIT
fi

cpu_usec() { awk '/^usage_usec/ {print $2}' /sys/fs/cgroup/cpu.stat; }

printf "HTTP benchmark · scenario=%s · %s URLs · %ss per backend · %s connections · 16 PHP-FPM workers · %s CPUs\n" \
  "$SCENARIO" "$(wc -l < "$BENCH_URLS")" "$SECONDS_RUN" "$CONN" "$(nproc)"
printf "latency = full response time (wrk); TTFB = unloaded time to first byte (curl, 200 sequential requests); CPU = whole server incl. MariaDB and the cache daemon\n\n"
printf "%-10s %9s %10s %10s %10s %10s %10s %7s %s\n" backend "req/s" p50 p95 p99 "TTFB p50" "CPU/req" errors "cache memory"
for b in "${BACKENDS[@]}"; do
  bash /bench/use-backend.sh "$b" >/dev/null 2>&1
  # Warm-up: fill the object cache and OPcache.
  wrk -t4 -c"$CONN" -d10s -s /bench/urls.lua http://127.0.0.1:80/ >/dev/null
  ttfb=$(shuf -n 200 --random-source=<(yes) "$BENCH_URLS" | while read -r u; do
    curl -s -o /dev/null -w '%{time_starttransfer}\n' "http://127.0.0.1$u"
  done | sort -n | awk '{a[NR]=$1} END {printf "%.1fms", a[int(NR/2)]*1000}')
  c0=$(cpu_usec)
  out=$(wrk -t4 -c"$CONN" -d"${SECONDS_RUN}s" -s /bench/urls.lua http://127.0.0.1:80/ | grep RESULT)
  c1=$(cpu_usec)
  eval "$(echo "$out" | sed 's/RESULT //; s/ /;/g')"
  case "$b" in
    rust) mem=$(wp-rust-cache stats --json | php -r '$s=json_decode(stream_get_contents(STDIN),true); printf("%.1fMB", $s["allocated_bytes"]/1048576);') ;;
    redis) mem=$(redis-cli -s /run/redis/redis.sock info memory | awk -F: '/^used_memory:/ {printf "%.1fMB", $2/1048576}') ;;
    memcached) mem=$(php -r '$m=new Memcached();$m->addServer("127.0.0.1",11211);$s=current($m->getStats());printf("%.1fMB",$s["bytes"]/1048576);') ;;
    *) mem="-" ;;
  esac
  php -r '
    [$b,$req,$dur,$p50,$p95,$p99,$ttfb,$cpu,$err,$mem] = array_slice($argv, 1);
    printf("%-10s %9.1f %8.1fms %8.1fms %8.1fms %10s %8.1fms %7s %s\n", $b, $req/($dur/1e6), $p50/1000, $p95/1000, $p99/1000, $ttfb, $cpu/1000/$req, $err, $mem);
  ' "$b" "$requests" "$duration_us" "$p50" "$p95" "$p99" "$ttfb" "$((c1 - c0))" "$((errors + non2xx))" "$mem"
done
