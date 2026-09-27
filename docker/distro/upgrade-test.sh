#!/bin/bash
# Upgrade path: OLD.deb installed and serving, then `apt install NEW.deb`
# and an FPM reload. The cache must survive (same segment layout), the new
# module must be the one loaded, and the site must keep working.
#   upgrade-test.sh OLD.deb NEW.deb
set -uo pipefail
OLD="$1"
NEW="$2"
fail=0
ok() { echo "  ok    $*"; }
ko() { echo "  FAIL  $*"; fail=$((fail + 1)); }
# systemd may stay "starting" if some unit waits at boot (seen on CI runners):
# the script starts the services it needs itself, so wait at most 90 s.
timeout 90 systemctl is-system-running --wait >/dev/null 2>&1 || true
V=$(php -r 'echo PHP_MAJOR_VERSION, ".", PHP_MINOR_VERSION;')
FPM="php$V-fpm"
WP="wp --allow-root --path=/var/www/wp"
echo "== $(. /etc/os-release; echo "$PRETTY_NAME"), PHP $(php -r 'echo PHP_VERSION;')"

systemctl start mariadb "$FPM" nginx
# The database must answer before the site can be set up (we no longer wait
# for the whole boot).
for _ in $(seq 60); do mysqladmin ping >/dev/null 2>&1 && break; sleep 1; done
mysql -e "CREATE DATABASE IF NOT EXISTS wp; CREATE USER IF NOT EXISTS 'wp'@'localhost' IDENTIFIED BY 'wp'; GRANT ALL ON wp.* TO 'wp'@'localhost';"
mkdir -p /var/www/wp
$WP core download --quiet
$WP config create --dbname=wp --dbuser=wp --dbpass=wp --dbhost=localhost --quiet
$WP core install --url=http://localhost --title=Up --admin_user=admin --admin_password="$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')" --admin_email=a@example.com --skip-email --quiet
$WP post generate --count=200 --quiet
chown -R www-data:www-data /var/www/wp
cat > /etc/nginx/sites-enabled/default <<EOF
server { listen 80 default_server; root /var/www/wp; index index.php;
  location / { try_files \$uri \$uri/ /index.php?\$args; }
  location ~ \.php\$ { include snippets/fastcgi-php.conf; fastcgi_pass unix:/run/php/$FPM.sock; } }
EOF
systemctl reload nginx

echo "-- old version serving"
apt-get install -y -qq "$OLD" >/dev/null 2>&1 && wp-rust-cache install --wp /var/www/wp --user www-data >/dev/null 2>&1 && systemctl reload "$FPM"
for p in $($WP post list --posts_per_page=100 --field=url 2>/dev/null); do curl -s -o /dev/null "$p"; done
entries() { wp-rust-cache stats --json | php -r 'echo json_decode(stream_get_contents(STDIN), true)["entries"];'; }
modver() { echo '<?php echo phpversion("wp_rust_cache");' > /var/www/wp/v.php; curl -s http://127.0.0.1/v.php; }
before=$(entries); old_v=$(modver)
ok "$(wp-rust-cache version), module $old_v serving, $before entries"

echo "-- apt install of the new package + reload"
apt-get install -y -qq "$NEW" >/tmp/up.log 2>&1 && ok "installed $(dpkg-query -W -f='${Version}' wp-rust-cache)" || { ko "upgrade"; cat /tmp/up.log; }
grep -q "reload PHP-FPM" /tmp/up.log && ok "postinst asks for the reload" || ko "no reload hint"
systemctl reload "$FPM"; sleep 1
new_v=$(modver)
[ "$new_v" = "$(dpkg-query -W -f='${Version}' wp-rust-cache)" ] && ok "PHP-FPM now runs module $new_v" || ko "module still $new_v"
after=$(entries)
[ "$after" -ge "$before" ] && ok "cache kept across the upgrade ($before → $after entries)" || ko "cache lost ($before → $after)"
bad=0
for p in $($WP post list --posts_per_page=50 --field=url 2>/dev/null); do [ "$(curl -s -o /dev/null -w '%{http_code}' "$p")" = 200 ] || bad=$((bad + 1)); done
[ $bad = 0 ] && ok "site answers after the upgrade" || ko "$bad pages failed"
out=$(wp-rust-cache stats --groups 2>&1); grep -q "groups" <<< "$out" && ok "new CLI: stats --groups" || ko "stats --groups"
out=$(wp-rust-cache stats --keys options 2>&1); grep -q "alloptions" <<< "$out" && ok "new CLI: stats --keys options lists alloptions" || ko "stats --keys: $out"
wp-rust-cache verify | grep -q consistent && ok "structures consistent" || ko "verify"
rm -f /var/www/wp/v.php

echo; [ $fail = 0 ] && echo "upgrade: all checks passed" || echo "upgrade: $fail check(s) failed"
exit $fail
