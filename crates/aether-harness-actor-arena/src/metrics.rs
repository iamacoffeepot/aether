use std::{
    alloc::{GlobalAlloc, Layout, System},
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATION_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static DEALLOCATION_CALLS: AtomicU64 = AtomicU64::new(0);
static DEALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

// SAFETY: every operation is forwarded to `System` with the original layout
// and pointer; the side counters do not affect allocator ownership.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies the `GlobalAlloc` layout contract.
        let pointer = unsafe { System.alloc(layout) };
        if COUNTING.load(Ordering::Relaxed) && !pointer.is_null() {
            ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if COUNTING.load(Ordering::Relaxed) {
            DEALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
            DEALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        // SAFETY: the caller returns a pointer and layout produced by this
        // allocator, which delegates allocation ownership to `System`.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller supplies a live pointer, its original layout, and
        // the requested replacement size under the `GlobalAlloc` contract.
        let replacement = unsafe { System.realloc(pointer, layout, new_size) };
        if COUNTING.load(Ordering::Relaxed) && !replacement.is_null() {
            ALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
            DEALLOCATION_CALLS.fetch_add(1, Ordering::Relaxed);
            DEALLOCATED_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        replacement
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AllocationSnapshot {
    pub allocation_calls: u64,
    pub allocated_bytes: u64,
    pub deallocation_calls: u64,
    pub deallocated_bytes: u64,
}

pub struct AllocationGuard {
    active: bool,
}

/// Allocation counting is an explicitly separate diagnostic pass because the
/// atomics perturb the primary throughput measurement.
#[must_use]
pub fn begin_allocation_counting() -> AllocationGuard {
    ALLOCATION_CALLS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    DEALLOCATION_CALLS.store(0, Ordering::Relaxed);
    DEALLOCATED_BYTES.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Release);
    AllocationGuard { active: true }
}

impl AllocationGuard {
    #[must_use]
    pub fn finish(mut self) -> AllocationSnapshot {
        COUNTING.store(false, Ordering::Release);
        self.active = false;
        AllocationSnapshot {
            allocation_calls: ALLOCATION_CALLS.load(Ordering::Relaxed),
            allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
            deallocation_calls: DEALLOCATION_CALLS.load(Ordering::Relaxed),
            deallocated_bytes: DEALLOCATED_BYTES.load(Ordering::Relaxed),
        }
    }
}

impl Drop for AllocationGuard {
    fn drop(&mut self) {
        if self.active {
            COUNTING.store(false, Ordering::Release);
        }
    }
}

/// Peak resident set size for the current fresh trial process.
#[must_use]
pub fn peak_rss_bytes() -> u64 {
    let mut usage = MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `usage` points to writable storage for `getrusage`, and it is
    // read only when libc reports success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return 0;
    }
    // SAFETY: successful `getrusage` initialized the complete `rusage`.
    let maximum_rss = unsafe { usage.assume_init() }.ru_maxrss;

    #[cfg(target_os = "macos")]
    {
        maximum_rss.try_into().unwrap_or(0)
    }
    #[cfg(not(target_os = "macos"))]
    {
        u64::try_from(maximum_rss).unwrap_or(0).saturating_mul(1_024)
    }
}
