#!/bin/sh
# Builds the Rust library and the PHP extension for the PHP found in PATH.
#   php-extension/build.sh            → prints the path of wp_rust_cache.so
# Environment: BUILD_DIR (default /tmp/wprc-ext-build), CARGO_TARGET_DIR.
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cargo build --release -p wprc-ffi --manifest-path "$ROOT/Cargo.toml" >&2
TARGET="${CARGO_TARGET_DIR:-$ROOT/target}/release"
BUILD="${BUILD_DIR:-/tmp/wprc-ext-build}"
rm -rf "$BUILD"
mkdir -p "$BUILD"
cp "$ROOT"/php-extension/*.c "$ROOT"/php-extension/*.h "$ROOT"/php-extension/config.m4 "$BUILD"/
cd "$BUILD"
phpize >&2
./configure --enable-wp-rust-cache --with-wprc-lib="$TARGET" >&2
make -j"$(nproc)" >&2
echo "$BUILD/modules/wp_rust_cache.so"
