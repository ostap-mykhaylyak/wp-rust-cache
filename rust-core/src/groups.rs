//! Namespaces and groups.
//!
//! A group is identified by a 128-bit hash of (namespace, group name) that
//! is written into every key, so there is no shared table to fill up:
//! WooCommerce's one-group-per-product (`product_123`) works for any number
//! of products. Flushes use generation counters in a fixed table indexed by
//! hash; groups that share a counter are flushed together, which can only
//! cost extra misses, never return stale data.
//!
//! Names are kept in a separate, best-effort directory for diagnostics.

use crate::layout::DirSlot;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use xxhash_rust::xxh3::xxh3_128;

/// Everything an operation needs to build a key for one group.
/// Process-local and cheap to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupHandle {
    pub ns_gen: u32,
    pub group_gen: u32,
    pub id: [u8; 16],
}

fn split(h: u128) -> (u64, u64) {
    ((h as u64).max(1), (h >> 64) as u64)
}

pub fn namespace_id(ns: &[u8]) -> u128 {
    let mut b = Vec::with_capacity(ns.len() + 1);
    b.push(0u8);
    b.extend_from_slice(ns);
    xxh3_128(&b)
}

pub fn group_id(ns: &[u8], group: &[u8]) -> u128 {
    let mut b = Vec::with_capacity(ns.len() + group.len() + 5);
    b.push(1u8);
    b.extend_from_slice(&(ns.len() as u32).to_le_bytes());
    b.extend_from_slice(ns);
    b.extend_from_slice(group);
    xxh3_128(&b)
}

/// Generation counter index of an identity.
pub fn gen_index(id: u128, gen_count: u32) -> u32 {
    ((id >> 64) as u64 % gen_count as u64) as u32
}

pub fn handle(ns: &[u8], group: &[u8], gen_count: u32) -> GroupHandle {
    let g = group_id(ns, group);
    GroupHandle {
        ns_gen: gen_index(namespace_id(ns), gen_count),
        group_gen: gen_index(g, gen_count),
        id: g.to_le_bytes(),
    }
}

// ---- directory (diagnostics only) -----------------------------------------

/// Probes at most this many slots: registering a name must stay cheap even
/// when the directory is full.
const MAX_PROBES: usize = 32;

fn fill(s: &DirSlot, hi: u64, parent_lo: u64, name: &[u8]) {
    s.hash_hi.store(hi, Relaxed);
    s.parent_lo.store(parent_lo, Relaxed);
    let n = name.len().min(32);
    s.name_len.store(name.len() as u32, Relaxed);
    let mut buf = [0u8; 32];
    buf[..n].copy_from_slice(&name[..n]);
    for (i, w) in s.name.iter().enumerate() {
        w.store(
            u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap()),
            Relaxed,
        );
    }
    s.ready.store(1, Release);
}

/// Records a name. Returns false when there was no room (harmless).
pub fn register(dir: &[DirSlot], id: u128, parent: Option<u128>, name: &[u8]) -> bool {
    let (lo, hi) = split(id);
    let parent_lo = parent.map(|p| split(p).0).unwrap_or(0);
    let n = dir.len();
    let start = (lo >> 7) as usize % n;
    for i in 0..MAX_PROBES.min(n) {
        let s = &dir[(start + i) % n];
        let cur = s.hash_lo.load(Acquire);
        if cur == lo {
            // Same identity already recorded (or being recorded).
            return true;
        }
        if cur == 0 && s.hash_lo.compare_exchange(0, lo, AcqRel, Acquire).is_ok() {
            fill(s, hi, parent_lo, name);
            return true;
        }
    }
    false
}

pub struct Entry {
    pub lo: u64,
    pub parent_lo: u64,
    pub name: String,
}

pub fn entries(dir: &[DirSlot]) -> Vec<Entry> {
    dir.iter()
        .filter(|s| s.ready.load(Acquire) == 1)
        .map(|s| Entry {
            lo: s.hash_lo.load(Relaxed),
            parent_lo: s.parent_lo.load(Relaxed),
            name: name(s),
        })
        .collect()
}

pub fn lookup(dir: &[DirSlot], id: u128) -> Option<&DirSlot> {
    let (lo, _) = split(id);
    let n = dir.len();
    let start = (lo >> 7) as usize % n;
    (0..MAX_PROBES.min(n))
        .map(|i| &dir[(start + i) % n])
        .find(|s| s.ready.load(Acquire) == 1 && s.hash_lo.load(Relaxed) == lo)
}

pub fn name(s: &DirSlot) -> String {
    let mut buf = [0u8; 32];
    for (i, w) in s.name.iter().enumerate() {
        buf[i * 8..i * 8 + 8].copy_from_slice(&w.load(Relaxed).to_le_bytes());
    }
    let len = (s.name_len.load(Relaxed) as usize).min(32);
    let mut out = String::from_utf8_lossy(&buf[..len]).into_owned();
    if s.name_len.load(Relaxed) as usize > 32 {
        out.push('…');
    }
    out
}

pub fn lo_of(id: u128) -> u64 {
    split(id).0
}
