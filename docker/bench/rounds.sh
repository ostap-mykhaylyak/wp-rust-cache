#!/bin/bash
# Repeats bench-http.sh with the backend order rotated each round, so that
# drift (thermal, background activity) does not favour one backend.
#   rounds.sh SCENARIO ROUNDS [seconds]
set -uo pipefail
SCENARIO="$1"
ROUNDS="${2:-3}"
SECS="${3:-30}"
orders=("none rust redis memcached" "memcached redis rust none" "redis none memcached rust" "rust memcached none redis")
for r in $(seq 1 "$ROUNDS"); do
  echo "## round $r"
  # shellcheck disable=SC2086
  bash /bench/bench-http.sh "$SCENARIO" "$SECS" 32 ${orders[$(( (r - 1) % 4 ))]} | tail -n +4
done
