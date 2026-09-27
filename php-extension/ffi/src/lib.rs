//! C ABI over `wprc-core`, linked statically into the PHP extension.
//! The declarations live in `php-extension/wprc.h`.
//!
//! Process model: one mapping per process (`GLOBAL`), and in every thread a
//! clone of it (`LOCAL`) so that the fast path touches no shared refcount
//! and no lock. When a segment is retired, the next request start replaces
//! the global mapping and bumps `EPOCH`; each thread notices the new epoch
//! on its next operation and drops its old clone, and the old mapping is
//! unmapped when the last thread lets go of it.
//!
//! Group ids handed to PHP are indices into a per-thread registry of
//! `GroupHandle`s. The drop-in resolves each group once per request, so the
//! registry is emptied at a request start once it grows large (WooCommerce
//! has one group per product).
//!
//! No function here calls into PHP, so a PHP fatal error can never unwind or
//! `longjmp` through these frames.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use wprc_core::config::DEFAULT_CONFIG_PATH;
use wprc_core::{value, AttachMode, Cache, Config, GroupHandle, SetMode, SetOutcome};

/// After a failed attach, wait this long before trying again, so a broken
/// setup costs one `open()` every few seconds rather than one per call.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// Registry size above which a request start empties it.
const REGISTRY_LIMIT: usize = 20_000;

struct Global {
    cache: Option<Arc<Cache>>,
    error: String,
    last_attempt: Option<Instant>,
}

static GLOBAL: RwLock<Global> = RwLock::new(Global {
    cache: None,
    error: String::new(),
    last_attempt: None,
});
static PARAMS: Mutex<Option<(String, String)>> = Mutex::new(None);
static EPOCH: AtomicU64 = AtomicU64::new(1);

struct Registered {
    handle: GroupHandle,
    /// Epoch whose segment directory has this group's names.
    epoch: u64,
}

struct Local {
    epoch: u64,
    cache: Option<Arc<Cache>>,
    ids: HashMap<Vec<u8>, u32>,
    /// Bumped whenever the registry is emptied; part of every id handed out,
    /// so an id from before the reset can never name another group.
    registry_gen: u32,
    groups: Vec<Registered>,
    // Boxed on purpose: the buffer address is handed to C as `owner`.
    #[allow(clippy::vec_box)]
    pool: Vec<Box<Vec<u8>>>,
    error: Vec<u8>,
}

thread_local! {
    static LOCAL: RefCell<Local> = RefCell::new(Local {
        epoch: 0,
        cache: None,
        ids: HashMap::new(),
        registry_gen: 1,
        groups: Vec::new(),
        pool: Vec::new(),
        error: Vec::new(),
    });
}

#[repr(C)]
pub struct WprcValue {
    ptr: *const c_char,
    len: usize,
    tag: u8,
    owner: *mut Vec<u8>,
}

#[repr(C)]
pub struct WprcStats {
    hits: u64,
    misses: u64,
    sets: u64,
    deletes: u64,
    evictions: u64,
    expired: u64,
    stale: u64,
    rejected: u64,
    too_large: u64,
    no_memory: u64,
    resets: u64,
    contended: u64,
    recoveries: u64,
    entries: u64,
    payload_bytes: u64,
    alloc_bytes: u64,
    heap_bytes: u64,
    total_size: u64,
    max_item_size: u64,
    created_at: u64,
    attaches: u64,
    groups_used: u64,
    namespaces: u64,
    group_slots: u64,
    groups_overflow: u64,
    shards: u64,
    get_p50: u64,
    get_p95: u64,
    get_p99: u64,
    set_p50: u64,
    set_p95: u64,
    set_p99: u64,
    get_samples: u64,
    set_samples: u64,
    policy: [c_char; 16],
    path: [c_char; 256],
}

/// # Safety
/// `p` must be null or point to `len` readable bytes.
unsafe fn slice<'a>(p: *const c_char, len: usize) -> &'a [u8] {
    if p.is_null() || len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(p as *const u8, len)
    }
}

fn guard<T: Copy>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

fn load_config() -> Result<(Config, PathBuf), String> {
    let (cfg_path, segment) = PARAMS
        .lock()
        .map(|p| p.clone())
        .unwrap_or(None)
        .unwrap_or_else(|| (DEFAULT_CONFIG_PATH.to_string(), String::new()));
    let cfg = Config::load(std::path::Path::new(&cfg_path))?;
    if !cfg.enabled {
        return Err(format!("disabled in {cfg_path}"));
    }
    let path = if segment.is_empty() {
        cfg.path()
    } else {
        let mut c = cfg.clone();
        c.path_template = segment;
        c.path()
    };
    Ok((cfg, path))
}

/// Attaches the process-wide mapping if there is none (rate-limited).
fn ensure_global() -> Option<Arc<Cache>> {
    if let Ok(g) = GLOBAL.read() {
        if let Some(c) = &g.cache {
            return Some(c.clone());
        }
        if matches!(g.last_attempt, Some(t) if t.elapsed() < RETRY_AFTER) {
            return None;
        }
    }
    let mut g = GLOBAL.write().ok()?;
    if let Some(c) = &g.cache {
        return Some(c.clone());
    }
    if matches!(g.last_attempt, Some(t) if t.elapsed() < RETRY_AFTER) {
        return None;
    }
    g.last_attempt = Some(Instant::now());
    let attached = load_config().and_then(|(cfg, path)| {
        Cache::attach_path(&cfg, &path, AttachMode::Create).map_err(|e| e.to_string())
    });
    match attached {
        Ok(c) => {
            let c = Arc::new(c);
            g.cache = Some(c.clone());
            g.error.clear();
            EPOCH.fetch_add(1, Ordering::AcqRel);
            Some(c)
        }
        Err(e) => {
            g.error = e;
            None
        }
    }
}

#[inline]
fn guard_local<T: Copy>(fallback: T, f: impl FnOnce(&mut Local) -> T) -> T {
    // A failure here (panic, TLS already destroyed during thread exit,
    // re-entry) must never propagate into PHP; the caller sees `fallback`.
    catch_unwind(AssertUnwindSafe(|| {
        LOCAL.with(|l| match l.try_borrow_mut() {
            Ok(mut l) => f(&mut l),
            Err(_) => fallback,
        })
    }))
    .unwrap_or(fallback)
}

/// Brings this thread's view up to date with the process mapping.
#[inline]
fn refresh(l: &mut Local) {
    let epoch = EPOCH.load(Ordering::Acquire);
    if l.epoch != epoch || l.cache.is_none() {
        l.cache = ensure_global();
        l.epoch = EPOCH.load(Ordering::Acquire);
    }
}

/// Runs `f` with this thread's cache and whole local state.
#[inline]
fn with<T: Copy>(fallback: T, f: impl FnOnce(&Cache, &mut Local) -> T) -> T {
    guard_local(fallback, |l| {
        refresh(l);
        match l.cache.clone() {
            Some(c) => f(&c, l),
            None => fallback,
        }
    })
}

/// Fast path: the cache, the handle of `gid`, and the buffer pool, without
/// cloning the `Arc` or touching anything shared.
#[inline]
fn with_group<T: Copy>(
    fallback: T,
    gid: u64,
    #[allow(clippy::vec_box)] f: impl FnOnce(&Cache, &GroupHandle, &mut Vec<Box<Vec<u8>>>) -> T,
) -> T {
    guard_local(fallback, |l| {
        refresh(l);
        if (gid >> 32) as u32 != l.registry_gen {
            return fallback; // an id from before the registry was emptied
        }
        let Local {
            cache,
            groups,
            pool,
            ..
        } = l;
        match (cache.as_deref(), groups.get((gid & 0xFFFF_FFFF) as usize)) {
            (Some(c), Some(r)) => f(c, &r.handle, pool),
            _ => fallback,
        }
    })
}

/// # Safety
/// Pointer arguments must be valid for the lengths given, as declared in
/// `php-extension/wprc.h`.
#[no_mangle]
pub unsafe extern "C" fn wprc_request_start(
    config: *const c_char,
    config_len: usize,
    segment: *const c_char,
    segment_len: usize,
) {
    guard((), || {
        if let Ok(mut p) = PARAMS.lock() {
            if p.is_none() {
                // SAFETY: the C caller passes valid (pointer, length) pairs.
                let (c, s) = unsafe { (slice(config, config_len), slice(segment, segment_len)) };
                let c = String::from_utf8_lossy(c).into_owned();
                *p = Some((
                    if c.is_empty() {
                        DEFAULT_CONFIG_PATH.into()
                    } else {
                        c
                    },
                    String::from_utf8_lossy(s).into_owned(),
                ));
            }
        }
        let stale = match GLOBAL.read() {
            Ok(g) => g.cache.as_ref().is_some_and(|c| c.is_stale()),
            Err(_) => false,
        };
        if stale {
            if let Ok(mut g) = GLOBAL.write() {
                if g.cache.as_ref().is_some_and(|c| c.is_stale()) {
                    g.cache = None;
                    g.last_attempt = None;
                    EPOCH.fetch_add(1, Ordering::AcqRel);
                }
            }
        }
        // Group ids do not outlive a request in the drop-in.
        guard_local((), |l| {
            if l.groups.len() > REGISTRY_LIMIT {
                l.groups.clear();
                l.ids.clear();
                l.registry_gen = l.registry_gen.wrapping_add(1).max(1);
            }
        });
    })
}

#[no_mangle]
pub extern "C" fn wprc_ready() -> i32 {
    with(0, |_, _| 1)
}

/// # Safety
/// Pointer arguments must be valid for the lengths given, as declared in
/// `php-extension/wprc.h`.
#[no_mangle]
pub unsafe extern "C" fn wprc_error(len: *mut usize) -> *const c_char {
    guard_local(std::ptr::null(), |l| {
        l.error = GLOBAL
            .read()
            .map(|g| g.error.clone().into_bytes())
            .unwrap_or_default();
        l.error.push(0);
        if !len.is_null() {
            // SAFETY: caller passes a valid out-pointer.
            unsafe { *len = l.error.len() - 1 };
        }
        l.error.as_ptr() as *const c_char
    })
}

/// # Safety
/// Pointer arguments must be valid for the lengths given, as declared in
/// `php-extension/wprc.h`.
#[no_mangle]
pub unsafe extern "C" fn wprc_group(
    ns: *const c_char,
    ns_len: usize,
    group: *const c_char,
    group_len: usize,
) -> i64 {
    // SAFETY: valid (pointer, length) pairs from C.
    let (ns, group) = unsafe { (slice(ns, ns_len), slice(group, group_len)) };
    with(-1, |c, l| {
        let mut k = Vec::with_capacity(ns.len() + group.len() + 5);
        k.extend_from_slice(&(ns.len() as u32).to_le_bytes());
        k.extend_from_slice(ns);
        k.extend_from_slice(group);
        if let Some(&id) = l.ids.get(&k) {
            let r = &mut l.groups[id as usize];
            if r.epoch != l.epoch {
                // New segment since registration: record the names again.
                r.handle = c.group(ns, group);
                r.epoch = l.epoch;
            }
            return ((l.registry_gen as i64) << 32) | id as i64;
        }
        let id = l.groups.len() as u32;
        l.groups.push(Registered {
            handle: c.group(ns, group),
            epoch: l.epoch,
        });
        l.ids.insert(k, id);
        ((l.registry_gen as i64) << 32) | id as i64
    })
}

/// # Safety
/// Pointer arguments must be valid for the lengths given, as declared in
/// `php-extension/wprc.h`.
#[no_mangle]
pub unsafe extern "C" fn wprc_get(
    gid: u64,
    blog: u32,
    key: *const c_char,
    key_len: usize,
    out: *mut WprcValue,
) -> i32 {
    // SAFETY: valid (pointer, length) from C.
    let key = unsafe { slice(key, key_len) };
    with_group(-1, gid, |c, g, pool| {
        let mut buf = pool.pop().unwrap_or_default();
        match c.get(g, blog, key, &mut buf) {
            Ok(Some(tag)) => {
                let owner = Box::into_raw(buf);
                // SAFETY: `out` is a valid out-pointer; `owner` stays alive
                // until wprc_value_release() hands it back.
                unsafe {
                    *out = WprcValue {
                        ptr: (*owner).as_ptr() as *const c_char,
                        len: (*owner).len(),
                        tag,
                        owner,
                    };
                }
                1
            }
            Ok(None) => {
                pool.push(buf);
                0
            }
            Err(_) => {
                pool.push(buf);
                -1
            }
        }
    })
}

/// # Safety
/// Pointer arguments must be valid for the lengths given, as declared in
/// `php-extension/wprc.h`.
#[no_mangle]
pub unsafe extern "C" fn wprc_value_release(v: *mut WprcValue) {
    guard((), || {
        // SAFETY: `v` came from wprc_get(); owner is released exactly once.
        let owner = unsafe {
            let o = (*v).owner;
            (*v).owner = std::ptr::null_mut();
            o
        };
        if owner.is_null() {
            return;
        }
        // SAFETY: produced by Box::into_raw in wprc_get.
        let mut buf = unsafe { Box::from_raw(owner) };
        if buf.capacity() > 1 << 20 {
            return; // do not pin large buffers
        }
        buf.clear();
        let _ = LOCAL.try_with(|l| {
            if let Ok(mut l) = l.try_borrow_mut() {
                if l.pool.len() < 8 {
                    l.pool.push(buf);
                }
            }
        });
    })
}

/// # Safety
/// Pointer arguments must be valid for the lengths given, as declared in
/// `php-extension/wprc.h`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn wprc_set(
    gid: u64,
    blog: u32,
    key: *const c_char,
    key_len: usize,
    tag: u8,
    val: *const c_char,
    val_len: usize,
    ttl: u32,
    mode: i32,
) -> i32 {
    // SAFETY: valid (pointer, length) pairs from C.
    let (key, val) = unsafe { (slice(key, key_len), slice(val, val_len)) };
    let mode = match mode {
        1 => SetMode::Add,
        2 => SetMode::Replace,
        _ => SetMode::Set,
    };
    with_group(-1, gid, |c, g, _| {
        match c.set(g, blog, key, tag, val, ttl, mode) {
            Ok(SetOutcome::Stored) => 0,
            Ok(SetOutcome::Exists) => 1,
            Ok(SetOutcome::Missing) => 2,
            Ok(SetOutcome::TooLarge) => 3,
            Ok(SetOutcome::NoMemory) => 4,
            Ok(SetOutcome::Rejected) => 5,
            Err(_) => -1,
        }
    })
}

/// # Safety
/// Pointer arguments must be valid for the lengths given, as declared in
/// `php-extension/wprc.h`.
#[no_mangle]
pub unsafe extern "C" fn wprc_delete(
    gid: u64,
    blog: u32,
    key: *const c_char,
    key_len: usize,
) -> i32 {
    // SAFETY: valid (pointer, length) from C.
    let key = unsafe { slice(key, key_len) };
    with_group(-1, gid, |c, g, _| match c.delete(g, blog, key) {
        Ok(true) => 1,
        Ok(false) => 0,
        Err(_) => -1,
    })
}

/// # Safety
/// Pointer arguments must be valid for the lengths given, as declared in
/// `php-extension/wprc.h`.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn wprc_incr(
    gid: u64,
    blog: u32,
    key: *const c_char,
    key_len: usize,
    offset: i64,
    tag: *mut u8,
    lval: *mut i64,
    dval: *mut f64,
) -> i32 {
    // SAFETY: valid (pointer, length) from C; out-pointers are valid.
    let key = unsafe { slice(key, key_len) };
    with_group(-1, gid, |c, g, _| match c.incr(g, blog, key, offset) {
        Ok(Some(value::Number::Long(v))) => {
            // SAFETY: valid out-pointers.
            unsafe {
                *tag = value::TAG_LONG;
                *lval = v;
            }
            1
        }
        Ok(Some(value::Number::Double(v))) => {
            // SAFETY: valid out-pointers.
            unsafe {
                *tag = value::TAG_DOUBLE;
                *dval = v;
            }
            1
        }
        Ok(None) => 0,
        Err(_) => -1,
    })
}

#[no_mangle]
pub extern "C" fn wprc_flush_group(gid: u64) -> i32 {
    with_group(
        -1,
        gid,
        |c, g, _| if c.flush_group(g).is_ok() { 1 } else { -1 },
    )
}

/// # Safety
/// Pointer arguments must be valid for the lengths given, as declared in
/// `php-extension/wprc.h`.
#[no_mangle]
pub unsafe extern "C" fn wprc_flush_namespace(ns: *const c_char, ns_len: usize) -> i32 {
    // SAFETY: valid (pointer, length) from C.
    let ns = unsafe { slice(ns, ns_len) };
    with(
        -1,
        |c, _| if c.flush_namespace(ns).is_ok() { 1 } else { -1 },
    )
}

#[no_mangle]
pub extern "C" fn wprc_flush_all() -> i32 {
    with(-1, |c, _| if c.flush_all().is_ok() { 1 } else { -1 })
}

fn copy_cstr(dst: &mut [c_char], s: &str) {
    let n = s.len().min(dst.len() - 1);
    for (d, b) in dst.iter_mut().zip(s.as_bytes()[..n].iter()) {
        *d = *b as c_char;
    }
    dst[n] = 0;
}

/// # Safety
/// Pointer arguments must be valid for the lengths given, as declared in
/// `php-extension/wprc.h`.
#[no_mangle]
pub unsafe extern "C" fn wprc_read_stats(out: *mut WprcStats) -> i32 {
    with(-1, |c, _| {
        let s = c.stats();
        let q = |get: bool, p: f64| s.latency(get, p).unwrap_or(0);
        let mut w = WprcStats {
            hits: s.hits,
            misses: s.misses,
            sets: s.sets,
            deletes: s.deletes,
            evictions: s.evictions,
            expired: s.expired,
            stale: s.stale,
            rejected: s.rejected,
            too_large: s.too_large,
            no_memory: s.no_memory,
            resets: s.resets,
            contended: s.contended,
            recoveries: s.recoveries,
            entries: s.entries,
            payload_bytes: s.payload_bytes,
            alloc_bytes: s.alloc_bytes,
            heap_bytes: s.heap_bytes,
            total_size: s.total_size,
            max_item_size: s.max_item_size,
            created_at: s.created_at,
            attaches: s.attaches,
            groups_used: s.groups_used as u64,
            namespaces: s.namespaces as u64,
            group_slots: s.group_slots as u64,
            groups_overflow: s.groups_overflow,
            shards: s.shards as u64,
            get_p50: q(true, 0.50),
            get_p95: q(true, 0.95),
            get_p99: q(true, 0.99),
            set_p50: q(false, 0.50),
            set_p95: q(false, 0.95),
            set_p99: q(false, 0.99),
            get_samples: s.samples(true),
            set_samples: s.samples(false),
            policy: [0; 16],
            path: [0; 256],
        };
        copy_cstr(&mut w.policy, s.policy);
        copy_cstr(&mut w.path, &s.path);
        // SAFETY: valid out-pointer from C.
        unsafe { *out = w };
        1
    })
}

/// Writes, at most once per event, a line describing the shard resets this
/// process performed since the last call; returns its length (0 = nothing).
///
/// # Safety
/// `buf` must be writable for `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn wprc_recovery_notice(buf: *mut c_char, cap: usize) -> usize {
    guard(0, || {
        let Some((n, cause)) = wprc_core::take_recovery_notice() else {
            return 0;
        };
        let msg = format!(
            "wp-rust-cache: reset {n} cache shard(s) because {}; \
             the cache keeps working, the reset entries are reloaded on demand",
            cause.describe()
        );
        if buf.is_null() || cap == 0 {
            return 0;
        }
        let n = msg.len().min(cap - 1);
        // SAFETY: the caller guarantees `cap` writable bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(msg.as_ptr(), buf as *mut u8, n);
            *buf.add(n) = 0;
        }
        n
    })
}
