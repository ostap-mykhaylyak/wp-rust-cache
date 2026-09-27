#!/bin/sh
# A PHP process killed while it holds a shard lock: the next process must
# reset that shard, count it as "owner died", and say so in the PHP log.
#   recovery_test.sh "php -d extension=... -d wp_rust_cache.config=..."
set -u
PHP="$1"
CLI="${WPRC_CLI:-}"
log=$(mktemp)

# The segment may already count resets from an earlier run: compare with
# the count now.
base=$($PHP -r '$s = wp_rust_cache_stats(); echo $s ? $s["recoveries"] : 0;')

# Writing 256 KB values to one key keeps the writer inside that key's shard
# lock much of the time, so a kill -9 soon lands there. Retry until it does.
for attempt in $(seq 1 40); do
	$PHP -r '
		$g = wp_rust_cache_group("recovery-test", "g");
		$v = str_repeat("x", 262144);
		for (;;) { wp_rust_cache_set($g, 0, "hot", $v); }
	' &
	pid=$!
	sleep 0.3
	kill -9 "$pid" 2>/dev/null
	wait "$pid" 2>/dev/null
	# Next process: its first operation on that shard performs the reset;
	# its next request start (here: wp_rust_cache_available) logs it.
	$PHP -d error_log="$log" -r '
		$g = wp_rust_cache_group("recovery-test", "g");
		wp_rust_cache_get($g, 0, "hot");
		wp_rust_cache_available();
		$s = wp_rust_cache_stats();
		echo $s["recoveries"], "\n";
	' > /tmp/recoveries 2>/dev/null
	if [ "$(cat /tmp/recoveries)" -gt "$base" ] 2>/dev/null; then
		break
	fi
done

fail=0
if [ "$(cat /tmp/recoveries)" -gt "$base" ] 2>/dev/null; then
	echo "  ok    lock owner killed on attempt $attempt, shard reset"
else
	echo "  FAIL  no kill landed inside a lock in 40 attempts"
	fail=1
fi
if grep -q 'wp-rust-cache: reset 1 cache shard(s) because a process died holding the shard lock' "$log"; then
	echo "  ok    the PHP log says why"
else
	echo "  FAIL  PHP log: $(cat "$log")"
	fail=1
fi
if [ -n "$CLI" ]; then
	out=$($CLI stats 2>&1)
	echo "$out" | grep -qE 'recoveries_owner_died +[1-9]' && echo "  ok    stats: recoveries_owner_died" || { echo "  FAIL  stats"; fail=1; }
	echo "$out" | grep -q 'Last shard reset: .* a process died holding the shard lock' && echo "  ok    stats: last shard reset and its cause" || { echo "  FAIL  last reset line"; fail=1; }
fi
rm -f "$log"
exit $fail
