# Changelog

## v0.1.0 — unreleased

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
- Debian package with modules for PHP 8.2–8.5.
