//! Configuration file (`/etc/wp-rust-cache/config.toml`).
//!
//! ```toml
//! [cache]
//! enabled = true
//! memory = "1GB"
//! shards = 64
//! eviction = "tinylfu"      # default; or "lru"
//!
//! [shared_memory]
//! path = "/dev/shm/wp-rust-cache"
//! permissions = "0600"
//! ```
//!
//! Every key is optional. Sizes accept `B`, `KB`, `MB`, `GB` (binary: 1 GB =
//! 1024 MB) with or without the trailing `B`.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG_PATH: &str = "/etc/wp-rust-cache/config.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Policy {
    Lru = 0,
    TinyLfu = 1,
}

impl Policy {
    pub fn from_u32(v: u32) -> Policy {
        if v == 1 {
            Policy::TinyLfu
        } else {
            Policy::Lru
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Policy::Lru => "lru",
            Policy::TinyLfu => "tinylfu",
        }
    }
}

/// Validated configuration, ready to size a segment.
#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub memory: u64,
    pub shards: u32,
    pub policy: Policy,
    pub max_item_size: u64,
    pub soft_limit_pct: u32,
    pub group_slots: u32,
    pub path_template: String,
    pub permissions: u32,
    pub group: Option<String>,
    /// Owner given to the segment when root creates it (root never creates
    /// a segment it would own itself: PHP-FPM workers could not open it).
    pub owner: Option<String>,
    pub preallocate: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: true,
            memory: 256 << 20,
            shards: 64,
            // Chosen by `wprc-bench eviction` (docs/BENCHMARKS.md): TinyLFU
            // never lost to LRU on WordPress-shaped traffic and wins by up
            // to 3.5 points of hit ratio when memory is short.
            policy: Policy::TinyLfu,
            max_item_size: 8 << 20,
            soft_limit_pct: 90,
            group_slots: 16384,
            path_template: "/dev/shm/wp-rust-cache".into(),
            permissions: 0o600,
            group: None,
            owner: None,
            preallocate: true,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawFile {
    cache: RawCache,
    shared_memory: RawShm,
    // Accepted for forward compatibility; multisite is detected by WordPress.
    #[allow(dead_code)]
    wordpress: Option<toml::Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawCache {
    enabled: Option<bool>,
    memory: Option<SizeValue>,
    shards: Option<u32>,
    eviction: Option<String>,
    max_item_size: Option<SizeValue>,
    soft_limit: Option<u32>,
    group_slots: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawShm {
    path: Option<String>,
    permissions: Option<String>,
    group: Option<String>,
    owner: Option<String>,
    preallocate: Option<bool>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SizeValue {
    Bytes(u64),
    Text(String),
}

impl SizeValue {
    fn bytes(&self) -> Result<u64, String> {
        match self {
            SizeValue::Bytes(b) => Ok(*b),
            SizeValue::Text(t) => parse_size(t),
        }
    }
}

/// Parses "1GB", "4096MB", "512M", "64k", "1048576".
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: u64 = num.parse().map_err(|_| format!("invalid size {s:?}"))?;
    let mult: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "K" | "KB" | "KIB" => 1 << 10,
        "M" | "MB" | "MIB" => 1 << 20,
        "G" | "GB" | "GIB" => 1 << 30,
        _ => return Err(format!("invalid size unit in {s:?}")),
    };
    n.checked_mul(mult)
        .ok_or_else(|| format!("size {s:?} overflows"))
}

pub fn format_size(b: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

impl Config {
    /// Reads a config file. A missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let raw: RawFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut c = Config::default();
        if let Some(v) = raw.cache.enabled {
            c.enabled = v;
        }
        if let Some(v) = raw.cache.memory {
            c.memory = v.bytes()?;
        }
        if let Some(v) = raw.cache.shards {
            c.shards = v;
        }
        if let Some(v) = raw.cache.eviction {
            c.policy = match v.to_ascii_lowercase().as_str() {
                "lru" => Policy::Lru,
                "tinylfu" | "tiny-lfu" => Policy::TinyLfu,
                other => return Err(format!("unknown eviction policy {other:?}")),
            };
        }
        if let Some(v) = raw.cache.max_item_size {
            c.max_item_size = v.bytes()?;
        }
        if let Some(v) = raw.cache.soft_limit {
            c.soft_limit_pct = v;
        }
        if let Some(v) = raw.cache.group_slots {
            c.group_slots = v;
        }
        if let Some(v) = raw.shared_memory.path {
            c.path_template = v;
        }
        if let Some(v) = raw.shared_memory.permissions {
            c.permissions = u32::from_str_radix(v.trim_start_matches("0o"), 8)
                .map_err(|_| format!("invalid permissions {v:?}"))?;
        }
        c.group = raw.shared_memory.group.filter(|g| !g.is_empty());
        c.owner = raw.shared_memory.owner.filter(|o| !o.is_empty());
        if let Some(v) = raw.shared_memory.preallocate {
            c.preallocate = v;
        }
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !self.shards.is_power_of_two() || self.shards > 1024 {
            return Err("shards must be a power of two between 1 and 1024".into());
        }
        let shard = self.shard_size();
        if shard < (1 << 20) {
            return Err(format!(
                "memory / shards = {} per shard; at least 1 MB is needed (lower shards or raise memory)",
                format_size(shard)
            ));
        }
        if shard > u32::MAX as u64 - 4096 {
            return Err("a shard cannot exceed 4 GB; raise the shard count".into());
        }
        if !(50..=100).contains(&self.soft_limit_pct) {
            return Err("soft_limit must be between 50 and 100 (percent)".into());
        }
        if self.permissions & 0o007 != 0 {
            return Err(
                "shared memory must not be accessible to other users (permissions & 0007 != 0)"
                    .into(),
            );
        }
        if self.group_slots < 64 || self.group_slots > 1 << 20 {
            return Err("group_slots must be between 64 and 1048576".into());
        }
        Ok(())
    }

    /// Bytes per shard, rounded down to a page.
    pub fn shard_size(&self) -> u64 {
        (self.memory / self.shards as u64) & !4095
    }

    /// Segment path with `{uid}` and `{user}` expanded for the effective user.
    pub fn path(&self) -> PathBuf {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let mut p = self.path_template.replace("{uid}", &uid.to_string());
        if p.contains("{user}") {
            p = p.replace("{user}", &user_name(uid).unwrap_or_else(|| uid.to_string()));
        }
        PathBuf::from(p)
    }
}

fn user_name(uid: u32) -> Option<String> {
    // SAFETY: getpwuid returns NULL or a pointer to static storage that stays
    // valid until the next getpw* call; we copy the name out immediately.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() {
            return None;
        }
        let name = std::ffi::CStr::from_ptr((*pw).pw_name);
        Some(name.to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1GB").unwrap(), 1 << 30);
        assert_eq!(parse_size("4096MB").unwrap(), 4 << 30);
        assert_eq!(parse_size("512m").unwrap(), 512 << 20);
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert!(parse_size("1XB").is_err());
    }

    #[test]
    fn parse_example() {
        let c = Config::parse(
            r#"
[cache]
enabled = true
memory = "1GB"
shards = 64
eviction = "lru"

[shared_memory]
path = "/dev/shm/wp-rust-cache"
permissions = "0660"

[wordpress]
multisite = false
"#,
        )
        .unwrap();
        assert_eq!(c.memory, 1 << 30);
        assert_eq!(c.permissions, 0o660);
        assert_eq!(c.shard_size(), (1 << 30) / 64);
    }

    #[test]
    fn rejects_world_readable() {
        assert!(Config::parse("[shared_memory]\npermissions = \"0664\"").is_err());
    }
}
