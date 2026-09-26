#!/bin/bash
# Installs WordPress + WooCommerce + Elementor with enough content for the
# cache to matter: 3000 posts, 1000 products, 400 terms, 500 users.
set -euo pipefail
WP="wp --allow-root --path=/var/www/html"

mysql -e "CREATE DATABASE IF NOT EXISTS wp; CREATE USER IF NOT EXISTS 'wp'@'localhost' IDENTIFIED BY 'wp'; GRANT ALL ON wp.* TO 'wp'@'localhost';"

$WP core download --version="${WP_VERSION:-latest}" --quiet
$WP config create --dbname=wp --dbuser=wp --dbpass=wp --dbhost=localhost:/run/mysqld/mysqld.sock --quiet
# Redis Object Cache: fastest local transport.
$WP config set WP_REDIS_SCHEME unix
$WP config set WP_REDIS_PATH /run/redis/redis.sock
$WP core install --url=http://localhost --title=Bench --admin_user=admin \
  --admin_password="$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')" \
  --admin_email=bench@example.com --skip-email --quiet
$WP rewrite structure '/%postname%/' --quiet

$WP plugin install woocommerce elementor --activate --quiet
# Drop-in sources for the comparison (never activated as plugins).
$WP plugin install redis-cache memcached --quiet

$WP term generate category --count=100 --quiet
$WP term generate post_tag --count=300 --quiet
$WP user generate --count=500 --quiet
$WP post generate --count=3000 --post_type=post --quiet
$WP eval-file /bench/seed.php

chown -R www-data:www-data /var/www/html
echo "WordPress ready: $($WP core version), WooCommerce $($WP plugin get woocommerce --field=version), Elementor $($WP plugin get elementor --field=version)"
