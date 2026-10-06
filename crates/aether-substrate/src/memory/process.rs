//! The size of the whole process, read from the platform when a report is
//! built.

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "macos")]
use std::mem::MaybeUninit;

/// The process's resident set size in bytes, or `None` when the platform has
/// no reader or the read fails.
#[cfg(target_os = "linux")]
pub fn resident_set_bytes() -> Option<u64> {
    // statm's second field is the resident set, in pages.
    let statm = fs::read_to_string("/proc/self/statm").ok()?;
    let pages = statm.split_ascii_whitespace().nth(1)?.parse::<u64>().ok()?;

    // SAFETY: `sysconf` takes no pointer and reads no caller memory;
    // `_SC_PAGESIZE` is a name every Linux libc answers.
    let page_bytes = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    pages.checked_mul(u64::try_from(page_bytes).ok()?)
}

/// The process's resident set size in bytes, or `None` when the read fails.
#[cfg(target_os = "macos")]
pub fn resident_set_bytes() -> Option<u64> {
    let mut info = MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = libc::c_int::try_from(size_of::<libc::proc_taskinfo>()).ok()?;

    // SAFETY: `info` is a writable `proc_taskinfo` and `size` is its exact
    // size, which is what the `PROC_PIDTASKINFO` flavor writes into; the
    // kernel writes at most `size` bytes and reads none of the buffer.
    let written =
        unsafe { libc::proc_pidinfo(libc::getpid(), libc::PROC_PIDTASKINFO, 0, info.as_mut_ptr().cast(), size) };
    if written != size {
        return None;
    }

    // SAFETY: the buffer was zeroed, every field of `proc_taskinfo` is an
    // integer for which all-zero bytes are a value, and the kernel reported
    // it wrote the whole struct.
    let info = unsafe { info.assume_init() };
    Some(info.pti_resident_size)
}

/// No reader on this platform.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn resident_set_bytes() -> Option<u64> {
    None
}
