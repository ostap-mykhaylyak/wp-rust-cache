#!/bin/bash
# Repeats bench-http.sh with the backend order rotated each round, so that
# drift (thermal, background activity) does not favour one backend.
#   rounds.sh SCENARIO ROUNDS [seconds]
set -uo pipefail
SCENARIO="$1"
ROUNDS="${2:-3}"
SECS="${3:-30}"
orders=("none rust redis memcached" "memcached redis rust none" "redis none memcached rust" "rust memcached none redis")
# The wordpress.org Memcached drop-in needs PECL memcache, which does not
# build on every PHP version (not on 8.5): leave that backend out there.
has_memcache=$(php -r 'echo extension_loaded("memcache") ? 1 : 0;')
for r in $(seq 1 "$ROUNDS"); do
  echo "## round $r"
  order=${orders[$(( (r - 1) % 4 ))]}
  [ "$has_memcache" = 1 ] || order=$(echo "$order" | sed 's/memcached//')
  # shellcheck disable=SC2086
  bash /bench/bench-http.sh "$SCENARIO" "$SECS" 32 $order | tail -n +4
done
