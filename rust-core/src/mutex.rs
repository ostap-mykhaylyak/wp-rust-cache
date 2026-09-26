//! Robust, process-shared pthread mutex living in the segment.
//!
//! Robust means the kernel tracks the owner: if a process dies holding the
//! lock (`kill -9`, segfault, OOM killer), the next `lock()` returns
//! `EOWNERDEAD` instead of blocking forever. We then mark the mutex
//! consistent and tell the caller, who resets the data the lock protected.

use std::io;

pub enum Locked {
    /// Normal acquisition.
    Clean,
    /// The previous owner died holding the lock; protected data is suspect.
    OwnerDied,
}

/// Initialises a mutex in place.
///
/// # Safety
/// `m` must point to writable memory of `pthread_mutex_t` size and alignment
/// that no other process is using yet.
pub unsafe fn init(m: *mut libc::pthread_mutex_t) -> io::Result<()> {
    let mut attr: libc::pthread_mutexattr_t = std::mem::zeroed();
    check(libc::pthread_mutexattr_init(&mut attr))?;
    let r = (|| {
        check(libc::pthread_mutexattr_setpshared(
            &mut attr,
            libc::PTHREAD_PROCESS_SHARED,
        ))?;
        check(libc::pthread_mutexattr_setrobust(
            &mut attr,
            libc::PTHREAD_MUTEX_ROBUST,
        ))?;
        check(libc::pthread_mutex_init(m, &attr))
    })();
    libc::pthread_mutexattr_destroy(&mut attr);
    r
}

/// Locks, counting contention through `contended`.
///
/// # Safety
/// `m` must point to a mutex initialised by [`init`] in mapped memory.
#[inline]
pub unsafe fn lock(
    m: *mut libc::pthread_mutex_t,
    contended: &std::sync::atomic::AtomicU64,
) -> io::Result<Locked> {
    let mut r = libc::pthread_mutex_trylock(m);
    if r == libc::EBUSY {
        contended.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        r = libc::pthread_mutex_lock(m);
    }
    match r {
        0 => Ok(Locked::Clean),
        libc::EOWNERDEAD => {
            check(libc::pthread_mutex_consistent(m))?;
            Ok(Locked::OwnerDied)
        }
        e => Err(io::Error::from_raw_os_error(e)),
    }
}

/// # Safety
/// The calling thread must hold the lock.
#[inline]
pub unsafe fn unlock(m: *mut libc::pthread_mutex_t) {
    libc::pthread_mutex_unlock(m);
}

fn check(r: i32) -> io::Result<()> {
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(r))
    }
}
