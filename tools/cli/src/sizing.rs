//! How much memory the cache may take on this machine.
//!
//! The segment is RAM the kernel cannot reclaim, and with `preallocate` all
//! of it is taken at once, whatever the cache holds. The first production
//! install (2 GB RAM, no swap, a container whose /dev/shm was 3.9 GB) got a
//! 1 GB segment sized from /dev/shm alone and PHP-FPM was OOM-killed every
//! few minutes. Sizing therefore starts from the RAM actually available,
//! container limits included.

use std::fs;

/// Largest share of RAM `install` picks by default.
const DEFAULT_SHARE: u64 = 10;
/// Share of RAM above which `install` and `status` warn.
const WARN_PERCENT: u64 = 25;
const MB: u64 = 1 << 20;

/// RAM this machine (or container) can use: MemTotal, lowered by a cgroup
/// memory limit when there is one.
pub fn ram_bytes() -> Option<u64> {
    let total = fs::read_to_string("/proc/meminfo").ok().and_then(|m| {
        m.lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
    })?;
    let limit = [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]
    .iter()
    .filter_map(|p| fs::read_to_string(p).ok())
    .filter_map(|v| v.trim().parse::<u64>().ok())
    .find(|&v| v > 0 && v < total);
    Some(limit.unwrap_or(total))
}

/// Default cache size: a tenth of the RAM, at most half of the free space in
/// /dev/shm (a configuration change briefly needs two segments), at most
/// 1 GB, in steps of 32 MB. `None` below 64 MB: too little to be useful.
pub fn recommended(ram: u64, shm_free: u64) -> Option<u64> {
    let m = (ram / DEFAULT_SHARE).min(shm_free / 2).min(1 << 30) / (32 * MB) * (32 * MB);
    (m >= 64 * MB).then_some(m)
}

/// A warning when `memory` is a large share of the RAM.
pub fn warning(memory: u64, ram: u64) -> Option<String> {
    if ram == 0 || memory * 100 <= ram * WARN_PERCENT {
        return None;
    }
    Some(format!(
        "memory = {} is {}% of this server's RAM ({}). The segment is RAM the kernel \
         cannot reclaim (with preallocate it is all taken at once); a large share can \
         get PHP-FPM OOM-killed. Consider {} or less (docs/OPERATIONS.md, \"Memory\").",
        wprc_core::config::format_size(memory),
        memory * 100 / ram,
        wprc_core::config::format_size(ram),
        wprc_core::config::format_size((ram / DEFAULT_SHARE).max(64 * MB) / (32 * MB) * (32 * MB)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    const GB: u64 = 1 << 30;

    #[test]
    fn the_first_production_server() {
        // 2 GB of RAM, /dev/shm 3.9 GB free: the old rule gave 1 GB.
        assert_eq!(recommended(2 * GB, 3900 * MB), Some(192 * MB));
        assert!(warning(GB, 2 * GB).is_some());
        assert!(warning(256 * MB, 2 * GB).is_none());
    }

    #[test]
    fn bounds() {
        assert_eq!(recommended(32 * GB, 16 * GB), Some(GB), "capped at 1 GB");
        assert_eq!(
            recommended(8 * GB, 400 * MB),
            Some(192 * MB),
            "half of free /dev/shm"
        );
        assert_eq!(
            recommended(512 * MB, 8 * GB),
            None,
            "too small to be useful"
        );
        assert_eq!(recommended(4 * GB, 4 * GB), Some(384 * MB));
    }
}
