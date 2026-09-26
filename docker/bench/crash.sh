#!/bin/bash
# Crash safety on the real stack: kill -9 PHP-FPM workers under load (one at
# a time and several at once), restart PHP-FPM, delete the segment while
# workers use it (what a reboot does to /dev/shm), retire it, change the
# configuration. After each step: the site answers, the structures verify,
# and a value written from WP-CLI is what PHP-FPM reads.
set -uo pipefail
WPQ="wp --allow-root --path=/var/www/html --skip-plugins=elementor"
fail=0
ok() { echo "  ok    $*"; }
ko() { echo "  FAIL  $*"; fail=$((fail + 1)); }

cat > /var/www/html/wprc-probe.php <<'PHP'
<?php require __DIR__ . '/wp-load.php'; echo get_option( 'wprc_probe' );
PHP

check() {
  local what="$1"
  local bad=0
  for u in $(shuf -n 40 /var/www/html/bench-urls.txt); do
    [ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1$u")" = 200 ] || bad=$((bad + 1))
  done
  local v="probe-$RANDOM-$RANDOM"
  $WPQ option update wprc_probe "$v" >/dev/null 2>&1
  local seen=""
  for _ in 1 2 3 4 5 6 7 8; do seen=$(curl -s http://127.0.0.1/wprc-probe.php); done
  local verify
  verify=$(wp-rust-cache verify 2>&1)
  if [ $bad = 0 ] && [ "$seen" = "$v" ] && echo "$verify" | grep -q consistent; then
    ok "$what: pages 200, CLI→FPM coherent, structures consistent"
  else
    ko "$what: failing pages=$bad, FPM read '$seen' (want $v), verify: $verify"
  fi
}

load() { wrk -t4 -c32 -d"$1"s -s /bench/urls.lua http://127.0.0.1:80/ >/dev/null 2>&1 & }
workers() { pgrep -f 'php-fpm: pool' | shuf; }
restart_fpm() {
  local master
  master=$(pgrep -f 'php-fpm: master' | head -1)
  [ -n "$master" ] && kill -TERM "$master" 2>/dev/null
  for _ in $(seq 100); do pgrep -f 'php-fpm: (master|pool)' >/dev/null || break; sleep 0.1; done
  rm -f /run/php-fpm.sock
  php-fpm -D >/dev/null 2>&1
}

bash /bench/use-backend.sh rust >/dev/null 2>&1
check "baseline"

echo "-- kill -9 one worker at a time, 20 times, under load"
load 25
for _ in $(seq 20); do kill -9 "$(workers | head -1)" 2>/dev/null; sleep 1; done
wait
check "after 20 single kills"

echo "-- kill -9 eight workers at once, 5 times, under load"
load 20
for _ in $(seq 5); do kill -9 $(workers | head -8) 2>/dev/null; sleep 3; done
wait
check "after 5 × 8 simultaneous kills"

echo "-- kill -9 every worker and the master, then start PHP-FPM"
load 10
sleep 3
pkill -9 php-fpm
sleep 1
rm -f /run/php-fpm.sock
php-fpm -D >/dev/null 2>&1
wait
check "after killing all of PHP-FPM"

echo "-- segment deleted while workers use it (reboot empties /dev/shm)"
curl -s -o /dev/null http://127.0.0.1/
$WPQ option update wprc_probe "before-delete" >/dev/null 2>&1
rm -f /dev/shm/wp-rust-cache
load 8
sleep 8
wait
check "after deleting the segment"

echo "-- recreate under load"
load 8
sleep 2
wp-rust-cache recreate >/dev/null
wait
check "after wp-rust-cache recreate"

echo "-- configuration change (shards 64 → 32) and PHP-FPM reload"
sed -i 's/^shards = 64/shards = 32/' /etc/wp-rust-cache/config.toml
restart_fpm
curl -s -o /dev/null http://127.0.0.1/
shards=$(wp-rust-cache stats --json | php -r 'echo json_decode(stream_get_contents(STDIN), true)["shards"];')
[ "$shards" = 32 ] && ok "new segment has 32 shards" || ko "segment has $shards shards"
check "after configuration change"
sed -i 's/^shards = 32/shards = 64/' /etc/wp-rust-cache/config.toml

echo
wp-rust-cache stats --json | php -r '$s = json_decode(stream_get_contents(STDIN), true); printf("segment counters: recoveries=%d shard_resets=%d attaches=%d\n", $s["recoveries"], $s["shard_resets"], $s["attaches"]);'
ls -la /dev/shm/
rm -f /var/www/html/wprc-probe.php
[ $fail = 0 ] && echo "crash: all checks passed" || echo "crash: $fail check(s) failed"
exit $fail
