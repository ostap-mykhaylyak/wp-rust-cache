# wp-rust-cache

A persistent object cache for WordPress that lives in shared memory, with the
engine written in Rust. It replaces Redis or Memcached on single-server
installs: PHP-FPM workers read and write one shared segment directly, with no
network, no socket, no daemon and no filesystem I/O on the cache path.

```
PHP-FPM worker ─┐
PHP-FPM worker ─┼─► wp_rust_cache extension (C shim) ─► Rust engine ─► /dev/shm segment
PHP-FPM worker ─┘
```

* **Drop-in compatible**: `wp-content/object-cache.php` implements the whole
  Object Cache API (`wp_cache_*`, `*_multiple`, `flush_group`,
  `flush_runtime`, global and non-persistent groups, multisite,
  `wp_cache_supports`). A parity test runs the same ~150 operations against
  WordPress core's `WP_Object_Cache` and against the drop-in and requires
  identical results — return values, `$found`, hit/miss counters,
  `_doing_it_wrong` notices — in single-site and multisite mode.
* **Safe by construction**: the segment holds only offsets and bytes, never
  pointers or `zval`s. Per-shard robust mutexes survive `kill -9` of a worker
  holding the lock (the shard is reset, the cache keeps working). Every
  offset is bounds-checked; `wp-rust-cache verify` walks every structure.
* **Correct concurrency**: `wp_cache_incr/decr` run atomically inside the
  segment — 100 processes × 1000 increments end exactly at 100 000.
  `wp_cache_flush` and `flush_group` are O(1) generation bumps that a racing
  writer cannot undo.
* **Any number of groups**: groups are hashed into the keys, not allocated,
  so WooCommerce's one-group-per-product works on any catalogue size.
* **Measured, not claimed** ([docs/BENCHMARKS.md](docs/BENCHMARKS.md), same
  VM for every backend): a `wp_cache_get()` reaching the backend costs
  3.6 µs against 148 µs (Redis) and 197 µs (Memcached); end to end on
  WordPress 7.1 + WooCommerce + Elementor, +13 % requests/s against both no
  cache and Redis, and −13 % / −5 % CPU per request — Redis and Memcached,
  on this server, were not faster than no object cache at all. On PHP 8.5
  the unloaded TTFB is 18–19 % lower than without a cache; the gain under
  load is smaller (+5 % to +10 %) and within the spread of that series.

**Primary target: Ubuntu 26.04 LTS with its own PHP 8.5 FPM.**

Supported: PHP 8.2, 8.3, 8.4, 8.5 (PHP-FPM, CLI; NTS and ZTS, incl.
FrankenPHP), Linux x86-64. Tested on Ubuntu 26.04 (PHP 8.5), Ubuntu 24.04
(PHP 8.3) and Debian 13 (PHP 8.4) with their own PHP-FPM under systemd, and
on the official PHP images. arm64 packages are
built by the release workflow but have not been run.

## Install

```bash
apt install ./wp-rust-cache_0.1.0_amd64.deb     # nothing is enabled yet
wp-rust-cache install --wp /var/www/html --user www-data
systemctl reload php8.5-fpm
wp-rust-cache status
```

`install` changes nothing until every check passes (PHP version, the module
loads in this PHP, WordPress root, no other cache's drop-in unless
`--force`, room in `/dev/shm`). Then it enables the extension, writes
`/etc/wp-rust-cache/config.toml` sized to `/dev/shm`, installs
`object-cache.php`, creates the segment owned by the PHP-FPM user, runs a
self-test through the extension and prints the status. `--dry-run` shows the
plan; `wp-rust-cache uninstall --wp …` reverses it.

Operations (sizing, several users, monitoring, upgrades, troubleshooting,
rollback): [docs/OPERATIONS.md](docs/OPERATIONS.md). Security model:
[SECURITY.md](SECURITY.md).

### From source

```bash
sh packaging/build-all.sh          # modules for PHP 8.2–8.5 + CLI + .deb, in Docker
# or, for the PHP in PATH only:
php-extension/build.sh             # prints the path of wp_rust_cache.so
cargo build --release -p wp-rust-cache
wp-rust-cache install --wp /var/www/html --extension /path/to/wp_rust_cache.so
```

## Configuration

```toml
# /etc/wp-rust-cache/config.toml — every key is optional
[cache]
enabled = true
memory = "1GB"           # the hard limit: the segment never grows
shards = 64              # power of two; one lock per shard
eviction = "tinylfu"     # or "lru"
max_item_size = "8MB"
soft_limit = 90          # % of each shard kept before proactive eviction

[shared_memory]
path = "/dev/shm/wp-rust-cache"
permissions = "0600"     # group/other bits for "other" are refused
owner = "www-data"       # who owns the segment when root creates it
# group = "www-data"     # with permissions = "0660"
preallocate = true       # reserve all pages now: never SIGBUS on a full tmpfs
```

PHP ini: `wp_rust_cache.enabled`, `wp_rust_cache.config`, and
`wp_rust_cache.segment` — a per-pool segment path for multi-user servers:

```ini
; /etc/php/8.5/fpm/pool.d/site1.conf
php_admin_value[wp_rust_cache.segment] = /dev/shm/wp-rust-cache-site1
```

Each pool running as its own user then has its own segment, readable by that
user only. Installs that share a segment are separated by namespace
(`WP_CACHE_KEY_SALT`, or `WP_RUST_CACHE_NAMESPACE`, else DB host + name +
table prefix), and `wp_cache_flush()` empties only its own namespace.

Run WP-CLI as the PHP-FPM user or as root: both reach the same segment.

## Diagnostics

Real output from the benchmark server (WordPress + WooCommerce + Elementor,
after 30 s of traffic):

```
$ wp-rust-cache status
WP Rust Cache

Status:       RUNNING
Backend:      shared-memory (/dev/shm/wp-rust-cache)
Memory:       4.33 MB / 495.57 MB
Entries:      11376
Hit ratio:    98.80%
Evictions:    0
P50:          960 ns
P95:          3.1 µs
P99:          11.3 µs

$ wp-rust-cache stats --groups
  ...
  namespace          group                           entries       memory      stale
  _HlldQI4kC0I*5fjy2 posts                              1352      1.35 MB          0
  _HlldQI4kC0I*5fjy2 post_meta                           914    980.58 KB          0
  _HlldQI4kC0I*5fjy2 post-queries                       1907    470.43 KB          0
  _HlldQI4kC0I*5fjy2 term-queries                        744    355.27 KB          0
```

The P50/P95/P99 here are engine latencies measured inside the extension
(copying the value out of shared memory included; PHP decoding excluded).

* `wp-rust-cache stats [--groups] [--json | --prometheus]` — every counter; `--groups` walks
  the segment for per-group entries and memory (no cost on the fast path).
* `wp-rust-cache flush [--namespace NS]`, `verify [--repair]`, `recreate`.
* WP-CLI: `wp rust-cache status | stats | flush [--all] | inspect <key> --group=<g>`.

Latency percentiles are sampled (1 operation in 64 is timed), so the fast
path does not pay for a clock read.

## Layout

| Path | What |
|---|---|
| `rust-core/` | the engine: segment, TLSF allocator, shards, index, LRU/TinyLFU, groups, stats |
| `php-extension/` | C shim (`wp_rust_cache.c`) + `ffi/` Rust static library with a C ABI |
| `wordpress/object-cache.php` | the drop-in, WP-CLI commands included |
| `tools/cli/` | `wp-rust-cache` |
| `tools/bench/` | engine benchmarks (`wprc-bench engine`, `wprc-bench eviction`) |
| `tests/` | PHP suites, core parity test, version matrix |
| `docker/` | build images, benchmark server (nginx + PHP-FPM + MariaDB + Redis + Memcached) |
| `docs/` | [ARCHITECTURE.md](docs/ARCHITECTURE.md), [BENCHMARKS.md](docs/BENCHMARKS.md) |

## Tests

```bash
cargo test --workspace -- --test-threads=1   # engine: kill -9, 100 processes, reference model, corruption
tests/run-php-tests.sh                       # extension + parity with WordPress core
powershell -File tests/matrix.ps1            # the above on PHP 8.2–8.5 and ZTS (Docker)
```

On the full stack (Docker):

| Script | What it proves |
|---|---|
| `docker/bench/compat.sh` | WordPress 7.1 + WooCommerce + Elementor on the cache: 554 URLs, cart, orders, WP-CLI coherence, PHP log |
| `docker/bench/crash.sh` | `kill -9` of workers under load, PHP-FPM killed, segment deleted, recreated, reconfigured |
| `docker/distro/distro-test.sh` | the .deb on Debian 13 and Ubuntu 24.04 with systemd: install, reload, upgrade, uninstall, remove, purge |
| `docker/frankenphp/test.sh` | FrankenPHP threads, classic and worker mode |
| `docker/bench/all.sh` | everything above plus every benchmark |

CI (`.github/workflows/ci.yml`) runs all of them except the benchmarks.

## Status

Working and tested: engine, extension (PHP 8.2–8.5, NTS and ZTS), drop-in,
CLI, installer and uninstaller, Debian package, crash recovery, benchmarks —
on WordPress 7.1 with WooCommerce 11 and Elementor, on Ubuntu 26.04,
Ubuntu 24.04 and Debian 13 under systemd, and on FrankenPHP; all in containers.

Not yet:

* an apt repository (the .deb is attached to releases);
* runs of the arm64 package;
* the standalone client/server mode described in the architecture;
* copying large values (e.g. `alloptions`) outside the shard lock — the
  measured hotspot of the engine under 8+ busy processes;
* igbinary encoding;
* benchmarks on bare metal: the published numbers come from a Docker
  Desktop VM.

## License

MIT
