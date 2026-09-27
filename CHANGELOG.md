# Changelog

## v0.1.2 — 2026-09-27

From the first production install: on a 2 GB server without swap, the
1 GB segment picked by v0.1.0 got PHP-FPM OOM-killed 84 times in a day.

- `install` sizes the cache from the RAM (container limits included): a
  tenth of it, at most half of the free `/dev/shm`, at most 1 GB. It used to
  look at `/dev/shm` alone, which in containers can exceed the RAM.
- `install` and `status` warn when `memory` is above 25 % of the RAM.
- Shard resets are counted by cause: the lock owner died (OOM kill,
  `kill -9`, crash), an operation was left half-done, a consistency check
  failed. `stats` shows when the last one happened and why, Prometheus gets
  `wp_rust_cache_recoveries_by_cause_total`, and the worker that performs a
  reset writes a line to the PHP error log.
- docs/OPERATIONS.md: a "Memory" section (non-reclaimable RAM, OOM kills,
  how to resize and make sure the old segment is released).
- Segment layout unchanged (new counters live in space older segments hold
  as zeros): upgrading keeps the cache.

## v0.1.1 — 2026-09-27

From the first production install (WooCommerce on Ubuntu 26.04, PHP 8.5).

- `stats --groups` folds numbered groups into one row per family
  (`product_4428`, `product_4429`, … → `product_*` with the number of groups;
  names the directory truncated fold too), and shows the 30 largest rows
  unless `--all` is given.
- `stats --keys GROUP [--namespace NS] [--all]`: the largest keys of a group,
  read from the segment, with type, value size, memory, TTL and blog. Finds
  the group by name, so the (often long) namespace is not needed.
- `stats --prometheus` folds numbered groups the same way: one label per
  WooCommerce product would have created a series per product.
- No change to the extension, the drop-in behaviour or the segment layout:
  upgrading keeps the cache.

## v0.1.0 — 2026-09-27

First release.

- Shared-memory engine in Rust: offset-only layout, per-shard robust mutexes
  (a worker killed while holding a lock costs one shard reset), TLSF
  allocator, TTL, O(1) flushes through generation counters, TinyLFU
  admission (default) or LRU.
- Groups are hashed into keys: any number of groups (WooCommerce creates one
  per product).
- PHP extension (C shim + Rust static library) for PHP 8.2–8.5, NTS and ZTS.
- `object-cache.php` drop-in: behaviour identical to WordPress core's cache
  (parity test against WordPress 6.8 and 7.1, single site and multisite),
  WP-CLI commands, Site Health test.
- `wp-rust-cache` command: status, stats (JSON, Prometheus), flush, verify,
  recreate, install, uninstall.
- Security: a segment is trusted only when owned by the process user, root,
  or the configured owner; world-accessible segments are refused.
- Debian package with modules for PHP 8.2–8.5, tested with systemd on
  Ubuntu 26.04 (PHP 8.5, the primary target), Ubuntu 24.04 and Debian 13.
