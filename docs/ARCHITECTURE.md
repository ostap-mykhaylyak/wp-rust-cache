# wp-rust-cache — architecture

This document is the analysis that precedes the code, and the architecture that
came out of it. Every decision below names the alternative it rejected and why.

## 1. What WordPress actually asks of an object cache

The drop-in `wp-content/object-cache.php` replaces `wp-includes/cache.php`
entirely. WordPress loads it from `wp_start_object_cache()` before plugins,
calls `wp_cache_init()`, then registers global groups (`users`, `site-options`,
`blog-details`, …) and non-persistent groups (`counts`, `plugins`,
`theme_json`, …). The drop-in must provide every `wp_cache_*` function and a
global `$wp_object_cache` of class `WP_Object_Cache`.

Semantics the backend must reproduce exactly (from core `class-wp-object-cache.php`):

| Behaviour | Core rule |
|---|---|
| Miss | `get()` returns `false`, `$found = false`. Storing `false` is legal: `$found` is what tells them apart. |
| Keys | `int` or non-empty (after `trim`) `string`; anything else → `_doing_it_wrong`, return `false`. `5` and `"5"` are the same key. |
| Empty group | becomes `'default'`. |
| Objects | stored and returned as **clones** — mutating a returned object never changes the cache. |
| `add` | fails if the key exists; also fails while `wp_suspend_cache_addition()` is on. |
| `replace` | fails if the key does not exist. |
| `incr`/`decr` | `false` if the key is missing; a non-numeric value becomes `0` first; the result is floored at `0`; the type follows PHP arithmetic (`int`, or `float` for float values). |
| `flush` | empties everything, all blogs. |
| `flush_group` | empties one group across all blogs. |
| `flush_runtime` | empties the in-request cache only. |
| Multisite | non-global groups are prefixed with the current blog ID; global groups are not; `switch_to_blog()` changes the prefix. |
| `wp_cache_supports()` | feature probe for `add_multiple`, `set_multiple`, `get_multiple`, `delete_multiple`, `flush_runtime`, `flush_group`. |

Every mature persistent drop-in (Redis Object Cache, the Memcached drop-in)
keeps an **in-request array** in front of the remote store: WordPress reads the
same option dozens of times per request, and a PHP array lookup is still
faster than any shared store. Some plugins (and Query Monitor) read
`$wp_object_cache->cache`, `cache_hits`, `cache_misses` directly. We keep all
of that: the shared store is the *second* level, the PHP array stays the first.

## 2. The PHP-FPM lifecycle and why it shapes the design

```
master (root)  MINIT ──fork──► worker (pool user)  RINIT … request … RSHUTDOWN  (×pm.max_requests)
```

* **Do not attach in `MINIT`.** The master runs as root and its mappings are
  inherited by every pool, whatever their user. That would let pool A read pool
  B's cache. Each worker attaches lazily, on first use, as its own user: the
  file permissions of the segment then *are* the access control.
* Attach once per worker, not per request. The mapping lives as long as the
  process; per request we only do one atomic load and one `fstat` to notice a
  segment that was retired or unlinked (section 5.6).
* ZTS builds (FrankenPHP, threaded SAPIs) run many requests in one process on
  many threads. The mapping is process-global; everything process-local that
  is mutable (scratch buffers, the group-id cache) is thread-local.

## 3. Binding PHP to Rust: decision

Candidates evaluated:

| Option | Verdict |
|---|---|
| **FFI (`ext-ffi`)** | Rejected by requirement, and rightly: per-call marshalling costs more than the operation. |
| **ext-php-rs** | Idiomatic, but: needs libclang/bindgen per PHP build; support for a new PHP minor depends on the crate catching up (8.5 was not advertised when checked); and — decisive — our functions must call `php_var_serialize`/`php_var_unserialize`, `emalloc` and user code (`__wakeup`, `__unserialize`). Any of those can `zend_bailout()` (fatal error, `memory_limit`), which is a `longjmp`. A `longjmp` across Rust frames is undefined behaviour, and across a held shared-memory lock it is a deadlock. |
| **Thin C shim + Rust static library** ✅ | A standard `phpize` extension (~600 lines of C) owns everything that touches `zval`s; the Rust library owns everything that touches shared memory and never sees a `zval`. Compiles against any PHP 8.2–8.5 header set, NTS or ZTS, with no third-party binding layer. |

The boundary is a C ABI of byte slices:

```
C shim  : zval → (tag, bytes)        Rust: lock shard → copy bytes → unlock
C shim  : (tag, bytes) → zval        Rust: lock shard → copy to thread-local buffer → unlock
```

Rust never calls into the Zend engine and C never holds a Rust lock, so a
PHP fatal error can never happen while a shard lock is held. The price is one
`memcpy` of the value on `get` (shared memory → scratch buffer → `zend_string`);
it is the copy that buys bailout safety, and it is measured, not assumed.

All logic — hash table, allocator, eviction, TTL, generations, statistics —
is in Rust. The C shim is a codec plus argument parsing.

## 4. Serialization

WordPress stores strings, ints, floats, bools, `null`, arrays and objects.

| PHP type | Stored as | Why |
|---|---|---|
| `null`, `false`, `true` | tag only | no payload |
| `int` | tag + 8 bytes LE | `incr`/`decr` work on it in place, atomically, under the shard lock |
| `float` | tag + 8 bytes | exact round-trip |
| `string` | tag + raw bytes | the most frequent type; no serializer at all |
| `array`, `object` | tag + `php_var_serialize()` output | the only format that preserves `__sleep`/`__wakeup`, `__serialize`/`__unserialize`, `Serializable`, references inside arrays, and class identity — i.e. exactly what `unserialize(serialize($x))` gives, which is what every persistent drop-in already returns |

Rejected: a custom encoder for arrays/objects (it would re-implement PHP's
object semantics and get edge cases wrong); `igbinary` as default (not always
installed; can be added later behind a tag without changing the layout).

Values that PHP cannot serialize (closures) make `serialize()` throw. Core's
non-persistent cache stores them happily, so the drop-in keeps them in the
in-request array, clears the exception, and **deletes** any older copy from
shared memory so no stale value can surface in the next request.

## 5. Shared memory

### 5.1 Segment

One file in `/dev/shm` (tmpfs, never touches a disk), by default
`/dev/shm/wp-rust-cache`. The path is deliberately **not** per-user by
default: WP-CLI is often run as root, and a root process attached to a
different segment than PHP-FPM would write to the database without
invalidating what the workers cached. Root opens the FPM user's segment; it
creates one only when `[shared_memory] owner` names the user to give it to.
Multi-user servers set one path per pool (`php_admin_value[wp_rust_cache.segment]`),
and the file permissions keep pools apart. Created with `O_CREAT|O_EXCL|O_NOFOLLOW`,
explicit `fchmod` (default `0600`, `0660` when a group is configured),
`posix_fallocate` to reserve the whole size up front, then `mmap(MAP_SHARED)`.
An existing segment is used only if it is owned by the process user, root, or
the configured `owner` (with the configured `group` for deliberate sharing):
its contents reach `unserialize()`, so a file planted in the world-writable
`/dev/shm` by another local user must never be trusted.

`posix_fallocate` is deliberate: a tmpfs that runs out of pages delivers
`SIGBUS` on write, which would kill workers in the middle of a request. Failing
at attach time instead is recoverable (the drop-in falls back to a
non-persistent cache).

Attaching is serialised with `flock(LOCK_EX)` on the file — released by the
kernel if the attaching process dies — so exactly one process initialises a
new segment; others wait for it and then validate the header.

### 5.2 Layout (all offsets, no pointers)

```
┌──────────────── Header (4 KiB) ────────────────┐
│ magic "WPRCACHE", layout version, state,        │
│ retired flag, sizes, shard count, checksum       │
├──────────────── Name directory ─────────────────┤
│ diagnostics only: identity → name, best-effort   │
├──────────────── Generations ────────────────────┤
│ 65 536 × AtomicU32 flush counters, by hash       │
├──────────────── Shard 0 … Shard S-1 ────────────┤
│ ShardHeader: robust mutex, LRU head/tail,        │
│   TLSF bitmaps + free lists, counters,           │
│   latency histograms                             │
│ Bucket array (u32 offsets)                       │
│ TinyLFU sketch (only when enabled)               │
│ Heap managed by TLSF                             │
└──────────────────────────────────────────────────┘
```

Every reference inside the segment is a `u32` offset relative to its shard
base (a shard is at most 4 GiB; the total is shards × shard size). Nothing
stored in the segment is a Rust type with a pointer, a `Vec`, a `String`, or a
`zval`. All structures are `#[repr(C)]` with explicit sizes and alignment, and
the layout version is part of the header: a new extension that finds an older
layout retires the segment and creates a new one.

### 5.3 Entry

One allocation per entry: header, key and value are contiguous.

```
TLSF block header (8 B: prev-physical offset, size | flags)
Entry header (40 B): hash, hash-chain next, LRU prev/next, expires_at,
                     key length, value length, value tag
Composite key: namespace counter index · namespace generation ·
               group counter index · group generation · blog id ·
               128-bit identity of (namespace, group) · raw key
Value bytes
```

### 5.4 Allocator: TLSF per shard

Values range from a few bytes (`notoptions`) to megabytes (`alloptions`,
WooCommerce arrays). Evaluated:

* slab classes (memcached): fast, but pages stay assigned to a size class
  ("slab calcification") unless a rebalancer moves them;
* buddy allocator: simple, but rounds every block to a power of two (≈25 %
  average waste);
* **TLSF** ✅: O(1) allocation and free, immediate coalescing with physical
  neighbours, bounded fragmentation, no background rebalancing. Well suited to
  a fixed-size region with variable-size blocks.

When an allocation fails the shard evicts from its LRU tail and retries,
bounded; an item larger than `max_item_size` is refused (and any older copy is
deleted).

### 5.5 Locking and memory ordering

* One **robust, process-shared `pthread_mutex_t`** per shard (never a global
  lock). The uncontended path is a single atomic in user space — no syscall.
  If the owner dies (`kill -9`), the next locker receives `EOWNERDEAD`, calls
  `pthread_mutex_consistent()` and **resets that shard**: the data it held may
  be half-written, and for a cache "empty" is always a correct state.
* A shard is marked dirty while it is being modified; a dirty shard found
  under a lock is reset for the same reason.
* Every offset read from shared memory is bounds-checked before use, against
  limits each process computes from the validated header and keeps in its own
  memory (so corrupted shared data cannot move the bounds themselves), and
  every chain walk is bounded: corrupted data produces a reset, never a
  segfault or an infinite loop. A test writes random bytes into live shards
  and keeps using the cache.
* Generations are `AtomicU32`s read with `Acquire` and bumped with `AcqRel`;
  the name directory is claimed lock-free by CAS and never read by cache
  operations.
* Contention is measured: `try_lock` first, count the failure, then block.

### 5.6 Lifecycle, crash safety, recovery

| Event | Outcome |
|---|---|
| `kill -9` a worker outside a lock | nothing to do |
| `kill -9` a worker holding a shard lock | next locker gets `EOWNERDEAD` → shard reset → continue |
| `kill -9` during segment initialisation | `flock` released by the kernel; the next attacher sees `state != READY` and re-initialises |
| PHP-FPM restart | workers re-attach to the existing segment; contents survive |
| Server restart | tmpfs is empty; first worker creates a new segment |
| Extension upgrade with a new layout / config changed size or shards / header checksum mismatch / unrecoverable mutex | the attacher sets `retired = 1` in the old header, unlinks the file and creates a new one; other workers see `retired` (or `st_nlink == 0`) at the start of their next request and re-attach |

## 6. Cache engine

* **Sharding by key hash**, not by CPU: a key must live in exactly one place
  for all processes. Shard = high bits of the 64-bit XXH3 hash, bucket = low
  bits. Shard count is configurable (default 64, power of two).
* **Index**: chained buckets of `u32` offsets; the chain link lives in the
  entry header, and the full 64-bit hash is compared before the key bytes.
* **TTL**: `expires_at` in unix seconds, checked lazily on read (an expired hit
  is a miss and is freed). The clock is `CLOCK_REALTIME_COARSE` (vDSO, no
  syscall). No global GC: every `set` inspects up to two LRU-tail entries and
  frees them if expired or stale — a light, amortised sweep.
* **Groups without a table.** A group is identified by a 128-bit hash of
  (namespace, group) written into its keys, so nothing is allocated per
  group. This matters: WooCommerce uses one group per product
  (`product_123`), so a first design with one shared slot per group would
  have run out of slots on a large store. It was found on the benchmark
  site and replaced (layout version 2).
* **Flush in O(1) with generations.** Generations live in a fixed table of
  65 536 counters; a namespace and a group each map to one counter by hash,
  and both counters are part of the composite key. Groups that share a
  counter are flushed together — extra misses, never stale data. `wp_cache_flush()` bumps the namespace generation,
  `wp_cache_flush_group()` bumps the group generation: old entries become
  unreachable instantly and are reclaimed by LRU and the tail sweep. A `set`
  that read the old generation before a concurrent flush writes an unreachable
  entry, so a flush can never be "undone" by a racing writer.
* **Namespaces** separate WordPress installs sharing one PHP user:
  `WP_CACHE_KEY_SALT` if defined, else a hash of DB host, DB name and table
  prefix.
* **incr/decr** run entirely under the shard lock on the stored value, with
  PHP's rules (`is_numeric`, int/float result, floor at 0) — no
  read-modify-write in PHP, so no lost updates.
* **Eviction**: LRU order, with TinyLFU admission by default (a count-min
  sketch with periodic halving: a new key only displaces the LRU victim if
  it has been requested more often); plain LRU is available. The default was
  chosen by the WordPress-shaped benchmark, not by assumption — see
  [BENCHMARKS.md](BENCHMARKS.md#4-eviction-policy-lru-vs-tinylfu).
* **Memory limits**: the hard limit is the segment size — the cache cannot grow
  beyond it by construction. A soft limit (default 90 % of each shard's heap)
  triggers proactive eviction on `set` to keep headroom and limit
  fragmentation.
* **Statistics**: per shard (updated under the lock the operation already
  holds, so no extra cache-line traffic): hits, misses, sets, deletes,
  evictions, expired, stale reclaimed, entries, payload bytes, allocated
  bytes, lock contention. Per group: bytes and entries. Latency: 1 operation
  in 64 is timed and recorded into a log-linear histogram, giving P50/P95/P99
  without timing the fast path.

## 7. Components

```
wp-rust-cache/
├── rust-core/        engine: segment, layout, TLSF, shard, index, LRU,
│                     TinyLFU, groups + generations, stats, config (no PHP, no I/O in the fast path)
├── php-extension/    C shim (zval codec, arginfo, ini) + ffi/ (Rust staticlib, C ABI)
├── wordpress/        object-cache.php (adapter + WP-CLI commands)
├── tools/cli/        `wp-rust-cache` status | stats | flush | verify | recreate | install
├── tools/bench/      engine benchmark (multi-process) and WordPress-shaped workloads
├── tests/            phpt tests, crash tests, WordPress integration
└── docker/           reproducible build and benchmark environment
```

A future standalone mode (PHP → Rust client → Rust cache server) is a separate
crate implementing the same byte-level operations over a socket. The C shim
talks to the FFI crate; only the FFI crate knows which backend is in use, so
the shared-memory fast path gains no branches or indirection from it.

## 8. What is deliberately not in the fast path

No network, no filesystem, no syscalls (the coarse clock is vDSO), no logging,
no metrics export, no allocation in Rust on `get` (the scratch buffer is
reused), no global lock. `wp_cache_get()` for a value already requested in
the same request never leaves PHP.
