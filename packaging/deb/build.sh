#!/bin/sh
# build.sh VERSION ARCH ARTIFACTS OUTDIR builds the Debian package from
# artifacts already built for ARCH:
#   ARTIFACTS/wp-rust-cache              the CLI
#   ARTIFACTS/php-8.X/wp_rust_cache.so   one module per PHP version
# Needs dpkg-deb. packaging/build-all.sh produces ARTIFACTS with Docker.
set -eu

if [ $# -ne 4 ]; then
    echo "usage: $0 VERSION ARCH ARTIFACTS OUTDIR" >&2
    exit 2
fi
version=${1#v}
arch=$2
art=$3
out=$4

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
chmod 0755 "$root"

install -D -m 0755 "$art/wp-rust-cache" "$root/usr/bin/wp-rust-cache"
found=0
for dir in "$art"/php-*; do
    [ -f "$dir/wp_rust_cache.so" ] || continue
    install -D -m 0644 "$dir/wp_rust_cache.so" "$root/usr/lib/wp-rust-cache/$(basename "$dir")/wp_rust_cache.so"
    found=$((found + 1))
done
if [ "$found" = 0 ]; then
    echo "no PHP modules in $art" >&2
    exit 1
fi
install -D -m 0644 "$repo/wordpress/object-cache.php" "$root/usr/share/wp-rust-cache/object-cache.php"
install -D -m 0644 "$repo/packaging/config.toml.example" "$root/usr/share/wp-rust-cache/config.toml.example"
for doc in README.md CHANGELOG.md docs/ARCHITECTURE.md docs/OPERATIONS.md docs/BENCHMARKS.md; do
    install -D -m 0644 "$repo/$doc" "$root/usr/share/doc/wp-rust-cache/$(basename "$doc")"
done
install -D -m 0644 "$repo/LICENSE" "$root/usr/share/doc/wp-rust-cache/copyright"

install -d "$root/DEBIAN"
for script in postinst prerm postrm; do
    install -m 0755 "$here/$script" "$root/DEBIAN/$script"
done
size=$(du -sk "$root" | cut -f1)
sed -e "s/@VERSION@/$version/" -e "s/@ARCH@/$arch/" -e "s/@SIZE@/$size/" \
    "$here/control" >"$root/DEBIAN/control"

mkdir -p "$out"
deb="$out/wp-rust-cache_${version}_${arch}.deb"
dpkg-deb --root-owner-group -Zxz --build "$root" "$deb" >/dev/null
echo "$deb"
