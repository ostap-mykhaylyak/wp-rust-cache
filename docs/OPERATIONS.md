# Operating wp-rust-cache

## Ubuntu 26.04 with PHP 8.5 (the primary target)

Ubuntu 26.04 LTS ships PHP 8.5 (`php8.5-fpm`), which the package supports
directly. The whole sequence below is what CI runs on `ubuntu:26.04` under
systemd (`docker/distro/distro-test.sh`).

```bash
apt install php8.5-fpm php8.5-mysql php8.5-xml php8.5-mbstring php8.5-curl php8.5-intl
apt install ./wp-rust-cache_0.1.2_amd64.deb
wp-rust-cache install --wp /var/www/example.com --user www-data
systemctl reload php8.5-fpm
```

* The extension is enabled through `phpenmod` for every SAPI of PHP 8.5
  (`/etc/php/8.5/mods-available/wp_rust_cache.ini`, pointing at
  `/usr/lib/wp-rust-cache/php-8.5/wp_rust_cache.so`).
* `/dev/shm` is half the RAM by default. On a 4 GB server that is 2 GB, and
  `install` picks a 1 GB cache; keep `memory` at or below half of `/dev/shm`.
  To enlarge it permanently, in `/etc/fstab`:
  `tmpfs /dev/shm tmpfs defaults,size=3G 0 0`.
* Per-site pools: `/etc/php/8.5/fpm/pool.d/<site>.conf`, with
  `php_admin_value[wp_rust_cache.segment]` (see "Several PHP users").
* The Memcached drop-in from wordpress.org cannot run on PHP 8.5 (its PECL
  `memcache` extension does not build there); Redis Object Cache can.

## Install

```bash
apt install ./wp-rust-cache_0.1.2_amd64.deb
wp-rust-cache install --wp /var/www/example.com --user www-data
systemctl reload php8.5-fpm
wp-rust-cache status
```

`install` changes nothing until every check has passed: PHP ≥ 8.2, the
module loads in *this* PHP, the directory is a WordPress root, any existing
`object-cache.php` is ours (or `--force`, which backs it up), `/dev/shm` has
room. Then it enables the extension, writes `/etc/wp-rust-cache/config.toml`
if missing (sized to half the free space of `/dev/shm`, at most 1 GB),
installs the drop-in, creates the segment owned by `--user`, runs a
self-test through the extension and prints the status. `--dry-run` shows the
plan.

Nothing reloads PHP-FPM for you: the moment is yours. Until the reload,
workers do not have the extension and the drop-in behaves like WordPress's
built-in cache.

Several sites on the same PHP user share one segment and stay separate by
namespace: run `install --wp` once per site.

## Memory: read this before choosing `memory`

The segment is **RAM the kernel cannot reclaim**, and with
`preallocate = true` (the default) all of it is taken when the segment is
created, whatever the cache actually holds. It counts against the PHP-FPM
service, not against a file cache.

What happened on the first production server shows why this matters: 2 GB of
RAM, no swap, inside a container whose `/dev/shm` reported 3.9 GB. v0.1.0
sized the cache from `/dev/shm` alone and picked 1 GB. PHP-FPM, MariaDB and
the segment no longer fitted: the kernel OOM killer killed PHP workers at
every traffic peak, and with systemd's `OOMPolicy=stop` each kill stopped
and restarted the whole PHP-FPM service — 84 times in one day, each time
emptying OPcache and cutting requests off. The cache itself held 87 MB.

Since v0.1.2:

* `install` picks a tenth of the RAM (container limits included), at most
  half of the free space in `/dev/shm`, at most 1 GB;
* `install` and `wp-rust-cache status` warn when `memory` is above 25 % of
  the RAM.

How to tell if it happens to you:

```bash
systemctl show php8.5-fpm -p NRestarts          # climbing = PHP-FPM keeps failing
journalctl -u php8.5-fpm | grep -i oom          # "Failed with result 'oom-kill'"
grep -E 'MemAvailable|Shmem:' /proc/meminfo
```

How much is enough: `wp-rust-cache stats` → `allocated_bytes` after a day of
traffic, times 1.5 to 2. A WooCommerce store with ~6 000 products used about
90 MB after a few hours and levels off well below 256 MB.

To resize, change `memory` and **restart** PHP-FPM (a reload may keep old
workers, and the old segment's RAM is only returned when no process maps it
any more):

```bash
sed -i 's/^memory = .*/memory = "256MB"/' /etc/wp-rust-cache/config.toml
systemctl restart php8.5-fpm
grep Shmem: /proc/meminfo                        # should drop by the old size
```

If it does not drop, some other process still maps the old segment (a CLI
cron job, for instance):

```bash
for p in $(grep -l 'wp-rust-cache (deleted)' /proc/[0-9]*/maps); do ps -o pid,user,etime,cmd --no-headers -p ${p//[^0-9]/}; done
```

Shard resets (`recoveries` in `stats`) now come with their cause:
`recoveries_owner_died` — a process was killed while holding a shard lock
(OOM kill, `kill -9`, a crash); `recoveries_interrupted`;
`recoveries_inconsistent` — a consistency check failed, which should not
happen and is worth reporting. The worker that performs a reset also writes a
line to the PHP error log.

## Sizing

* `memory` is a hard limit: the segment is created at that size and never
  grows. With `preallocate = true` (default) the pages are reserved at once,
  so RAM usage is `memory` from the start, and a full `/dev/shm` is reported
  at creation instead of killing a worker with SIGBUS later.
* `/dev/shm` defaults to half the RAM. A configuration change creates the
  new segment while old workers still map the old one: keep room for two.
  If there is not, the new segment is created a few seconds later, when the
  old workers have been replaced; in between requests run without the
  persistent cache.
* Start with what `install` picks, then watch `wp-rust-cache status`:
  evictions that keep rising with memory near full mean the cache is too
  small. Site Health says so too.
* `max_item_size` (8 MB) caps one value. WordPress's `alloptions` is usually
  well below; if `too_large` rises in `stats`, a plugin stores huge values.

## Several PHP users (shared hosting)

Give every pool its own segment; file permissions keep them apart:

```ini
; /etc/php/8.5/fpm/pool.d/site1.conf
php_admin_value[wp_rust_cache.segment] = /dev/shm/wp-rust-cache-site1
```

Run WP-CLI for that site as its user, or as root with the same setting:
`php -d wp_rust_cache.segment=/dev/shm/wp-rust-cache-site1 $(which wp) ...`.
A WP-CLI process attached to a different segment than PHP-FPM would write
the database without invalidating what the workers cached.

## WP-CLI

WP-CLI uses the same segment as PHP-FPM when run as the FPM user or as root
(root attaches to the segment owned by `owner`). Useful commands:

```bash
wp rust-cache status
wp rust-cache stats --format=json
wp rust-cache inspect alloptions --group=options
wp rust-cache flush          # this site (all blogs of a multisite)
wp rust-cache flush --all    # every site in the segment
```

## Monitoring

* `wp-rust-cache status` / `stats [--groups] [--json]`.
* `wp-rust-cache stats --keys options` lists the largest keys of a group. A
  large non-autoloaded option there (hundreds of KB) is read and unserialized
  by every request that uses it, with any object cache: worth a look. On the
  first production site this found a 1.8 MB `wp_mail_smtp_debug` (an event
  list WP Mail SMTP never trimmed) read on every admin page.
* Prometheus, through node_exporter's textfile collector:
  ```
  * * * * * root wp-rust-cache stats --prometheus > /var/lib/node_exporter/textfile/wp_rust_cache.prom.tmp && mv /var/lib/node_exporter/textfile/wp_rust_cache.prom.tmp /var/lib/node_exporter/textfile/wp_rust_cache.prom
  ```
  Worth alerting on: `recoveries_total` increasing (a worker died holding a
  lock, or corruption was repaired), evictions rising with
  `allocated_bytes` close to `capacity_bytes`.
* Latency percentiles are sampled (1 operation in 64) inside the extension.
* When the segment cannot be used, the extension writes the reason to the
  PHP error log once per worker ("wp-rust-cache: shared memory unavailable
  …") and Site Health turns red. The site keeps working.

## Upgrades

* **Package upgrade**: the module lives in `/usr/lib/wp-rust-cache/php-X.Y/`
  and the ini points there, so `systemctl reload phpX.Y-fpm` loads the new
  build. If the new build changes the segment layout, the first new worker
  retires the old segment and creates a new, empty one; the others follow at
  their next request. The cache starts cold; nothing else happens.
* **Drop-in**: `object-cache.php` is a copy inside each site. Re-run
  `wp-rust-cache install --wp …` after an upgrade to refresh it.
* **PHP upgrade** (e.g. 8.3 → 8.4): run `install` again for the new version.

## Troubleshooting

| Symptom | Check |
|---|---|
| Site Health: "not in use", extension not loaded | `php -m \| grep wp_rust_cache` for the FPM binary; reload FPM after install |
| "belongs to uid N … refusing" | a segment owned by another user is at the path (planted, or left by another pool). Remove it, or set `owner`/`group` if sharing is intended |
| "is accessible to other users" | the file mode has bits for "other"; recreate it (`wp-rust-cache recreate`) |
| "cannot reserve … is /dev/shm large enough?" | lower `memory` or enlarge `/dev/shm` (`mount -o remount,size=… /dev/shm`) |
| "is being set up by another process" | transient during creation; persists only if a process hangs holding the file lock |
| hit ratio low, evictions high | raise `memory` |
| WP-CLI changes not seen by the site | WP-CLI attached to another segment: see "Several PHP users" |

`wp-rust-cache verify` checks every shard; `verify --repair` resets the
inconsistent ones (a reset only empties that part of the cache).
`wp-rust-cache recreate` replaces the segment with an empty one; workers
switch at their next request.

## Rollback

```bash
wp-rust-cache uninstall --wp /var/www/example.com
systemctl reload php8.5-fpm
apt remove wp-rust-cache        # purge also removes /etc/wp-rust-cache
```

`uninstall` removes our drop-in (never another cache's), disables the
extension and releases the segment, in that order. Removing the package
without `uninstall` is also safe: the extension is disabled by the package,
and a drop-in left behind falls back to WordPress's per-request cache.

## Security model

* Local mode opens no socket. The segment is a file in `/dev/shm` created
  with `O_EXCL|O_NOFOLLOW` and an explicit mode; modes with bits for
  "other" are refused.
* Values in the segment reach `unserialize()`. A segment is therefore only
  used when it is owned by the process user, by root, or by the configured
  `owner` (with the configured `group` for deliberate sharing): a file
  planted in the world-writable `/dev/shm` by another local user is refused.
* Root never creates a segment for itself: it creates it for `owner`.
* Every offset read from shared memory is bounds-checked and every walk is
  bounded; corrupted structures cause a shard reset, not a crash (tested by
  writing random bytes into live shards).
* Anyone who can write the segment can make every PHP process sharing it
  unserialize chosen data: treat write access to the segment like write
  access to the site's code.
