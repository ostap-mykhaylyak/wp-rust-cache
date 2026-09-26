//! Mapping of the shared-memory file (or an anonymous shared mapping for
//! tests and single-process use).

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub struct Segment {
    pub base: *mut u8,
    pub len: usize,
    pub fd: i32,
    pub path: Option<PathBuf>,
}

// SAFETY: the mapping is plain shared memory; all concurrent access to it is
// synchronised by the atomics and process-shared mutexes stored inside it.
unsafe impl Send for Segment {}
unsafe impl Sync for Segment {}

pub struct Opened {
    pub fd: i32,
    pub created: bool,
}

fn last_err(what: &str, path: &Path) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what} {}: {e}", path.display()))
}

fn cpath(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

/// Opens the segment file, creating it (exclusively) when missing.
/// `O_NOFOLLOW` refuses a symlink planted in a shared directory.
pub fn open_file(path: &Path, mode: u32, create: bool) -> io::Result<Opened> {
    let c = cpath(path)?;
    let base = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    if create {
        // SAFETY: `c` is a valid NUL-terminated path.
        let fd = unsafe { libc::open(c.as_ptr(), base | libc::O_CREAT | libc::O_EXCL, mode) };
        if fd >= 0 {
            // The umask may have narrowed the mode; set it exactly.
            // SAFETY: fd is a file descriptor we just opened.
            unsafe { libc::fchmod(fd, mode as libc::mode_t) };
            return Ok(Opened { fd, created: true });
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
            return Err(last_err("cannot create", path));
        }
    }
    // SAFETY: as above.
    let fd = unsafe { libc::open(c.as_ptr(), base) };
    if fd < 0 {
        return Err(last_err("cannot open", path));
    }
    Ok(Opened { fd, created: false })
}

pub fn fstat(fd: i32) -> io::Result<libc::stat> {
    // SAFETY: `st` is a valid out-pointer; fd is caller-owned.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st)
    }
}

/// Exclusive lock with a deadline. A request must never queue behind another
/// process's attach for long: past the deadline the caller gives up and runs
/// without the shared cache, retrying later.
pub fn lock_exclusive(fd: i32, deadline: std::time::Duration) -> io::Result<bool> {
    let start = std::time::Instant::now();
    loop {
        // SAFETY: plain syscall on a caller-owned fd.
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EWOULDBLOCK) | Some(libc::EINTR) => {}
            _ => return Err(e),
        }
        if start.elapsed() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

pub fn flock(fd: i32, op: i32) -> io::Result<()> {
    loop {
        // SAFETY: plain syscall on a caller-owned fd.
        if unsafe { libc::flock(fd, op) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}

/// Sizes the file and, if asked, reserves every page now: a full tmpfs would
/// otherwise raise SIGBUS on first write, in the middle of a request.
pub fn size_file(fd: i32, len: u64, preallocate: bool) -> io::Result<()> {
    // SAFETY: plain syscalls on a caller-owned fd.
    unsafe {
        if libc::ftruncate(fd, len as libc::off_t) != 0 {
            return Err(io::Error::last_os_error());
        }
        if preallocate {
            #[cfg(target_os = "linux")]
            {
                let r = libc::posix_fallocate(fd, 0, len as libc::off_t);
                if r != 0 {
                    return Err(io::Error::from_raw_os_error(r));
                }
            }
        }
    }
    Ok(())
}

impl Segment {
    pub fn map(fd: i32, len: usize, path: PathBuf) -> io::Result<Segment> {
        // SAFETY: mapping a file we own; the result is checked for MAP_FAILED.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Segment {
            base: p as *mut u8,
            len,
            fd,
            path: Some(path),
        })
    }

    /// Anonymous shared mapping: shared with children after `fork()`, never
    /// visible in the filesystem. Used by tests and benchmarks.
    pub fn anonymous(len: usize) -> io::Result<Segment> {
        // SAFETY: anonymous mapping; result checked for MAP_FAILED.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Segment {
            base: p as *mut u8,
            len,
            fd: -1,
            path: None,
        })
    }

    /// True when the file was removed from the filesystem (another process
    /// recreated the segment, or an administrator deleted it).
    pub fn is_unlinked(&self) -> bool {
        if self.fd < 0 {
            return false;
        }
        match fstat(self.fd) {
            Ok(st) => st.st_nlink == 0,
            Err(_) => true,
        }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        // SAFETY: base/len describe a mapping we created; fd is ours.
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.len);
            if self.fd >= 0 {
                libc::close(self.fd);
            }
        }
    }
}

pub fn unlink(path: &Path) -> io::Result<()> {
    let c = cpath(path)?;
    // SAFETY: valid NUL-terminated path.
    if unsafe { libc::unlink(c.as_ptr()) } != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ENOENT) {
            return Err(e);
        }
    }
    Ok(())
}

pub fn close(fd: i32) {
    // SAFETY: closing a descriptor we own.
    unsafe { libc::close(fd) };
}

pub fn group_id(name: &str) -> Option<u32> {
    let c = CString::new(name).ok()?;
    // SAFETY: getgrnam returns NULL or static storage; we read gr_gid only.
    unsafe {
        let g = libc::getgrnam(c.as_ptr());
        if g.is_null() {
            None
        } else {
            Some((*g).gr_gid)
        }
    }
}

/// (uid, primary gid) of a user name.
pub fn user_ids(name: &str) -> Option<(u32, u32)> {
    let c = CString::new(name).ok()?;
    // SAFETY: getpwnam returns NULL or static storage; we copy two integers.
    unsafe {
        let p = libc::getpwnam(c.as_ptr());
        if p.is_null() {
            None
        } else {
            Some(((*p).pw_uid, (*p).pw_gid))
        }
    }
}

/// `u32::MAX` leaves that id unchanged.
pub fn fchown(fd: i32, uid: u32, gid: u32) -> io::Result<()> {
    // SAFETY: plain syscall on a caller-owned fd.
    if unsafe { libc::fchown(fd, uid, gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
