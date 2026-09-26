//! The on-segment layout. Everything here is `#[repr(C)]`, fixed-size and
//! pointer-free: processes map the segment at different addresses, so every
//! reference is an offset. Changing any of these structures requires bumping
//! [`LAYOUT_VERSION`]; a process that finds another version retires the
//! segment and creates a fresh one.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64};
use xxhash_rust::xxh3::xxh3_64;

pub const MAGIC: u64 = u64::from_le_bytes(*b"WPRCACHE");
pub const LAYOUT_VERSION: u32 = 3;
pub const HEADER_SIZE: usize = 4096;

pub const STATE_INIT: u32 = 0;
pub const STATE_READY: u32 = 1;

/// Flush generations: a fixed table of counters indexed by hash. A group's
/// counter may be shared with other groups; flushing one then also
/// invalidates the others, which is always safe for a cache.
pub const GEN_COUNT: u32 = 1 << 16;

/// Segment header, at offset 0.
#[repr(C)]
pub struct SegmentHeader {
    pub magic: u64,
    pub layout_version: u32,
    pub state: AtomicU32,
    pub retired: AtomicU32,
    pub shard_count: u32,
    pub dir_slots: u32,
    pub policy: u32,
    pub gen_count: u32,
    pub soft_limit_pct: u32,
    pub total_size: u64,
    pub shard_size: u64,
    pub dir_offset: u64,
    pub gens_offset: u64,
    pub shards_offset: u64,
    pub max_item_size: u64,
    pub created_at: u64,
    pub creator_pid: u32,
    pub _pad: u32,
    pub checksum: u64,
    /// Processes that attached since creation.
    pub attaches: AtomicU64,
    /// Shards reset after an owner died holding the lock or after corruption.
    pub recoveries: AtomicU64,
    /// Names the diagnostic directory had no room for (no effect on caching).
    pub dir_overflow: AtomicU64,
}

const _: () = assert!(std::mem::size_of::<SegmentHeader>() <= HEADER_SIZE);

impl SegmentHeader {
    /// Checksum of the immutable fields; detects a torn or scribbled header.
    pub fn compute_checksum(&self) -> u64 {
        let mut b = Vec::with_capacity(128);
        b.extend_from_slice(&self.magic.to_le_bytes());
        b.extend_from_slice(&self.layout_version.to_le_bytes());
        b.extend_from_slice(&self.shard_count.to_le_bytes());
        b.extend_from_slice(&self.dir_slots.to_le_bytes());
        b.extend_from_slice(&self.policy.to_le_bytes());
        b.extend_from_slice(&self.gen_count.to_le_bytes());
        b.extend_from_slice(&self.soft_limit_pct.to_le_bytes());
        b.extend_from_slice(&self.total_size.to_le_bytes());
        b.extend_from_slice(&self.shard_size.to_le_bytes());
        b.extend_from_slice(&self.dir_offset.to_le_bytes());
        b.extend_from_slice(&self.gens_offset.to_le_bytes());
        b.extend_from_slice(&self.shards_offset.to_le_bytes());
        b.extend_from_slice(&self.max_item_size.to_le_bytes());
        b.extend_from_slice(&self.created_at.to_le_bytes());
        xxh3_64(&b)
    }
}

/// Diagnostic directory entry: maps a namespace or group identity to its
/// name so that `stats --groups` can print names. Claimed best-effort and
/// never consulted by cache operations: a full directory changes nothing.
#[repr(C, align(64))]
pub struct DirSlot {
    /// Low half of the 128-bit identity; 0 means free.
    pub hash_lo: AtomicU64,
    pub hash_hi: AtomicU64,
    /// Namespace identity (low half) of a group; 0 for a namespace.
    pub parent_lo: AtomicU64,
    pub ready: AtomicU32,
    pub name_len: AtomicU32,
    /// First 32 bytes of the name.
    pub name: [AtomicU64; 4],
}

pub const DIR_SLOT_SIZE: usize = 64;
const _: () = assert!(std::mem::size_of::<DirSlot>() == DIR_SLOT_SIZE);

// ---- shards ---------------------------------------------------------------

/// TLSF second-level subdivisions (log2).
pub const SL_LOG2: u32 = 4;
pub const SL_COUNT: usize = 1 << SL_LOG2;
/// Blocks below `1 << FL_SHIFT` bytes all live in first level 0.
pub const FL_SHIFT: u32 = SL_LOG2 + 3;
/// Shard offsets are u32, so block sizes are below 2^32.
pub const FL_COUNT: usize = (32 - FL_SHIFT + 1) as usize;

pub const HIST_BUCKETS: usize = 320;

/// Per-shard counters. Written only by the lock holder (plain load + store),
/// read by anyone at any time: atomics keep those reads well-defined.
#[repr(C)]
pub struct ShardStats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub sets: AtomicU64,
    pub deletes: AtomicU64,
    pub evictions: AtomicU64,
    pub expired: AtomicU64,
    pub stale: AtomicU64,
    pub rejected: AtomicU64,
    pub too_large: AtomicU64,
    pub no_memory: AtomicU64,
    pub entries: AtomicU64,
    pub payload_bytes: AtomicU64,
    pub alloc_bytes: AtomicU64,
    pub resets: AtomicU64,
    /// Updated outside the lock (fetch_add): lock attempts that had to wait.
    pub contended: AtomicU64,
    pub lat_get: [AtomicU64; HIST_BUCKETS],
    pub lat_set: [AtomicU64; HIST_BUCKETS],
}

/// State that only the lock holder touches.
#[repr(C)]
pub struct ShardState {
    /// Set while a mutation is in progress; found set by the next locker, it
    /// means the previous holder died mid-update and the shard is reset.
    pub dirty: u32,
    pub lru_head: u32,
    pub lru_tail: u32,
    pub sketch_ops: u32,
    pub fl_bitmap: u32,
    pub sl_bitmap: [u32; FL_COUNT],
    pub free_heads: [[u32; SL_COUNT]; FL_COUNT],
}

#[repr(C, align(64))]
pub struct ShardHeader {
    pub mutex: UnsafeCell<libc::pthread_mutex_t>,
    pub stats: ShardStats,
    pub state: UnsafeCell<ShardState>,
}

/// Bytes reserved at the start of each shard for its header.
pub const SHARD_HEADER_SIZE: usize = (std::mem::size_of::<ShardHeader>() + 63) & !63;

// ---- entries --------------------------------------------------------------
//
// An entry is one TLSF block:
//
//   +0  u32 prev physical block      } TLSF block header
//   +4  u32 size | flags             }
//   +8  u64 hash
//   +16 u32 next in hash chain
//   +20 u32 LRU prev (towards head)
//   +24 u32 LRU next (towards tail)
//   +28 u32 expires_at (unix seconds, 0 = never)
//   +32 u32 key length
//   +36 u32 value length
//   +40 u8  value tag
//   +48 key bytes, then value bytes
//
// The key is composite: see `KEY_PREFIX`.

pub const E_HASH: u32 = 8;
pub const E_NEXT: u32 = 16;
pub const E_LPREV: u32 = 20;
pub const E_LNEXT: u32 = 24;
pub const E_EXPIRES: u32 = 28;
pub const E_KLEN: u32 = 32;
pub const E_VLEN: u32 = 36;
pub const E_TAG: u32 = 40;
pub const E_DATA: u32 = 48;
/// Entry bytes inside a block, excluding the 8-byte block header.
pub const ENTRY_OVERHEAD: usize = (E_DATA - 8) as usize;

/// Composite key prefix, then the raw key:
///   u32 namespace generation index, u32 namespace generation,
///   u32 group generation index,     u32 group generation,
///   u32 blog id, 16 bytes 128-bit identity of (namespace, group).
pub const KEY_PREFIX: usize = 36;
