#!/bin/bash
# Starts every service of the benchmark server, installs WordPress on first
# run, then runs the container command.
set -euo pipefail

mkdir -p /run/redis /run/mysqld
chown mysql:mysql /run/mysqld /var/lib/mysql
# An empty volume mounted on /var/lib/mysql has no system tables yet.
if [ ! -d /var/lib/mysql/mysql ]; then
  mariadb-install-db --user=mysql --datadir=/var/lib/mysql >/dev/null
fi
service mariadb start >/dev/null

# Redis and Memcached get the same memory budget as wp-rust-cache and their
# fastest local transport: a Unix socket for Redis, localhost for Memcached
# (the Memcached drop-in only speaks host:port).
redis-server --daemonize yes --save '' --appendonly no \
  --unixsocket /run/redis/redis.sock --unixsocketperm 777 \
  --maxmemory 512mb --maxmemory-policy allkeys-lru >/dev/null
memcached -d -u memcache -m 512 -l 127.0.0.1 -p 11211 -t 4

php-fpm -D
nginx

if [ ! -f /var/www/html/wp-config.php ]; then
  bash /bench/setup.sh
fi

exec "$@"
