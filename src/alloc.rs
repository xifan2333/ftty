//! Audited platform memory allocator tuning and heap compaction.

/// Releases free glibc arena memory pages back to the operating system kernel.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[inline]
pub fn trim_memory() {
    // SAFETY: malloc_trim(0) is thread-safe on GNU/Linux libc and requests the allocator
    // to return free arena pages to the OS kernel via madvise(MADV_DONTNEED).
    unsafe {
        libc::malloc_trim(0);
    }
}

/// No-op fallback on platforms or libc variants that do not support glibc malloc_trim.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
#[inline]
pub fn trim_memory() {}
