//! wp-rust-cache engine.
//!
//! A fixed-size cache living in one shared-memory segment that any number of
//! processes map at the same time. Everything stored in the segment is plain
//! data addressed by offsets, so every process can map it at a different
//! address. The engine knows nothing about PHP: keys and values are bytes,
//! plus a one-byte value tag chosen by the caller.
//!
//! See `docs/ARCHITECTURE.md` for the reasoning behind the layout.

#![cfg(unix)]

pub mod cache;
pub mod config;
pub mod groups;
pub mod histogram;
pub mod layout;
mod mutex;
mod segment;
mod shard;
pub mod stats;
pub mod value;

pub use cache::{
    take_recovery_notice, AttachMode, Cache, EntryInfo, Error, IncrOutcome, KeyUsage, Recovery,
    SetMode, SetOutcome,
};
pub use config::Config;
pub use groups::GroupHandle;
pub use stats::{GroupUsage, Stats};

/// Current time in unix seconds, from the coarse clock (vDSO, no syscall).
#[inline]
pub fn now_secs() -> u32 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    #[cfg(target_os = "linux")]
    let clock = libc::CLOCK_REALTIME_COARSE;
    #[cfg(not(target_os = "linux"))]
    let clock = libc::CLOCK_REALTIME;
    // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec as u32
}
