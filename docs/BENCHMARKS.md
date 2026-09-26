# Benchmarks

Every number here was produced by a script in this repository; the raw
output of the run reported below is in [`results/`](results/). Nothing is
extrapolated. Where a result is surprising the likely cause is stated, and
results that do not favour wp-rust-cache are reported the same way.

**Machine** — Windows 11 host, Docker Desktop VM with 12 vCPUs and 8 GB RAM.
Everything a comparison involves runs in **one** container (same kernel,
CPUs, PHP 8.4 build with OPcache, PHP-FPM pool of 16 static workers, MariaDB
11.8, WordPress): nginx, PHP-FPM, MariaDB, Redis, Memcached and the load
generator share the machine, as on a real single server. Absolute numbers
are those of a desktop VM; the comparisons between backends are the point.

| Backend | Drop-in | Transport |
|---|---|---|
| none | WordPress core (non-persistent) | — |
| **rust** | `wordpress/object-cache.php` | shared memory, in-process |
| redis | Redis Object Cache (wordpress.org `redis-cache`), phpredis | Unix socket (its fastest) |
| memcached | Memcached Object Cache (wordpress.org `memcached`), PECL memcache | 127.0.0.1:11211 |

Redis and Memcached get the same memory budget (512 MB) as wp-rust-cache;
Redis runs without persistence. wp-rust-cache uses its defaults (TinyLFU).

## 1. End-to-end: nginx + PHP-FPM + WordPress

`docker/bench/rounds.sh SCENARIO 3 30` — each backend 30 s under `wrk`
(4 threads, 32 connections) after a 10 s warm-up; 3 rounds, backend order
rotated between rounds. URLs are drawn at random from 554 real URLs (home,
300 posts, 200 products, shop, search, post and product categories, an
Elementor page). CPU/req is the CPU of the whole container (PHP, MariaDB, the
cache daemon, nginx, wrk) divided by the requests served. TTFB is measured
unloaded (200 sequential requests) after the warm-up.

Site: WordPress 7.1.2, WooCommerce 11.1.2, Elementor, 3000 posts, 1000
products, 400 terms, 500 users.

### Scenario "core" — WooCommerce and Elementor deactivated (331 URLs)

| backend | req/s per round | mean | p50 | p95 | p99 | TTFB p50 | CPU/req |
|---|---|---|---|---|---|---|---|
| none | 99.0 · 94.8 · 97.8 | 97.2 | 315.6 ms | 445.3 ms | 520.2 ms | 70.0 ms | 116.9 ms |
| **rust** | 112.9 · 112.8 · 113.0 | **112.9** | **271.7 ms** | **373.8 ms** | **437.5 ms** | **58.0 ms** | **101.1 ms** |
| redis | 94.2 · 94.3 · 89.5 | 92.7 | 334.2 ms | 430.5 ms | 491.2 ms | 69.1 ms | 109.7 ms |
| memcached | 94.4 · 91.3 · 92.2 | 92.6 | 333.3 ms | 456.3 ms | 519.3 ms | 77.0 ms | 118.8 ms |

### Scenario "full" — WooCommerce + Elementor active (554 URLs)

| backend | req/s per round | mean | p50 | p95 | p99 | TTFB p50 | CPU/req |
|---|---|---|---|---|---|---|---|
| none | 35.0 · 35.3 · 35.2 | 35.2 | 884.5 ms | 1155.7 ms | 1293.6 ms | 175.2 ms | 325.2 ms |
| **rust** | 40.1 · 40.6 · 39.1 | **39.9** | **776.0 ms** | **1036.8 ms** | **1139.8 ms** | **144.2 ms** | **284.5 ms** |
| redis | 35.8 · 34.2 · 35.5 | 35.2 | 883.3 ms | 1150.0 ms | 1267.2 ms | 165.0 ms | 298.1 ms |
| memcached | 34.4 · 33.9 · 34.6 | 34.3 | 903.8 ms | 1173.8 ms | 1317.5 ms | 189.7 ms | 329.8 ms |

Latency columns are the mean of the three rounds. No request failed.

**Reading.**

* wp-rust-cache served the most requests with the least CPU in every round
  of both scenarios: +16 % requests/s over no cache and +22 % over Redis on
  plain WordPress; +13 % over both no cache and Redis with WooCommerce and
  Elementor. Unloaded TTFB is 17–18 % lower than without a cache.
* **Redis and Memcached were not faster than no object cache** on this
  server: 5 % slower on plain WordPress, equal or 3 % slower with
  WooCommerce. MariaDB runs on the same machine with a warm buffer pool, so
  the queries a network cache saves are cheap, while every lookup that misses
  the request's local array costs a round trip (150–200 µs here, section 2).
  wp-rust-cache pays ~4 µs for the same lookup and keeps the saved queries as
  profit. With a remote or loaded database Redis and Memcached would look
  better against "none"; the gap to wp-rust-cache comes from the per-lookup
  cost and would remain.
* Absolute throughput is low because these pages are expensive: a profile of
  one warm request showed ~5 ms in MariaDB and ~200 ms in PHP (WordPress 7.1 +
  WooCommerce + Elementor compile to 2167 scripts) on this VM. An object cache
  only removes database work, so no backend can move these numbers by more
  than tens of percent. That is a job for a page cache.
* Memory after a run for the same data: 4.7–5.1 MB allocated in wp-rust-cache,
  7.2–8.0 MB `used_memory` in Redis, 6.1–7.0 MB `bytes` in Memcached.

An earlier series (before the group model of layout version 2, see
ARCHITECTURE.md §6) gave the same ranking: rust 111.2 / 38.7 req/s against
Redis 84.0 / 31.6 and none 96.2 / 32.6.

## 2. `wp_cache_*` inside WordPress

`docker/bench/bench-ops.sh 10 1,2,4,8,16,32` — N `wp eval-file` processes
(WordPress bootstrapped, the drop-in under test installed) each simulate
WooCommerce page views for 10 s: `alloptions`, `notoptions`, `last_changed`,
10 posts + post meta, 6 products + meta, 5 terms, 2 users, the visitor's
session with a TTL, and an occasional post update — with the **real values**
WordPress stores (`WP_Post` objects, meta arrays, `WP_Term`, user rows).
Before every simulated request the drop-in's in-request array is emptied, so
every first lookup reaches the backend; a miss is followed by a set. Latency
is `hrtime()` around one `wp_cache_get()`/`wp_cache_set()` call: the drop-in's
PHP code and (un)serialization included — what WordPress actually waits for.
All workers start together behind a barrier.

```
backend    workers       ops/s   get p50   get p95   get p99   set p50   set p95   set p99    CPU/op
rust             1      158187    3.6µs   14.3µs   49.2µs    3.6µs   49.2µs   73.7µs     6.39µs
rust             2      301778    3.6µs   14.3µs   49.2µs    4.6µs   53.2µs   81.9µs     6.70µs
rust             4      559666    3.8µs   15.4µs   53.2µs    4.6µs   53.2µs   98.3µs     7.19µs
rust             8      781048    4.6µs   16.4µs   90.1µs    7.2µs   30.7µs  122.9µs    10.27µs
rust            16      779217    6.7µs   28.7µs   98.3µs    9.2µs   36.9µs  147.5µs    13.85µs
rust            32      728396    6.7µs   30.7µs  147.5µs    9.2µs   45.1µs     1.8ms    16.20µs
redis            1        6363  147.5µs  229.4µs  327.7µs  147.5µs  229.4µs  327.7µs   156.43µs
redis            2       11982  163.8µs  294.9µs  426.0µs  163.8µs  262.1µs  458.8µs   134.73µs
redis            4       21920  180.2µs  294.9µs  426.0µs  196.6µs  294.9µs  426.0µs   116.91µs
redis            8       23689  327.7µs  524.3µs  655.4µs  360.4µs  458.8µs  589.8µs   125.00µs
redis           16       24513  655.4µs  917.5µs     1.2ms  720.9µs  917.5µs     1.3ms   136.73µs
redis           32       23060     1.4ms     2.1ms     3.7ms     1.4ms     2.4ms     3.9ms   170.88µs
memcached        1        4992  196.6µs  262.1µs  852.0µs  180.2µs  245.8µs  655.4µs   212.04µs
memcached        2        8938  213.0µs  294.9µs  917.5µs  213.0µs  294.9µs  917.5µs   229.44µs
memcached        4       16591  229.4µs  327.7µs  983.0µs  213.0µs  294.9µs  393.2µs   240.83µs
memcached        8       27935  262.1µs  458.8µs     1.2ms  245.8µs  393.2µs  720.9µs   247.43µs
memcached       16       44710  294.9µs     1.0ms     1.6ms  294.9µs  720.9µs     1.4ms   204.91µs
memcached       32       53565  393.2µs     1.8ms     3.4ms  393.2µs     1.4ms     2.9ms   198.23µs
```

**Reading.**

* A `wp_cache_get()` that reaches the backend costs 3.6 µs (P50) with
  wp-rust-cache, 148 µs with Redis and 197 µs with Memcached on this machine.
  Most of the 3.6 µs is PHP (the drop-in's method call and the `unserialize()`
  of a `WP_Post`); the engine's share is ~0.4 µs (section 3).
* The get P99 of ~49 µs at one worker is `alloptions`: one lookup in ~43 per
  simulated request is the unserialize of that array.
* wp-rust-cache reaches ~780 000 ops/s at 8–16 processes and stays above
  700 000 at 32 (12 CPUs). The set P99 of 1.8 ms at 32 workers is processes
  preempted while holding a shard lock. Redis stops at ~24 000 ops/s — one
  single-threaded server; Memcached keeps scaling to ~54 000.
* Round trips cost more inside the Docker Desktop VM than on bare metal, for
  Redis and Memcached alike. These ratios should not be assumed for bare-metal
  hardware: run the script there.

Two measurement traps met while building this benchmark, both handled by the
script because each would invalidate a comparison: the Memcached drop-in does
not implement `wp_cache_flush_runtime()` (a first run showed "2.6 µs"
Memcached gets that were hits in its in-request array), and it records every
operation in `$group_ops`, which in a long-lived process grew until the VM ran
out of memory at 32 workers. The script empties `$cache` and `$group_ops` at
each simulated request, which is what the end of a real request does.

**After the hardening of v0.1.0** (shard bounds kept in process memory,
64-bit group ids, segment ownership checks), a shorter re-run on the same
machine gave: 1 worker 161 933 ops/s, get P50 3.6 µs (unchanged); 8 workers
693 944 ops/s (−11 %); 16 workers 770 028 ops/s (−1 %). The engine benchmark
at 8 workers went from 63 831 to 55 063 requests/s (−14 %) with unchanged
single-worker figures. Whether the multi-worker drop is the extra checks or
noise from other containers on the VM was not established; a repeated run
would tell.

## 3. Engine (no PHP)

`wprc-bench engine` forks N worker processes on one shared segment and runs
WordPress-shaped traffic (per request: `alloptions` 120 KB, `notoptions`,
8 options, `last_changed`, 10 posts + post meta, 5 terms + term meta,
2 users + user meta, 2 transients with TTL; Zipf popularity; a miss is
followed by a set). Latency is one `Cache::get`/`set` call timed in the worker
(the timer adds ~20 ns). 256 MB, 64 shards, LRU.

```
workers  requests/s        ops/s  get p50  get p95  get p99  set p50  set p95  set p99   CPU/req    hit%  contended
      1       25664      1217680    384ns    1.4µs    3.8µs    6.7µs   18.4µs   30.7µs    39.2µs   99.1%      0.00‰
      2       41105      1944110    480ns    1.8µs    5.1µs    6.7µs   18.4µs   49.2µs    46.7µs   99.5%     10.67‰
      4       65549      3093884    576ns    2.3µs   10.2µs    7.2µs   36.9µs   53.2µs    53.0µs   99.7%     27.28‰
      8       63831      3013029    768ns    4.1µs   41.0µs   10.2µs   53.2µs   73.7µs    89.9µs   99.7%     43.74‰
     16       58842      2778351    960ns    6.1µs  147.5µs   13.3µs   57.3µs  106.5µs   136.2µs   99.6%     55.18‰
     32       61595      2907850    1.0µs   10.2µs  360.4µs   14.3µs   57.3µs  180.2µs   143.2µs   99.6%     61.84‰
```

Same engine, 100-byte values (`--workload get`, 100 % hits, Zipf 0.99 over
100 000 keys):

```
workers  requests/s        ops/s  get p50  get p95  get p99   contended
      1       92343      1846860    288ns    640ns    896ns      0.00‰
      4      282425      5648500    416ns    768ns    2.3µs     24.88‰
      8      402715      8054300    512ns    1.4µs    3.1µs     53.95‰
     16      481067      9621340    576ns    2.3µs    3.8µs     76.52‰
     32      489093      9781860    576ns    2.3µs    4.6µs     79.00‰
```

**Reading.** With small values the engine scales to ~9.8 M ops/s and keeps
P99 under 5 µs with 32 processes on 12 CPUs. The WordPress-shaped load stops
scaling at 4–8 workers and its tail grows: every request copies the 120 KB
`alloptions` value — one key, so one shard — under that shard's lock. This is
a real WordPress hotspot (Redis has it too: one key, one single-threaded
server). The obvious next optimisation, copying large values outside the
lock, is **not** implemented yet; sections 1–2 show how little it weighs with
PHP in the loop (the engine does 25 000 requests/s per worker, PHP renders
~100).

## 4. Eviction policy: LRU vs TinyLFU

`wprc-bench eviction` replays the same traffic in one process through caches
smaller than the working set (16 shards; the first 20 % of 150 000 requests
is warm-up).

```
WooCommerce-shaped (plus a per-visitor session rewritten on every request, 200 000 visitors)
  memory   policy      hit%  misses/req  evictions   rejected
16.00 MB      lru    70.25%       17.85    2143085          0
16.00 MB  tinylfu    73.91%       15.66     319247    1560560
32.00 MB      lru    78.89%       12.67    1521872          0
32.00 MB  tinylfu    81.40%       11.16     268853    1070894
64.00 MB      lru    87.19%        7.69     923985          0
64.00 MB  tinylfu    88.47%        6.92     250974     580120
128.00 MB     lru    94.69%        3.19     382889          0
128.00 MB tinylfu    95.67%        2.60      58839     252664
256.00 MB     lru    98.90%        0.66      57982          0
256.00 MB tinylfu    98.92%        0.65       9066      50323

WordPress-shaped
16.00 MB      lru    77.56%       10.55    1266729          0
16.00 MB  tinylfu    79.95%        9.42     329473     801892
32.00 MB      lru    85.77%        6.69     803643          0
32.00 MB  tinylfu    87.13%        6.05     326307     400463
64.00 MB      lru    93.09%        3.25     390976          0
64.00 MB  tinylfu    93.11%        3.24     347751      42489
128.00 MB     lru    99.23%        0.36      45670          0
128.00 MB tinylfu    99.38%        0.29       5506      29194

Multisite (3 blogs)
32.00 MB      lru    74.28%       12.09    1451242          0
32.00 MB  tinylfu    76.35%       11.11     338706     996023
64.00 MB      lru    82.50%        8.22     988500          0
64.00 MB  tinylfu    83.84%        7.59     383402     526949
128.00 MB     lru    90.03%        4.69     559740          0
128.00 MB tinylfu    90.22%        4.60     495129      60574
```

**Decision.** TinyLFU was never worse than LRU and, when memory is short,
saves up to 2.2 database lookups per request (+3.7 points of hit ratio). It
evicts up to 7× less, because one-hit keys (sessions of visitors who never
return, unique query results) are refused instead of pushing out popular
entries. Its cost is a few counter increments per `get`, tens of
nanoseconds, against a miss that costs a MySQL query. **TinyLFU is the
default**; `eviction = "lru"` remains available.

Refusing a key is always safe: only a key that is not in the cache can be
refused, so no older value is ever left behind.

## Reproduce

```bash
docker build -f docker/dev.Dockerfile --build-arg PHP_VERSION=8.4 -t wprc-dev:8.4 docker
docker build -f docker/bench/Dockerfile -t wprc-bench .
docker run -d --init --name wprc-srv --memory=5g --shm-size=1g \
  -v wprc-www:/var/www/html -v wprc-db:/var/lib/mysql wprc-bench
# the first start installs WordPress, WooCommerce, Elementor and the content (~10 min)
docker exec wprc-srv bash /bench/all.sh      # every check and benchmark, ~1 hour
docker cp wprc-srv:/results docs/results
```

`--memory` protects the Docker VM: a runaway process kills the container,
not the Docker engine (which happened twice while this benchmark was built).
