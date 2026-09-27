//! Public engine API: attach to a segment and operate on it.

use crate::config::{Config, Policy};
use crate::groups::{self, GroupHandle};
use crate::histogram;
use crate::layout::*;
use crate::mutex::{self, Locked};
use crate::segment::{self, Segment};
use crate::shard::{self, Corrupt, Ctx};
use crate::stats::{GroupUsage, Stats};
use crate::value::Number;
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::time::Instant;
use xxhash_rust::xxh3::xxh3_64;

pub use crate::shard::{SetMode, SetOutcome};
pub type IncrOutcome = Number;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Config(String),
    /// The segment file exists with unsafe ownership or permissions.
    Permissions(String),
    /// No usable segment (attach in `Existing` mode).
    NotFound(String),
    Layout(String),
    Lock(io::Error),
    /// Another process holds the segment file lock (creating or replacing
    /// the segment) for longer than an attach may wait.
    Busy(String),
    /// A shard failed a consistency check and was reset; the operation is
    /// reported as failed (the cache is empty there, which is correct).
    Corrupted,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Config(s) => write!(f, "configuration: {s}"),
            Error::Permissions(s) => write!(f, "permissions: {s}"),
            Error::NotFound(s) => write!(f, "{s}"),
            Error::Layout(s) => write!(f, "segment: {s}"),
            Error::Lock(e) => write!(f, "shard lock: {e}"),
            Error::Busy(s) => write!(f, "{s}"),
            Error::Corrupted => write!(f, "shard failed a consistency check and was reset"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachMode {
    /// Create the segment if missing; recreate it if its layout or sizing
    /// no longer matches the configuration. What PHP workers use.
    Create,
    /// Attach to whatever valid segment exists, never modify its layout.
    /// What the diagnostics CLI uses.
    Existing,
}

/// One key of a group, from `Cache::group_keys`.
#[derive(Debug, Clone)]
pub struct KeyUsage {
    /// The key as WordPress passed it.
    pub key: Vec<u8>,
    /// 0 for single sites and global groups.
    pub blog: u32,
    pub tag: u8,
    pub value_len: u32,
    /// Bytes of the whole entry block.
    pub alloc: u32,
    /// Unix seconds, 0 = never.
    pub expires: u32,
    /// False when a flush made it unreachable (awaiting reclamation).
    pub live: bool,
}

/// Result of `Cache::inspect`.
#[derive(Debug, Clone)]
pub struct EntryInfo {
    pub tag: u8,
    pub expires: u32,
    pub alloc: u32,
    pub shard: usize,
}

/// A mapped segment. Cheap to share between threads (`&Cache` is `Sync`).
pub struct Cache {
    seg: Segment,
    hdr: *const SegmentHeader,
    dir: *const DirSlot,
    dir_slots: u32,
    gens: *const AtomicU32,
    gen_count: u32,
    shards: *mut u8,
    shard_size: usize,
    shard_count: usize,
    tinylfu: bool,
    plan: shard::Plan,
}

// SAFETY: all shared state is behind atomics or the per-shard process-shared
// mutex; the raw pointers only address the mapping owned by `seg`.
unsafe impl Send for Cache {}
unsafe impl Sync for Cache {}

thread_local! {
    static SAMPLE: Cell<u32> = const { Cell::new(0) };
}

/// Why a shard was reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Recovery {
    /// The lock owner died holding it (kill -9, OOM kill, crash).
    OwnerDied = 1,
    /// An operation was found half-done.
    Interrupted = 2,
    /// A consistency check failed.
    Inconsistent = 3,
}

impl Recovery {
    pub fn from_u32(v: u32) -> Option<Recovery> {
        match v {
            1 => Some(Recovery::OwnerDied),
            2 => Some(Recovery::Interrupted),
            3 => Some(Recovery::Inconsistent),
            _ => None,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Recovery::OwnerDied => {
                "a process died holding the shard lock (kill -9, OOM kill or crash)"
            }
            Recovery::Interrupted => "an operation was left half-done",
            Recovery::Inconsistent => "a consistency check failed",
        }
    }
}

/// Resets performed by this process, not yet reported (see
/// `take_recovery_notice`): the PHP extension logs them.
static PROCESS_RECOVERIES: AtomicU64 = AtomicU64::new(0);
static PROCESS_LAST_CAUSE: AtomicU32 = AtomicU32::new(0);

/// Resets this process performed since the last call, with the last cause.
pub fn take_recovery_notice() -> Option<(u64, Recovery)> {
    let n = PROCESS_RECOVERIES.swap(0, Relaxed);
    if n == 0 {
        return None;
    }
    Recovery::from_u32(PROCESS_LAST_CAUSE.load(Relaxed)).map(|c| (n, c))
}

/// One operation in 64 is timed, so the fast path does not pay for a clock
/// read and the histograms still converge quickly.
#[inline]
fn sampled() -> bool {
    SAMPLE.with(|c| {
        let v = c.get().wrapping_add(1);
        c.set(v);
        v & 63 == 0
    })
}

struct Offsets {
    dir: u64,
    gens: u64,
    shards: u64,
    total: u64,
}

fn segment_layout(cfg: &Config) -> Offsets {
    let page = |v: u64| (v + 4095) & !4095;
    let dir = HEADER_SIZE as u64;
    let gens = dir + page(cfg.group_slots as u64 * DIR_SLOT_SIZE as u64);
    let shards = gens + page(GEN_COUNT as u64 * 4);
    let total = shards + cfg.shard_size() * cfg.shards as u64;
    Offsets {
        dir,
        gens,
        shards,
        total,
    }
}

/// Releases the shard lock even if the operation panics. A panic leaves
/// `dirty` set, so the next holder resets the shard.
struct Guard(*mut libc::pthread_mutex_t);

impl Drop for Guard {
    fn drop(&mut self) {
        // SAFETY: constructed only after a successful lock by this thread.
        unsafe { mutex::unlock(self.0) }
    }
}

impl Cache {
    /// Attaches to (and if needed creates) the segment named by `cfg`.
    pub fn attach(cfg: &Config, mode: AttachMode) -> Result<Cache, Error> {
        Self::attach_path(cfg, &cfg.path(), mode)
    }

    pub fn attach_path(cfg: &Config, path: &Path, mode: AttachMode) -> Result<Cache, Error> {
        cfg.validate().map_err(Error::Config)?;
        // SAFETY: geteuid has no preconditions.
        let root = unsafe { libc::geteuid() } == 0;
        // A segment created by root would be unreadable for PHP-FPM workers,
        // so root creates only when told whom the segment belongs to.
        let may_create = mode == AttachMode::Create && (!root || cfg.owner.is_some());
        for _ in 0..4 {
            let opened = match segment::open_file(path, cfg.permissions, may_create) {
                Ok(o) => o,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let why = if mode == AttachMode::Create {
                        " (running as root: set [shared_memory] owner, or let PHP-FPM create it)"
                    } else {
                        ""
                    };
                    return Err(Error::NotFound(format!(
                        "no segment at {}{why}",
                        path.display()
                    )));
                }
                Err(e) => return Err(Error::Io(e)),
            };
            let fd = opened.fd;
            let result = (|| {
                let st = segment::fstat(fd)?;
                if st.st_mode & libc::S_IFMT != libc::S_IFREG {
                    return Err(Error::Permissions(format!(
                        "{} is not a regular file",
                        path.display()
                    )));
                }
                if st.st_mode & 0o007 != 0 {
                    return Err(Error::Permissions(format!(
                        "{} is accessible to other users (mode {:o}); refusing to use it",
                        path.display(),
                        st.st_mode & 0o777
                    )));
                }
                if !opened.created {
                    Self::check_owner(&st, cfg, root, path)?;
                }
                if opened.created {
                    let mut uid = u32::MAX;
                    let mut gid = u32::MAX;
                    if let (true, Some(o)) = (root, &cfg.owner) {
                        let ids = segment::user_ids(o)
                            .ok_or_else(|| Error::Config(format!("unknown user {o:?}")))?;
                        (uid, gid) = ids;
                    }
                    if let Some(g) = &cfg.group {
                        gid = segment::group_id(g)
                            .ok_or_else(|| Error::Config(format!("unknown group {g:?}")))?;
                    }
                    if uid != u32::MAX || gid != u32::MAX {
                        segment::fchown(fd, uid, gid)?;
                    }
                }
                if !segment::lock_exclusive(fd, std::time::Duration::from_secs(1))? {
                    return Err(Error::Busy(format!(
                        "{} is being set up by another process",
                        path.display()
                    )));
                }
                let r = Self::attach_locked(fd, cfg, path, mode);
                let _ = segment::flock(fd, libc::LOCK_UN);
                r
            })();
            match result {
                Ok(Some(mut cache)) => {
                    cache.seg.fd = fd; // the segment owns the descriptor from now on
                    return Ok(cache);
                }
                Ok(None) => segment::close(fd), // retired or unlinked: retry
                Err(e) => {
                    segment::close(fd);
                    return Err(e);
                }
            }
        }
        Err(Error::Layout(
            "segment keeps being replaced; giving up".into(),
        ))
    }

    /// Whose segment may we trust? Its contents end up in `unserialize()`,
    /// so a file planted in the world-writable `/dev/shm` by another local
    /// user would be a code-injection vector. Trusted owners: ourselves; the
    /// configured `owner` (root attaching to the PHP-FPM user's segment, or
    /// a pool sharing it through the configured `group`); root.
    fn check_owner(st: &libc::stat, cfg: &Config, root: bool, path: &Path) -> Result<(), Error> {
        // SAFETY: geteuid has no preconditions.
        let euid = unsafe { libc::geteuid() };
        let owner_uid = cfg
            .owner
            .as_deref()
            .and_then(segment::user_ids)
            .map(|(u, _)| u);
        let trusted = st.st_uid == euid
            || st.st_uid == 0
            || (root && (owner_uid.is_none() || owner_uid == Some(st.st_uid)))
            || (owner_uid == Some(st.st_uid)
                && cfg.group.as_deref().and_then(segment::group_id) == Some(st.st_gid));
        if trusted {
            Ok(())
        } else {
            Err(Error::Permissions(format!(
                "{} belongs to uid {} and this process runs as uid {euid}; \
                 refusing a segment another user could have written \
                 (set [shared_memory] owner and group to share one on purpose)",
                path.display(),
                st.st_uid
            )))
        }
    }

    /// Runs with the file lock held. `Ok(None)` means "reopen and retry".
    fn attach_locked(
        fd: i32,
        cfg: &Config,
        path: &Path,
        mode: AttachMode,
    ) -> Result<Option<Cache>, Error> {
        let st = segment::fstat(fd)?;
        if st.st_nlink == 0 {
            return Ok(None);
        }
        let size = st.st_size as u64;
        if size >= HEADER_SIZE as u64 {
            let mut seg = Segment::map(fd, size as usize, path.to_path_buf())?;
            seg.fd = -1; // not owned until attach succeeds
                         // SAFETY: the mapping is at least HEADER_SIZE bytes.
            let hdr = unsafe { &*(seg.base as *const SegmentHeader) };
            match Self::validate(hdr, size, cfg, mode) {
                Ok(()) => {
                    hdr.attaches.fetch_add(1, Relaxed);
                    return Ok(Some(Self::from_segment(seg)));
                }
                Err(reason) => {
                    if mode == AttachMode::Existing {
                        return Err(Error::Layout(reason));
                    }
                    // Tell processes still using it to re-attach, then
                    // replace it. Unlinked memory lives on until the last
                    // process unmaps it, so nobody faults.
                    if hdr.magic == MAGIC {
                        hdr.retired.store(1, Release);
                    }
                    drop(seg);
                    segment::unlink(path)?;
                    return Ok(None);
                }
            }
        }
        if mode == AttachMode::Existing {
            return Err(Error::NotFound(format!(
                "{} is not initialised",
                path.display()
            )));
        }
        let total = segment_layout(cfg).total;
        // Truncating to zero first guarantees the new segment is all zeros.
        segment::size_file(fd, 0, false)?;
        // When a segment has just been replaced, the processes queued on the
        // old file's lock still hold it open, so its pages are not free yet
        // and tmpfs can briefly lack room for both. They let go within
        // milliseconds of acquiring that lock: retry briefly. If the old
        // segment is still mapped by live processes (old workers finishing
        // their requests), give up; the next attempt comes seconds later.
        let mut reserved = segment::size_file(fd, total, cfg.preallocate);
        for _ in 0..10 {
            match &reserved {
                Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
                    let _ = segment::size_file(fd, 0, false);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    reserved = segment::size_file(fd, total, cfg.preallocate);
                }
                _ => break,
            }
        }
        reserved.map_err(|e| {
            // Do not leave a large sparse file behind in tmpfs.
            let _ = segment::size_file(fd, 0, false);
            Error::Io(io::Error::new(
                e.kind(),
                format!(
                    "cannot reserve {} for {}: {e} (is /dev/shm large enough?)",
                    crate::config::format_size(total),
                    path.display()
                ),
            ))
        })?;
        let mut seg = Segment::map(fd, total as usize, path.to_path_buf())?;
        seg.fd = -1;
        // SAFETY: fresh zeroed mapping of `total` bytes, exclusively ours
        // while we hold the file lock and state is not READY.
        unsafe { Self::format(seg.base, cfg)? };
        Ok(Some(Self::from_segment(seg)))
    }

    fn validate(
        hdr: &SegmentHeader,
        size: u64,
        cfg: &Config,
        mode: AttachMode,
    ) -> Result<(), String> {
        if hdr.magic != MAGIC {
            return Err("bad magic".into());
        }
        if hdr.layout_version != LAYOUT_VERSION {
            return Err(format!(
                "layout version {} (this build uses {LAYOUT_VERSION})",
                hdr.layout_version
            ));
        }
        if hdr.state.load(Acquire) != STATE_READY {
            return Err("initialisation never completed".into());
        }
        if hdr.retired.load(Acquire) != 0 {
            return Err("retired".into());
        }
        if hdr.checksum != hdr.compute_checksum() || hdr.total_size != size {
            return Err("header checksum mismatch".into());
        }
        if mode == AttachMode::Create {
            let total = segment_layout(cfg).total;
            if hdr.total_size != total
                || hdr.shard_count != cfg.shards
                || hdr.dir_slots != cfg.group_slots
                || hdr.gen_count != GEN_COUNT
                || hdr.policy != cfg.policy as u32
                || hdr.max_item_size != cfg.max_item_size
                || hdr.soft_limit_pct != cfg.soft_limit_pct
            {
                return Err("configuration changed".into());
            }
        }
        Ok(())
    }

    /// Writes a fresh layout.
    ///
    /// # Safety
    /// `base` must be a zeroed, writable mapping of the size given by
    /// `segment_layout(cfg)`, not yet visible as READY to anyone.
    unsafe fn format(base: *mut u8, cfg: &Config) -> Result<(), Error> {
        let off = segment_layout(cfg);
        let h = base as *mut SegmentHeader;
        (*h).magic = MAGIC;
        (*h).layout_version = LAYOUT_VERSION;
        (*h).shard_count = cfg.shards;
        (*h).dir_slots = cfg.group_slots;
        (*h).gen_count = GEN_COUNT;
        (*h).policy = cfg.policy as u32;
        (*h).total_size = off.total;
        (*h).shard_size = cfg.shard_size();
        (*h).dir_offset = off.dir;
        (*h).gens_offset = off.gens;
        (*h).shards_offset = off.shards;
        (*h).max_item_size = cfg.max_item_size;
        (*h).soft_limit_pct = cfg.soft_limit_pct;
        (*h).creator_pid = std::process::id();
        (*h).created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let shard_size = cfg.shard_size() as usize;
        for i in 0..cfg.shards as usize {
            let sh = base.add(off.shards as usize + i * shard_size) as *mut ShardHeader;
            mutex::init((*sh).mutex.get())?;
            let st = &mut *(*sh).state.get();
            let plan = shard::plan(
                shard_size as u32,
                cfg.policy == Policy::TinyLfu,
                cfg.max_item_size,
                cfg.soft_limit_pct,
            );
            let mut ctx = Ctx {
                base: sh as *mut u8,
                stats: &(*sh).stats,
                st,
                plan,
                tinylfu: cfg.policy == Policy::TinyLfu,
            };
            ctx.reset();
        }
        (*h).checksum = (*h).compute_checksum();
        (*h).state.store(STATE_READY, Release);
        Ok(())
    }

    /// A private cache in an anonymous shared mapping: shared with children
    /// forked afterwards, invisible to anyone else. For tests and benchmarks.
    pub fn anonymous(cfg: &Config) -> Result<Cache, Error> {
        cfg.validate().map_err(Error::Config)?;
        let total = segment_layout(cfg).total;
        let seg = Segment::anonymous(total as usize)?;
        // SAFETY: anonymous mappings are zero-filled and private to us.
        unsafe { Self::format(seg.base, cfg)? };
        Ok(Self::from_segment(seg))
    }

    fn from_segment(seg: Segment) -> Cache {
        // SAFETY: header validated or freshly formatted by the caller.
        let h = unsafe { &*(seg.base as *const SegmentHeader) };
        Cache {
            hdr: h,
            // SAFETY: offsets come from a validated header.
            dir: unsafe { seg.base.add(h.dir_offset as usize) } as *const DirSlot,
            dir_slots: h.dir_slots,
            gens: unsafe { seg.base.add(h.gens_offset as usize) } as *const AtomicU32,
            gen_count: h.gen_count,
            shards: unsafe { seg.base.add(h.shards_offset as usize) },
            shard_size: h.shard_size as usize,
            shard_count: h.shard_count as usize,
            tinylfu: h.policy == Policy::TinyLfu as u32,
            plan: shard::plan(
                h.shard_size as u32,
                h.policy == Policy::TinyLfu as u32,
                h.max_item_size,
                h.soft_limit_pct,
            ),
            seg,
        }
    }

    #[inline]
    fn header(&self) -> &SegmentHeader {
        // SAFETY: valid for the lifetime of the mapping.
        unsafe { &*self.hdr }
    }

    #[inline]
    fn dir(&self) -> &[DirSlot] {
        // SAFETY: the directory has `dir_slots` entries inside the mapping.
        unsafe { std::slice::from_raw_parts(self.dir, self.dir_slots as usize) }
    }

    #[inline]
    fn gen(&self, i: u32) -> &AtomicU32 {
        // SAFETY: indices are reduced modulo gen_count (checked below for
        // indices read back from stored keys).
        unsafe { &*self.gens.add(i as usize) }
    }

    pub fn path(&self) -> Option<&PathBuf> {
        self.seg.path.as_ref()
    }

    /// True when this mapping should be dropped and re-attached: another
    /// process retired the segment, or the file was removed.
    pub fn is_stale(&self) -> bool {
        self.header().retired.load(Acquire) != 0 || self.seg.is_unlinked()
    }

    /// Marks the segment retired and removes the file; every process
    /// re-attaches (to a new, empty segment) at its next check.
    pub fn retire(&self) -> Result<(), Error> {
        self.header().retired.store(1, Release);
        if let Some(p) = &self.seg.path {
            segment::unlink(p)?;
        }
        Ok(())
    }

    // ---- groups ------------------------------------------------------------

    /// Handle for `group` inside namespace `ns` (one WordPress install).
    /// Nothing is allocated in the segment for it: any number of groups
    /// works. The names are recorded in the diagnostic directory if there
    /// is room.
    pub fn group(&self, ns: &[u8], group: &[u8]) -> GroupHandle {
        let h = groups::handle(ns, group, self.gen_count);
        let dir = self.dir();
        let nsid = groups::namespace_id(ns);
        let ok = groups::register(dir, nsid, None, ns)
            & groups::register(dir, u128::from_le_bytes(h.id), Some(nsid), group);
        if !ok {
            self.header().dir_overflow.fetch_add(1, Relaxed);
        }
        h
    }

    #[inline]
    fn prefix(&self, g: &GroupHandle, blog: u32) -> [u8; KEY_PREFIX] {
        let mut p = [0u8; KEY_PREFIX];
        p[0..4].copy_from_slice(&g.ns_gen.to_le_bytes());
        p[4..8].copy_from_slice(&self.gen(g.ns_gen).load(Acquire).to_le_bytes());
        p[8..12].copy_from_slice(&g.group_gen.to_le_bytes());
        p[12..16].copy_from_slice(&self.gen(g.group_gen).load(Acquire).to_le_bytes());
        p[16..20].copy_from_slice(&blog.to_le_bytes());
        p[20..36].copy_from_slice(&g.id);
        p
    }

    /// Whether a stored composite key still belongs to the current
    /// generations of its namespace and group.
    fn is_live(&self, k: &[u8]) -> bool {
        if k.len() < KEY_PREFIX {
            return true;
        }
        let u = |i: usize| u32::from_le_bytes(k[i..i + 4].try_into().unwrap());
        if u(0) >= self.gen_count || u(8) >= self.gen_count {
            return false;
        }
        self.gen(u(0)).load(Acquire) == u(4) && self.gen(u(8)).load(Acquire) == u(12)
    }

    #[inline]
    fn with_key<T>(
        &self,
        g: &GroupHandle,
        blog: u32,
        key: &[u8],
        f: impl FnOnce(&[u8], u64) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let p = self.prefix(g, blog);
        let total = KEY_PREFIX + key.len();
        if total <= 256 {
            let mut buf = [0u8; 256];
            buf[..KEY_PREFIX].copy_from_slice(&p);
            buf[KEY_PREFIX..total].copy_from_slice(key);
            let k = &buf[..total];
            f(k, xxh3_64(k))
        } else {
            let mut v = Vec::with_capacity(total);
            v.extend_from_slice(&p);
            v.extend_from_slice(key);
            f(&v, xxh3_64(&v))
        }
    }

    // ---- shards -------------------------------------------------------------

    #[inline]
    fn shard_of(&self, hash: u64) -> usize {
        // Bucket selection uses the low 32 bits; shards use the top bits.
        ((hash >> 48) as usize) & (self.shard_count - 1)
    }

    #[inline]
    fn shard_ptr(&self, i: usize) -> *mut ShardHeader {
        // SAFETY: i < shard_count, so the shard lies inside the mapping.
        unsafe { self.shards.add(i * self.shard_size) as *mut ShardHeader }
    }

    fn with_shard<T>(
        &self,
        i: usize,
        f: impl FnOnce(&mut Ctx<'_>) -> Result<T, Corrupt>,
    ) -> Result<T, Error> {
        let sh = self.shard_ptr(i);
        // SAFETY: `sh` points at an initialised shard header; the mutable
        // state is only dereferenced while we hold its lock.
        unsafe {
            let stats = &(*sh).stats;
            let m = (*sh).mutex.get();
            let locked = match mutex::lock(m, &stats.contended) {
                Ok(l) => l,
                Err(e) => {
                    if e.raw_os_error() == Some(libc::ENOTRECOVERABLE) {
                        // Nothing in this segment can be trusted any more.
                        let _ = self.retire();
                    }
                    return Err(Error::Lock(e));
                }
            };
            let _guard = Guard(m);
            let mut ctx = Ctx {
                base: sh as *mut u8,
                stats,
                st: &mut *(*sh).state.get(),
                plan: self.plan,
                tinylfu: self.tinylfu,
            };
            if matches!(locked, Locked::OwnerDied) {
                // The previous holder died holding the lock.
                self.recover(&mut ctx, Recovery::OwnerDied);
            } else if ctx.st.dirty != 0 {
                // An operation was left half-done.
                self.recover(&mut ctx, Recovery::Interrupted);
            }
            ctx.st.dirty = 1;
            match f(&mut ctx) {
                Ok(v) => {
                    ctx.st.dirty = 0;
                    Ok(v)
                }
                Err(Corrupt) => {
                    self.recover(&mut ctx, Recovery::Inconsistent);
                    Err(Error::Corrupted)
                }
            }
        }
    }

    fn recover(&self, ctx: &mut Ctx<'_>, cause: Recovery) {
        ctx.reset();
        let r = &ctx.stats.resets;
        r.store(r.load(Relaxed) + 1, Relaxed);
        let h = self.header();
        h.recoveries.fetch_add(1, Relaxed);
        match cause {
            Recovery::OwnerDied => &h.recover_owner_died,
            Recovery::Interrupted => &h.recover_interrupted,
            Recovery::Inconsistent => &h.recover_inconsistent,
        }
        .fetch_add(1, Relaxed);
        h.last_recovery_at.store(crate::now_secs() as u64, Relaxed);
        h.last_recovery_cause.store(cause as u32, Relaxed);
        PROCESS_RECOVERIES.fetch_add(1, Relaxed);
        PROCESS_LAST_CAUSE.store(cause as u32, Relaxed);
    }

    #[inline]
    fn record(&self, shard: usize, get: bool, t0: Option<Instant>) {
        if let Some(t0) = t0 {
            let ns = t0.elapsed().as_nanos() as u64;
            // SAFETY: shard index is valid; histogram cells are atomics.
            let stats = unsafe { &(*self.shard_ptr(shard)).stats };
            let h = if get { &stats.lat_get } else { &stats.lat_set };
            h[histogram::bucket(ns)].fetch_add(1, Relaxed);
        }
    }

    // ---- data operations ----------------------------------------------------

    /// Looks up a key; on a hit the value is copied into `out` and its tag
    /// returned.
    pub fn get(
        &self,
        g: &GroupHandle,
        blog: u32,
        key: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<Option<u8>, Error> {
        let t0 = sampled().then(Instant::now);
        self.with_key(g, blog, key, |k, h| {
            let s = self.shard_of(h);
            let now = crate::now_secs();
            let r = self.with_shard(s, |c| c.get(h, k, now, out, true));
            self.record(s, true, t0);
            Ok(r?.map(|f| f.tag))
        })
    }

    /// Reads without touching LRU order, statistics or expiry.
    pub fn inspect(
        &self,
        g: &GroupHandle,
        blog: u32,
        key: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<Option<EntryInfo>, Error> {
        self.with_key(g, blog, key, |k, h| {
            let s = self.shard_of(h);
            let now = crate::now_secs();
            let r = self.with_shard(s, |c| c.get(h, k, now, out, false))?;
            Ok(r.map(|f| EntryInfo {
                tag: f.tag,
                expires: f.expires,
                alloc: f.alloc,
                shard: s,
            }))
        })
    }

    /// Stores a value. `ttl` in seconds, 0 = no expiry.
    #[allow(clippy::too_many_arguments)]
    pub fn set(
        &self,
        g: &GroupHandle,
        blog: u32,
        key: &[u8],
        tag: u8,
        value: &[u8],
        ttl: u32,
        mode: SetMode,
    ) -> Result<SetOutcome, Error> {
        let t0 = sampled().then(Instant::now);
        self.with_key(g, blog, key, |k, h| {
            let s = self.shard_of(h);
            let now = crate::now_secs();
            let expires = if ttl == 0 { 0 } else { now.saturating_add(ttl) };
            let live = |k: &[u8]| self.is_live(k);
            let r = self.with_shard(s, |c| c.set(h, k, tag, value, expires, mode, now, &live));
            self.record(s, false, t0);
            r
        })
    }

    pub fn delete(&self, g: &GroupHandle, blog: u32, key: &[u8]) -> Result<bool, Error> {
        self.with_key(g, blog, key, |k, h| {
            let now = crate::now_secs();
            self.with_shard(self.shard_of(h), |c| c.delete(h, k, now))
        })
    }

    /// Atomic increment; `None` when the key does not exist.
    pub fn incr(
        &self,
        g: &GroupHandle,
        blog: u32,
        key: &[u8],
        offset: i64,
    ) -> Result<Option<Number>, Error> {
        self.with_key(g, blog, key, |k, h| {
            let now = crate::now_secs();
            self.with_shard(self.shard_of(h), |c| c.incr(h, k, offset, now))
        })
    }

    /// Makes every key of the group unreachable, in O(1). Groups sharing the
    /// same generation counter are flushed too (extra misses, never stale).
    pub fn flush_group(&self, g: &GroupHandle) -> Result<(), Error> {
        self.gen(g.group_gen).fetch_add(1, AcqRel);
        Ok(())
    }

    /// Makes every key of the namespace (all groups, all blogs) unreachable.
    pub fn flush_namespace(&self, ns: &[u8]) -> Result<(), Error> {
        let i = groups::gen_index(groups::namespace_id(ns), self.gen_count);
        self.gen(i).fetch_add(1, AcqRel);
        Ok(())
    }

    /// Empties the whole segment, every namespace. Generations are bumped
    /// first so that a write racing with the flush cannot survive it.
    pub fn flush_all(&self) -> Result<(), Error> {
        for i in 0..self.gen_count {
            self.gen(i).fetch_add(1, AcqRel);
        }
        for i in 0..self.shard_count {
            self.with_shard(i, |c| {
                c.reset();
                Ok(())
            })?;
        }
        Ok(())
    }

    // ---- diagnostics --------------------------------------------------------

    pub fn stats(&self) -> Stats {
        let h = self.header();
        let mut s = Stats {
            path: self
                .seg
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(anonymous)".into()),
            policy: Policy::from_u32(h.policy).name(),
            shards: h.shard_count,
            total_size: h.total_size,
            max_item_size: h.max_item_size,
            created_at: h.created_at,
            attaches: h.attaches.load(Relaxed),
            recoveries: h.recoveries.load(Relaxed),
            recover_owner_died: h.recover_owner_died.load(Relaxed),
            recover_interrupted: h.recover_interrupted.load(Relaxed),
            recover_inconsistent: h.recover_inconsistent.load(Relaxed),
            last_recovery_at: h.last_recovery_at.load(Relaxed),
            last_recovery_cause: Recovery::from_u32(h.last_recovery_cause.load(Relaxed)),
            group_slots: h.dir_slots,
            groups_overflow: h.dir_overflow.load(Relaxed),
            lat_get: vec![0; HIST_BUCKETS],
            lat_set: vec![0; HIST_BUCKETS],
            ..Default::default()
        };
        for e in groups::entries(self.dir()) {
            if e.parent_lo == 0 {
                s.namespaces += 1;
            } else {
                s.groups_used += 1;
            }
        }
        for i in 0..self.shard_count {
            // SAFETY: shard header is initialised; we read its atomics only.
            let st = unsafe { &(*self.shard_ptr(i)).stats };
            let heap = (self.plan.heap_end - self.plan.heap_offset) as u64;
            s.heap_bytes += heap;
            s.hits += st.hits.load(Relaxed);
            s.misses += st.misses.load(Relaxed);
            s.sets += st.sets.load(Relaxed);
            s.deletes += st.deletes.load(Relaxed);
            s.evictions += st.evictions.load(Relaxed);
            s.expired += st.expired.load(Relaxed);
            s.stale += st.stale.load(Relaxed);
            s.rejected += st.rejected.load(Relaxed);
            s.too_large += st.too_large.load(Relaxed);
            s.no_memory += st.no_memory.load(Relaxed);
            s.resets += st.resets.load(Relaxed);
            s.contended += st.contended.load(Relaxed);
            s.entries += st.entries.load(Relaxed);
            s.payload_bytes += st.payload_bytes.load(Relaxed);
            s.alloc_bytes += st.alloc_bytes.load(Relaxed);
            for b in 0..HIST_BUCKETS {
                s.lat_get[b] += st.lat_get[b].load(Relaxed);
                s.lat_set[b] += st.lat_set[b].load(Relaxed);
            }
        }
        s
    }

    /// Per-group entries and bytes, computed by walking every shard (one
    /// shard locked at a time). Costs nothing on the fast path. Names come
    /// from the diagnostic directory; a group it has no room for is shown
    /// by its identity hash.
    pub fn group_usage(&self) -> Result<Vec<GroupUsage>, Error> {
        let entries = groups::entries(self.dir());
        let names: HashMap<u64, (u64, String)> = entries
            .iter()
            .map(|e| (e.lo, (e.parent_lo, e.name.clone())))
            .collect();
        let mut map: HashMap<[u8; 16], GroupUsage> = HashMap::new();
        for i in 0..self.shard_count {
            self.with_shard(i, |c| {
                c.walk(|w| {
                    if w.key.len() < KEY_PREFIX {
                        return;
                    }
                    let id: [u8; 16] = w.key[20..36].try_into().unwrap();
                    let live = self.is_live(w.key);
                    let u = map.entry(id).or_insert_with(|| {
                        let lo = groups::lo_of(u128::from_le_bytes(id));
                        let (namespace, name) = match names.get(&lo) {
                            Some((parent, name)) => (
                                names
                                    .get(parent)
                                    .map(|(_, n)| n.clone())
                                    .unwrap_or_default(),
                                name.clone(),
                            ),
                            None => (String::new(), format!("#{lo:016x}")),
                        };
                        GroupUsage {
                            namespace,
                            name,
                            entries: 0,
                            bytes: 0,
                            stale_entries: 0,
                            stale_bytes: 0,
                        }
                    });
                    if live {
                        u.entries += 1;
                        u.bytes += w.alloc as u64;
                    } else {
                        u.stale_entries += 1;
                        u.stale_bytes += w.alloc as u64;
                    }
                })
            })?;
        }
        let mut v: Vec<GroupUsage> = map.into_values().collect();
        v.sort_by_key(|u| std::cmp::Reverse(u.bytes));
        Ok(v)
    }

    /// The keys stored for `group` of namespace `ns`, largest first: what
    /// `wp-rust-cache stats --keys` prints. Walks every shard, one locked at
    /// a time; nothing on the fast path.
    pub fn group_keys(&self, ns: &[u8], group: &[u8]) -> Result<Vec<KeyUsage>, Error> {
        let id = groups::group_id(ns, group).to_le_bytes();
        let mut out = Vec::new();
        for i in 0..self.shard_count {
            self.with_shard(i, |c| {
                c.walk(|w| {
                    if w.key.len() >= KEY_PREFIX && w.key[20..36] == id {
                        out.push(KeyUsage {
                            key: w.key[KEY_PREFIX..].to_vec(),
                            blog: u32::from_le_bytes(w.key[16..20].try_into().unwrap()),
                            tag: w.tag,
                            value_len: w.value_len,
                            alloc: w.alloc,
                            expires: w.expires,
                            live: self.is_live(w.key),
                        });
                    }
                })
            })?;
        }
        out.sort_by_key(|k| std::cmp::Reverse(k.alloc));
        Ok(out)
    }

    /// Like `group_keys`, for every namespace that has a group called
    /// `group`, found through the name directory (so the namespace name,
    /// often a long salt, is not needed). Returns (namespace, key) pairs,
    /// largest first. Groups the directory had no room for are not found.
    pub fn find_group_keys(&self, group: &[u8]) -> Result<Vec<(String, KeyUsage)>, Error> {
        let entries = groups::entries(self.dir());
        let ns_names: HashMap<u64, String> = entries
            .iter()
            .filter(|e| e.parent_lo == 0)
            .map(|e| (e.lo, e.name.clone()))
            .collect();
        let wanted = String::from_utf8_lossy(group);
        let targets: HashMap<u64, String> = entries
            .iter()
            .filter(|e| e.parent_lo != 0 && e.name == wanted)
            .map(|e| {
                (
                    e.lo,
                    ns_names.get(&e.parent_lo).cloned().unwrap_or_default(),
                )
            })
            .collect();
        let mut out = Vec::new();
        if targets.is_empty() {
            return Ok(out);
        }
        for i in 0..self.shard_count {
            self.with_shard(i, |c| {
                c.walk(|w| {
                    if w.key.len() < KEY_PREFIX {
                        return;
                    }
                    let id = u128::from_le_bytes(w.key[20..36].try_into().unwrap());
                    if let Some(ns) = targets.get(&groups::lo_of(id)) {
                        out.push((
                            ns.clone(),
                            KeyUsage {
                                key: w.key[KEY_PREFIX..].to_vec(),
                                blog: u32::from_le_bytes(w.key[16..20].try_into().unwrap()),
                                tag: w.tag,
                                value_len: w.value_len,
                                alloc: w.alloc,
                                expires: w.expires,
                                live: self.is_live(w.key),
                            },
                        ));
                    }
                })
            })?;
        }
        out.sort_by_key(|(_, k)| std::cmp::Reverse(k.alloc));
        Ok(out)
    }

    /// Structural check of every shard. Returns `(shard, problem)` pairs; with
    /// `repair`, broken shards are reset.
    pub fn verify(&self, repair: bool) -> Result<Vec<(usize, String)>, Error> {
        let mut problems = Vec::new();
        for i in 0..self.shard_count {
            let r = self.with_shard(i, |c| {
                let r = c.verify();
                if r.is_err() && repair {
                    self.recover(c, Recovery::Inconsistent);
                }
                Ok(r)
            })?;
            if let Err(p) = r {
                problems.push((i, p));
            }
        }
        Ok(problems)
    }

    /// Namespaces recorded in the diagnostic directory.
    pub fn namespaces(&self) -> Vec<String> {
        groups::entries(self.dir())
            .into_iter()
            .filter(|e| e.parent_lo == 0)
            .map(|e| e.name)
            .collect()
    }

    /// Writes `bytes` at `offset` inside a shard, past its mutex and
    /// counters: buckets, sketch, heap, or (with `state == true`) the LRU and
    /// allocator state. Simulates memory corruption.
    #[cfg(test)]
    pub(crate) fn test_scribble(&self, shard: usize, state: bool, offset: usize, bytes: &[u8]) {
        let sh = self.shard_ptr(shard);
        // SAFETY: test helper; writes stay inside the chosen region.
        unsafe {
            if state {
                let st = (*sh).state.get() as *mut u8;
                let len = std::mem::size_of::<ShardState>();
                let off = offset % len;
                let n = bytes.len().min(len - off);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), st.add(off), n);
            } else {
                let base = sh as *mut u8;
                let span = self.shard_size - SHARD_HEADER_SIZE;
                let off = SHARD_HEADER_SIZE + offset % span;
                let n = bytes.len().min(self.shard_size - off);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(off), n);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn test_mark_dirty(&self, shard: usize) {
        // SAFETY: test helper, single-threaded.
        unsafe { (*(*self.shard_ptr(shard)).state.get()).dirty = 1 }
    }

    pub fn shard_count(&self) -> usize {
        self.shard_count
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod randomized;
