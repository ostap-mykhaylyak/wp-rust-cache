#!/bin/bash
# Thread safety under FrankenPHP: many PHP threads in one process share one
# mapping, each with its own thread-local state.
set -uo pipefail
fail=0
ok() { echo "  ok    $*"; }
ko() { echo "  FAIL  $*"; fail=$((fail + 1)); }
hit() { curl -s "http://127.0.0.1:$1/$2"; }
burst() { # port path count parallel → number of responses that were not OK
  # Parallel curls interleave their output, so count the OKs, not lines.
  local oks
  oks=$(seq "$3" | xargs -P "$4" -I{} curl -s "http://127.0.0.1:$1/$2" | grep -o 'OK' | wc -l)
  echo $(( $3 - oks ))
}

frankenphp run --config /etc/frankenphp/Caddyfile >/tmp/frankenphp.log 2>&1 &
for _ in $(seq 100); do curl -s -o /dev/null http://127.0.0.1:8080/ops.php?op=get && break; sleep 0.2; done
echo "== $(php -r 'echo PHP_VERSION, PHP_ZTS ? " ZTS" : " NTS";'), FrankenPHP $(frankenphp version | head -1)"

echo "-- classic mode, 32 threads"
[ "$(hit 8080 'ops.php?op=reset')" = OK ] && ok "reset" || ko "reset"
bad=$(burst 8080 'ops.php?op=incr' 5000 64)
v=$(hit 8080 'ops.php?op=get')
[ "$bad" = 0 ] && [ "$v" = 5000 ] && ok "5000 concurrent incr → $v" || ko "incr: $bad failures, counter $v"
bad=$(burst 8080 'ops.php?op=rw' 5000 64)
[ "$bad" = 0 ] && ok "5000 concurrent array writes/reads, all exact" || ko "rw: $bad mismatches"

echo "-- worker mode, 8 workers"
[ "$(hit 8081 'worker.php?op=reset')" = OK ] && ok "reset" || ko "reset"
bad=$(burst 8081 'worker.php?op=incr' 3000 32)
v=$(hit 8081 'worker.php?op=get')
[ "$bad" = 0 ] && [ "$v" = 3000 ] && ok "3000 concurrent incr → $v" || ko "incr: $bad failures, counter $v"

echo "-- worker mode: segment deleted while workers hold it"
rm -f /dev/shm/wp-rust-cache-test
# The next requests must notice and attach to a new segment: the counter
# starts over there, and a separate process sees the same new segment.
for _ in $(seq 20); do hit 8081 'worker.php?op=get' >/dev/null; done
[ "$(hit 8081 'worker.php?op=reset')" = OK ] && ok "workers write to a new segment" || ko "reset after delete"
bad=$(burst 8081 'worker.php?op=incr' 1000 32)
seen=$(php -r '$g = wp_rust_cache_group("frankenphp", "worker"); echo wp_rust_cache_get($g, 0, "counter");')
[ "$bad" = 0 ] && [ "$seen" = 1000 ] && ok "all 8 workers switched: a CLI process reads 1000" || ko "after delete: $bad failures, CLI reads '$seen'"
[ -e /dev/shm/wp-rust-cache-test ] && ok "new segment file exists" || ko "no new segment"

if grep -qiE 'panic|segfault|fatal' /tmp/frankenphp.log; then
  ko "FrankenPHP log:"; grep -iE 'panic|segfault|fatal' /tmp/frankenphp.log | head -5
else
  ok "no panic, segfault or fatal error in the FrankenPHP log"
fi
echo; [ $fail = 0 ] && echo "frankenphp: all checks passed" || echo "frankenphp: $fail check(s) failed"
exit $fail
