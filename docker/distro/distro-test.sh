#!/bin/bash
# The administrator's path on a real distribution with systemd:
# WordPress on the distribution's nginx + PHP-FPM + MariaDB, then
# apt install of the package, `wp-rust-cache install`, systemctl reload,
# checks, upgrade, `uninstall`, apt remove and purge.
#   distro-test.sh /path/to/wp-rust-cache_X_amd64.deb
set -uo pipefail
DEB="$1"
fail=0
ok() { echo "  ok    $*"; }
ko() { echo "  FAIL  $*"; fail=$((fail + 1)); }
step() { echo "-- $*"; }

# systemd may stay "starting" if some unit waits at boot (seen on CI runners):
# the script starts the services it needs itself, so wait at most 90 s.
timeout 90 systemctl is-system-running --wait >/dev/null 2>&1 || true
V=$(php -r 'echo PHP_MAJOR_VERSION, ".", PHP_MINOR_VERSION;')
FPM="php$V-fpm"
WP="wp --allow-root --path=/var/www/wp"
echo "== $(. /etc/os-release; echo "$PRETTY_NAME"), PHP $(php -r 'echo PHP_VERSION;'), $(systemctl --version | head -1)"

step "WordPress on the distribution's stack"
systemctl start mariadb "$FPM" nginx
# The database must answer before the site can be set up (we no longer wait
# for the whole boot).
for _ in $(seq 60); do mysqladmin ping >/dev/null 2>&1 && break; sleep 1; done
mysql -e "CREATE DATABASE IF NOT EXISTS wp; CREATE USER IF NOT EXISTS 'wp'@'localhost' IDENTIFIED BY 'wp'; GRANT ALL ON wp.* TO 'wp'@'localhost';"
mkdir -p /var/www/wp
$WP core download --quiet
$WP config create --dbname=wp --dbuser=wp --dbpass=wp --dbhost=localhost --quiet
$WP core install --url=http://localhost --title=Distro --admin_user=admin \
  --admin_password="$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')" --admin_email=a@example.com --skip-email --quiet
$WP post generate --count=300 --quiet
chown -R www-data:www-data /var/www/wp
cat > /etc/nginx/sites-enabled/default <<EOF
server {
    listen 80 default_server;
    root /var/www/wp;
    index index.php;
    location / { try_files \$uri \$uri/ /index.php?\$args; }
    location ~ \.php\$ {
        include snippets/fastcgi-php.conf;
        fastcgi_pass unix:/run/php/$FPM.sock;
    }
}
EOF
systemctl reload nginx
[ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1/)" = 200 ] && ok "site answers without cache" || ko "site down before install"

step "apt install"
apt-get install -y -qq "$DEB" >/tmp/apt.log 2>&1 && ok "installed $(dpkg-query -W -f='${Package} ${Version}' wp-rust-cache)" || { ko "apt install"; cat /tmp/apt.log; }
grep -q "not enabled" /tmp/apt.log && ok "postinst explains the next step" || ko "postinst message missing"
[ -f "/usr/lib/wp-rust-cache/php-$V/wp_rust_cache.so" ] && ok "module for PHP $V shipped" || ko "no module for PHP $V"
php -m | grep -q wp_rust_cache && ko "extension enabled before install" || ok "nothing enabled by the package itself"

step "wp-rust-cache install"
wp-rust-cache install --wp /var/www/wp --user www-data --dry-run >/tmp/dry.log 2>&1 && ok "dry run" || { ko "dry run"; cat /tmp/dry.log; }
[ ! -f /var/www/wp/wp-content/object-cache.php ] && ok "dry run changed nothing" || ko "dry run wrote the drop-in"
wp-rust-cache install --wp /var/www/wp --user www-data >/tmp/install.log 2>&1 && ok "install" || { ko "install"; cat /tmp/install.log; }
grep -q "extension=/usr/lib/wp-rust-cache/php-$V/wp_rust_cache.so" "/etc/php/$V/mods-available/wp_rust_cache.ini" \
  && ok "ini points at the packaged module" || ko "ini: $(cat /etc/php/$V/mods-available/wp_rust_cache.ini)"
[ -L "/etc/php/$V/fpm/conf.d/20-wp_rust_cache.ini" ] && ok "enabled for FPM via phpenmod" || ko "phpenmod did not link the FPM conf.d"
systemctl reload "$FPM" && sleep 1 && ok "systemctl reload $FPM"

step "the site on the cache"
bad=0
for i in $(seq 1 60); do
  u=$($WP post list --posts_per_page=1 --offset=$((i * 4)) --field=url 2>/dev/null)
  [ "$(curl -s -o /dev/null -w '%{http_code}' "$u")" = 200 ] || bad=$((bad + 1))
done
[ $bad = 0 ] && ok "60 posts answer 200" || ko "$bad posts failed"
for i in 1 2 3; do curl -s -o /dev/null http://127.0.0.1/; done
hits=$(wp-rust-cache stats --json | php -r 'echo json_decode(stream_get_contents(STDIN), true)["hits"];')
[ "${hits:-0}" -gt 100 ] && ok "PHP-FPM hits in shared memory: $hits" || ko "no hits ($hits)"
owner=$(stat -c '%U %a' /dev/shm/wp-rust-cache)
[ "$owner" = "www-data 600" ] && ok "segment owned by www-data, mode 600" || ko "segment: $owner"
echo '<?php require __DIR__ . "/wp-load.php"; echo get_option("probe");' > /var/www/wp/probe.php
$WP option update probe "v1-$$" >/dev/null 2>&1; curl -s -o /dev/null http://127.0.0.1/probe.php
$WP option update probe "v2-$$" >/dev/null 2>&1
[ "$(curl -s http://127.0.0.1/probe.php)" = "v2-$$" ] && ok "WP-CLI (root) and PHP-FPM share the segment" || ko "stale option in PHP-FPM"
health=$($WP eval 'echo wp_rust_cache_site_health_test()["status"];' 2>/dev/null)
[ "$health" = good ] && ok "Site Health: good" || ko "Site Health: $health"
$WP rust-cache status 2>/dev/null | grep -q RUNNING && ok "wp rust-cache status" || ko "wp rust-cache status"
wp-rust-cache stats --prometheus | grep -q '^wp_rust_cache_hits_total ' && ok "Prometheus output" || ko "Prometheus output"
wp-rust-cache verify | grep -q consistent && ok "structures consistent" || ko "verify"
journal=$(journalctl -u "$FPM" --no-pager 2>/dev/null | grep -ciE 'wp.rust|segfault|SIGBUS|SIGSEGV' || true)
[ "${journal:-0}" = 0 ] && ok "nothing about the cache in the $FPM journal" || ko "journal: $(journalctl -u "$FPM" --no-pager | grep -iE 'wp.rust|segfault|SIG' | tail -3)"

step "PHP-FPM restart keeps the cache"
before=$(wp-rust-cache stats --json | php -r 'echo json_decode(stream_get_contents(STDIN), true)["entries"];')
systemctl restart "$FPM"; curl -s -o /dev/null http://127.0.0.1/
after=$(wp-rust-cache stats --json | php -r 'echo json_decode(stream_get_contents(STDIN), true)["entries"];')
[ "$after" -ge "$before" ] && ok "entries survive a restart ($before → $after)" || ko "entries lost ($before → $after)"

step "package upgrade (reinstall of the same version)"
apt-get install -y -qq --reinstall "$DEB" >/tmp/up.log 2>&1 && ok "reinstall" || ko "reinstall"
grep -q "reload PHP-FPM" /tmp/up.log && ok "upgrade asks for an FPM reload" || ko "no reload hint on upgrade"
systemctl reload "$FPM"; [ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1/)" = 200 ] && ok "site fine after reload" || ko "site broken after upgrade"

step "wp-rust-cache uninstall"
wp-rust-cache uninstall --wp /var/www/wp >/tmp/un.log 2>&1 && ok "uninstall" || { ko "uninstall"; cat /tmp/un.log; }
systemctl reload "$FPM"; sleep 1
[ ! -e /var/www/wp/wp-content/object-cache.php ] && ok "drop-in removed" || ko "drop-in still there"
php -m | grep -q wp_rust_cache && ko "extension still enabled" || ok "extension disabled"
[ ! -e /dev/shm/wp-rust-cache ] && ok "segment released" || ko "segment still there"
[ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1/)" = 200 ] && ok "site answers without the cache" || ko "site broken after uninstall"

step "remove without uninstall is safe too"
wp-rust-cache install --wp /var/www/wp --user www-data >/dev/null 2>&1 && systemctl reload "$FPM"
apt-get remove -y -qq wp-rust-cache >/dev/null 2>&1 && ok "apt remove"
systemctl reload "$FPM"; sleep 1
php -m 2>&1 | grep -qi "wp_rust_cache" && ko "PHP still references the module" || ok "prerm disabled the extension"
[ "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1/)" = 200 ] && ok "site answers with the orphan drop-in" || ko "orphan drop-in breaks the site"
health=$($WP eval 'echo wp_rust_cache_site_health_test()["status"];' 2>/dev/null)
[ "$health" = critical ] && ok "Site Health reports the missing extension" || ko "Site Health: $health"
apt-get purge -y -qq wp-rust-cache >/dev/null 2>&1
[ ! -e /etc/wp-rust-cache ] && ok "purge removed /etc/wp-rust-cache" || ko "config left after purge"

echo; [ $fail = 0 ] && echo "distro: all checks passed" || echo "distro: $fail check(s) failed"
exit $fail
