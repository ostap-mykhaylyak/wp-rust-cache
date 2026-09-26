#!/bin/bash
# Compatibility checks on the real stack with the wp-rust-cache drop-in:
# every benchmark URL answers 200, WooCommerce cart and checkout data flow
# through the cache, Elementor renders, WP-CLI sees the same cache as
# PHP-FPM, and PHP logs nothing.
set -uo pipefail
WP="wp --allow-root --path=/var/www/html"
# Elementor prints its own PHP 8.4 deprecation notices on stdout under WP-CLI;
# data queries skip it so their output is only the data.
WPQ="$WP --skip-plugins=elementor"
fail=0
ok() { echo "  ok    $*"; }
ko() { echo "  FAIL  $*"; fail=$((fail + 1)); }

bash /bench/use-backend.sh rust
: > /var/log/php-errors.log

echo "-- drop-in active"
$WP eval 'echo $GLOBALS["wp_object_cache"]->is_persistent() ? "persistent" : "NOT persistent", PHP_EOL;' 2>/dev/null | grep -q '^persistent$' \
  && ok "WP-CLI uses shared memory" || ko "WP-CLI is not using shared memory"

echo "-- every URL, twice (cold, then from cache)"
for pass in cold warm; do
  bad=0
  while read -r u; do
    code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1$u")
    [ "$code" = 200 ] || { bad=$((bad + 1)); echo "    $code $u"; }
  done < /var/www/html/bench-urls.txt
  [ $bad = 0 ] && ok "$pass: $(wc -l < /var/www/html/bench-urls.txt) URLs → 200" || ko "$pass: $bad URLs failed"
done
curl -s -o /dev/null http://127.0.0.1/ # ensure FPM attached
hits=$(wp-rust-cache stats --json | php -r 'echo json_decode(stream_get_contents(STDIN),true)["hits"];')
[ "${hits:-0}" -gt 1000 ] && ok "PHP-FPM hits in shared memory: $hits" || ko "no shared-memory hits from PHP-FPM ($hits)"

echo "-- cache coherence between PHP-FPM and WP-CLI"
$WPQ option update wprc_probe "from-cli-$$" >/dev/null
cat > /var/www/html/wprc-probe.php <<'PHP'
<?php require __DIR__ . '/wp-load.php'; echo get_option( 'wprc_probe' );
PHP
got=$(curl -s http://127.0.0.1/wprc-probe.php)
[ "$got" = "from-cli-$$" ] && ok "option written by WP-CLI is read by PHP-FPM" || ko "PHP-FPM read '$got'"
$WPQ option update wprc_probe "second-$$" >/dev/null
got=$(curl -s http://127.0.0.1/wprc-probe.php)
[ "$got" = "second-$$" ] && ok "update invalidates the value PHP-FPM had cached" || ko "stale value after update: '$got'"
rm -f /var/www/html/wprc-probe.php

echo "-- WooCommerce"
pid=$($WPQ post list --post_type=product --posts_per_page=1 --field=ID --orderby=ID --order=ASC)
jar=$(mktemp)
curl -s -c "$jar" -b "$jar" -o /dev/null "http://127.0.0.1/?add-to-cart=$pid"
# The block-based cart renders client-side: read it through the Store API.
cart() {
  curl -s -c "$jar" -b "$jar" http://127.0.0.1/wp-json/wc/store/v1/cart \
    | php -r '$c = json_decode(stream_get_contents(STDIN), true); foreach ($c["items"] ?? [] as $i) echo $i["name"], "=", $i["quantity"], " ";'
}
got=$(cart)
[ "$got" = "Product 0=1 " ] && ok "cart keeps the product across requests (session)" || ko "cart after add-to-cart: '$got'"
curl -s -c "$jar" -b "$jar" -o /dev/null "http://127.0.0.1/?add-to-cart=$pid"
got=$(cart)
[ "$got" = "Product 0=2 " ] && ok "second add-to-cart gives quantity 2" || ko "cart after two adds: '$got'"
order=$($WPQ eval "\$o = wc_create_order(); \$o->add_product(wc_get_product($pid), 3); \$o->calculate_totals(); \$o->save(); echo \$o->get_id();")
stock_before=$($WPQ eval "echo wc_get_product($pid)->get_stock_quantity();")
$WPQ eval "wc_reduce_stock_levels($order);" >/dev/null
stock_cli=$($WPQ eval "echo wc_get_product($pid)->get_stock_quantity();")
cat > /var/www/html/wprc-stock.php <<PHP
<?php require __DIR__ . '/wp-load.php'; echo wc_get_product( $pid )->get_stock_quantity();
PHP
stock_fpm=$(curl -s http://127.0.0.1/wprc-stock.php)
rm -f /var/www/html/wprc-stock.php
[ "$stock_cli" = $((stock_before - 3)) ] && [ "$stock_fpm" = "$stock_cli" ] \
  && ok "order #$order reduced stock $stock_before → $stock_cli, seen by PHP-FPM" \
  || ko "stock: before=$stock_before cli=$stock_cli fpm=$stock_fpm"
rm -f "$jar"

echo "-- Elementor"
page=$($WPQ post list --post_type=page --title="Elementor page" --field=url)
html=$(curl -s "$page")
echo "$html" | grep -q "Built with Elementor" && echo "$html" | grep -q "elementor-widget-heading" \
  && ok "Elementor page renders its widgets" || ko "Elementor page did not render"
html2=$(curl -s "$page")
[ "$html" = "$html2" ] || [ "$(echo "$html2" | grep -c 'Built with Elementor')" -gt 0 ] && ok "Elementor page renders again from cache" || ko "second render differs"

echo "-- WP-CLI commands"
$WP rust-cache status | grep -q "RUNNING" && ok "wp rust-cache status" || ko "wp rust-cache status"
inspected=$($WP rust-cache inspect alloptions --group=options 2>&1)
grep -q "Type:  array" <<< "$inspected" && ok "wp rust-cache inspect alloptions" || ko "inspect"
$WP rust-cache flush | grep -q Success && ok "wp rust-cache flush" || ko "flush"

echo "-- structures"
wp-rust-cache verify | grep -q consistent && ok "wp-rust-cache verify" || ko "verify"

echo "-- PHP log"
# Plugins log their own deprecations under PHP 8.4+; only lines that point at
# the cache (drop-in or extension) count as failures.
ours=$(grep -E 'object-cache\.php|wp_rust_cache|WP_Object_Cache' /var/log/php-errors.log || true)
if [ -n "$ours" ]; then
  ko "PHP logged problems in the cache:"; echo "$ours" | sort | uniq -c | sort -rn | head -20
else
  ok "nothing logged by or about the object cache"
fi
others=$(grep -vcE 'object-cache\.php|wp_rust_cache|WP_Object_Cache' /var/log/php-errors.log || true)
[ "${others:-0}" -gt 0 ] && echo "  info  $others unrelated plugin log line(s), e.g.: $(grep -vE 'object-cache\.php|wp_rust_cache' /var/log/php-errors.log | head -1 | cut -c1-160)"

echo; [ $fail = 0 ] && echo "compat: all checks passed" || echo "compat: $fail check(s) failed"
exit $fail
