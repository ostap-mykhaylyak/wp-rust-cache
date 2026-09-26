#!/bin/bash
# Switches the object cache: none | rust | redis | memcached.
# Empties every backend, restarts PHP-FPM (fresh workers, fresh opcache).
set -euo pipefail
backend="$1"
CONTENT=/var/www/html/wp-content
rm -f "$CONTENT/object-cache.php"
case "$backend" in
  none) ;;
  rust) cp /bench/wp-rust-cache-object-cache.php "$CONTENT/object-cache.php" ;;
  redis) cp "$CONTENT/plugins/redis-cache/includes/object-cache.php" "$CONTENT/object-cache.php" ;;
  memcached) cp "$CONTENT/plugins/memcached/object-cache.php" "$CONTENT/object-cache.php" ;;
  *) echo "unknown backend $backend" >&2; exit 2 ;;
esac
[ -f "$CONTENT/object-cache.php" ] && chown www-data:www-data "$CONTENT/object-cache.php"

redis-cli -s /run/redis/redis.sock flushall >/dev/null
php -r '$m = new Memcached(); $m->addServer("127.0.0.1", 11211); $m->flush();'
wp-rust-cache flush >/dev/null 2>&1 || true

restart_fpm() {
  local master
  master=$(pgrep -f 'php-fpm: master' | head -1)
  [ -n "$master" ] && kill -TERM "$master" 2>/dev/null
  for _ in $(seq 100); do pgrep -f 'php-fpm: (master|pool)' >/dev/null || break; sleep 0.1; done
  rm -f /run/php-fpm.sock
  php-fpm -D >/dev/null 2>&1
}
restart_fpm
