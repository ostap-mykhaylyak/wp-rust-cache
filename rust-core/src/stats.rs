//! Aggregated statistics, read without taking any lock.

use crate::histogram;

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub path: String,
    pub policy: &'static str,
    pub shards: u32,
    pub total_size: u64,
    /// Sum of the shards' heaps: what entries can actually use.
    pub heap_bytes: u64,
    pub max_item_size: u64,
    pub created_at: u64,
    pub attaches: u64,
    pub recoveries: u64,
    pub groups_used: u32,
    pub namespaces: u32,
    /// Size of the diagnostic name directory, and names it had no room for.
    pub group_slots: u32,
    pub groups_overflow: u64,

    pub hits: u64,
    pub misses: u64,
    pub sets: u64,
    pub deletes: u64,
    pub evictions: u64,
    pub expired: u64,
    pub stale: u64,
    pub rejected: u64,
    pub too_large: u64,
    pub no_memory: u64,
    pub resets: u64,
    pub contended: u64,

    pub entries: u64,
    /// Key + value bytes stored.
    pub payload_bytes: u64,
    /// Bytes of allocated blocks (payload + headers + alignment).
    pub alloc_bytes: u64,

    pub lat_get: Vec<u64>,
    pub lat_set: Vec<u64>,
}

impl Stats {
    pub fn hit_ratio(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        (total > 0).then(|| self.hits as f64 / total as f64)
    }

    /// Sampled latency quantile, nanoseconds. `get == true` for reads.
    pub fn latency(&self, get: bool, q: f64) -> Option<u64> {
        histogram::quantile(if get { &self.lat_get } else { &self.lat_set }, q)
    }

    pub fn samples(&self, get: bool) -> u64 {
        (if get { &self.lat_get } else { &self.lat_set })
            .iter()
            .sum()
    }
}

#[derive(Debug, Clone)]
pub struct GroupUsage {
    pub namespace: String,
    pub name: String,
    pub entries: u64,
    pub bytes: u64,
    /// Unreachable entries (written before the last flush), not yet reclaimed.
    pub stale_entries: u64,
    pub stale_bytes: u64,
}
