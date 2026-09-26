#!/bin/sh
# packaging/build-all.sh — builds the CLI and one PHP module per PHP version
# in Debian 12 containers (glibc 2.36: the result runs on Debian 12 and 13
# and Ubuntu 24.04), then the .deb, into dist/.
#   PHP_VERSIONS="8.2 8.3 8.4 8.5" (default)
# Needs Docker. On Windows (Git Bash) run it with MSYS_NO_PATHCONV=1.
set -eu

repo=$(cd "$(dirname "$0")/.." && { pwd -W 2>/dev/null || pwd; })
versions=${PHP_VERSIONS:-"8.2 8.3 8.4 8.5"}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)
art="$repo/dist/artifacts"
rm -rf "$art"
mkdir -p "$art"

for v in $versions; do
    echo "== PHP $v" >&2
    docker build -q -f "$repo/docker/dev.Dockerfile" \
        --build-arg PHP_VERSION="$v" --build-arg PHP_VARIANT=cli-bookworm \
        -t "wprc-build:$v" "$repo/docker" >/dev/null
    docker run --rm \
        -v "$repo:/src" \
        -v wprc-cargo:/usr/local/cargo/registry \
        -v "wprc-target-bookworm-$v:/src/target" \
        -e BUILD_DIR=/tmp/ext \
        "wprc-build:$v" sh -c "
            set -e
            sh php-extension/build.sh >/dev/null 2>/tmp/build.log || { tail -40 /tmp/build.log; exit 1; }
            mkdir -p dist/artifacts/php-$v
            strip --strip-unneeded -o dist/artifacts/php-$v/wp_rust_cache.so /tmp/ext/modules/wp_rust_cache.so
            php -n -d extension=\$PWD/dist/artifacts/php-$v/wp_rust_cache.so -r 'exit(extension_loaded(\"wp_rust_cache\") ? 0 : 1);'
            if [ ! -f dist/artifacts/wp-rust-cache ]; then
                cargo build --release -p wp-rust-cache 2>/dev/null
                strip -o dist/artifacts/wp-rust-cache target/release/wp-rust-cache
            fi
        "
done

docker run --rm -v "$repo:/src" -w /src debian:bookworm \
    sh -c 'sh packaging/deb/build.sh "$0" "$(dpkg --print-architecture)" dist/artifacts dist' "$version"
