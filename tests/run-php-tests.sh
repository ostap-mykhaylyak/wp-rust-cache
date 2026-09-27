#!/bin/sh
# Builds the extension for the PHP in PATH and runs the PHP test suites.
# Needs a /dev/shm of at least 64 MB (docker run --shm-size=256m).
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WP_VERSION="${WP_VERSION:-6.8}"

SO=$(sh "$ROOT/php-extension/build.sh" 2>/tmp/wprc-build.log) || { tail -40 /tmp/wprc-build.log; exit 1; }
PHP="php -d extension=$SO -d wp_rust_cache.config=$ROOT/tests/php/test.toml"
echo "== $(php -r 'echo PHP_VERSION, PHP_ZTS ? " ZTS" : " NTS";') =="

echo "-- extension"
$PHP "$ROOT/tests/php/extension_test.php"

echo "-- worker killed holding a lock"
cargo build -q --release -p wp-rust-cache --manifest-path "$ROOT/Cargo.toml" 2>/dev/null
WPRC_CLI="${CARGO_TARGET_DIR:-$ROOT/target}/release/wp-rust-cache --config $ROOT/tests/php/test.toml" \
	sh "$ROOT/tests/php/recovery_test.sh" "$PHP"

# Core's object cache, the reference for the parity test.
CORE="${CORE_DIR:-/tmp/wp-core-$WP_VERSION}"
if [ ! -f "$CORE/class-wp-object-cache.php" ]; then
	mkdir -p "$CORE"
	for f in class-wp-object-cache.php cache.php; do
		curl -fsSL "https://raw.githubusercontent.com/WordPress/wordpress-develop/$WP_VERSION/src/wp-includes/$f" -o "$CORE/$f"
	done
fi

for mode in single multisite; do
	echo "-- parity with WordPress $WP_VERSION core ($mode)"
	php "$ROOT/tests/php/parity.php" core "$CORE" "$mode" > /tmp/parity-core.json
	$PHP "$ROOT/tests/php/parity.php" rust "$ROOT/wordpress/object-cache.php" "$mode" > /tmp/parity-rust.json
	if diff -u /tmp/parity-core.json /tmp/parity-rust.json; then
		echo "identical ($(grep -c '"' /tmp/parity-core.json) lines of results)"
	else
		echo "PARITY FAILURE ($mode)"
		exit 1
	fi
done
