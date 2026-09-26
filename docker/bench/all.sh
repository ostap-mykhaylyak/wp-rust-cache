#!/bin/bash
# Every check and benchmark, in order, into /results. Takes about an hour.
set -uo pipefail
mkdir -p /results
until [ -S /run/redis/redis.sock ] && [ -S /run/php-fpm.sock ] && pgrep nginx >/dev/null; do sleep 1; done
run() { local name="$1"; shift; echo "== $name"; "$@" > "/results/$name.txt" 2>&1; echo "   exit $?"; }
run compat bash /bench/compat.sh
run crash bash /bench/crash.sh
run http-core bash /bench/rounds.sh core 3 30
run http-full bash /bench/rounds.sh full 3 30
backends="rust redis"
[ "$(php -r 'echo extension_loaded("memcache") ? 1 : 0;')" = 1 ] && backends="$backends memcached"
# shellcheck disable=SC2086
run ops bash /bench/bench-ops.sh 10 1,2,4,8,16,32 $backends
run engine-wordpress wprc-bench engine --workload wordpress --seconds 3
run engine-get wprc-bench engine --workload get --workers 1,4,8,16,32 --seconds 2
run eviction-woocommerce wprc-bench eviction --workload woocommerce --memory 16MB,32MB,64MB,128MB,256MB --requests 150000
run eviction-wordpress wprc-bench eviction --workload wordpress --memory 16MB,32MB,64MB,128MB --requests 150000
run eviction-multisite wprc-bench eviction --workload multisite --memory 32MB,64MB,128MB --requests 150000
echo "== done"
