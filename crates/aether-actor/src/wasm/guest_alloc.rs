/// Generic guest allocator backing host→guest payload delivery (ADR-0095).
///
/// The substrate writes every inbound payload — mail, init config, rehydrate
/// state — into a region it obtains from this allocator, then calls the entry
/// point (`receive` / `init_with_config` / `on_rehydrate`). The host owns no
/// fixed offset into guest memory; the guest reports where its memory is, so
/// delivery is independent of the guest's linear-memory layout.
///
/// The export is `cabi_realloc`-shaped (the Component Model canonical): one
/// function that allocates (`old_ptr == 0`), grows possibly-relocating
/// (`new_size > old_size`), or frees (`new_size == 0`). The substrate drives it
/// under the run-token invariant — one small region allocated once and reused,
/// one large region grown to fit, each consumed synchronously — and never frees
/// (wasm has no `memory.shrink`, so a free reclaims nothing).
///
/// The first sub-threshold allocation — the small delivery region the host
/// obtains once at instantiate (`align = 8`, `new_size <= SMALL_REGION_BYTES`)
/// — is served from a reused `static` scratch (ADR-0095 point 4) rather than
/// the global allocator. Every later allocation (the large region, any
/// subsequent small one) falls through to the global allocator. The scratch is
/// purely a guest-side optimization beneath the same ABI; a guest in any
/// language supplies the same export however it likes.
///
/// The module is not target-gated so the allocator is host-testable (its body
/// is plain `alloc`-crate code, like [`crate::Slot`]); the `realloc_p32` FFI
/// shims that call it remain wasm32-only.
use alloc::alloc::{Layout, alloc, dealloc, realloc};
use core::cell::UnsafeCell;
use core::ptr::{copy_nonoverlapping, null_mut};

/// Size of the `static` scratch that backs the first sub-threshold
/// delivery allocation. Kept `>=` the host's `SMALL_REGION_BYTES`
/// (`aether-substrate`, currently 8 KiB) so the scratch covers that
/// instantiate-time small region. If it ever drifts below the host floor
/// the first small allocation simply spills to the global allocator —
/// still correct, just no longer served from the scratch.
const SCRATCH_REGION_BYTES: usize = 8 * 1024;

/// Alignment the scratch satisfies. The host requests `align = 8` for the
/// delivery region; a request needing more falls through to the global
/// allocator even if it would fit by size.
const SCRATCH_ALIGN: usize = 8;

/// Reused backing store for the first small delivery region plus a
/// single-shot claim flag. `UnsafeCell` + a blanket `Sync` impl mirrors
/// [`crate::Slot`]: the guest is single-threaded (ADR-0010 §5) and the
/// substrate serializes delivery under the run token, so the cell is never
/// touched concurrently and no atomics are needed. `bytes` is the first
/// field of a `repr(C, align(8))` struct, so its base is
/// `SCRATCH_ALIGN`-aligned.
#[repr(C, align(8))]
struct Scratch {
    bytes: UnsafeCell<[u8; SCRATCH_REGION_BYTES]>,
    claimed: UnsafeCell<bool>,
}

// SAFETY: a single-threaded WASM guest plus the substrate's serialized,
// run-token-gated delivery mean `SCRATCH` is only ever touched from one
// thread at a time — the same argument that licenses `crate::Slot`'s
// `Sync`. On the host unit-test build the static is reached from one test.
unsafe impl Sync for Scratch {}

static SCRATCH: Scratch = Scratch {
    bytes: UnsafeCell::new([0u8; SCRATCH_REGION_BYTES]),
    claimed: UnsafeCell::new(false),
};

/// `cabi_realloc`-shaped reallocation.
///
/// - `old_ptr == 0` — allocate `new_size` bytes, return the pointer. The
///   first such call that fits the scratch (`new_size <=
///   SCRATCH_REGION_BYTES`, `align <= SCRATCH_ALIGN`) returns the reused
///   `static` scratch base; later calls use the global allocator.
/// - `new_size == 0` — free the `(old_ptr, old_size, align)` allocation,
///   return null. Freeing the scratch base is a no-op.
/// - otherwise — resize the allocation to `new_size`, returning the current
///   pointer (which differs from `old_ptr` if the block was relocated).
///
/// # Safety
/// `old_ptr` / `old_size` / `align` must describe a live allocation from a
/// prior call (or `0` / `0` for a fresh allocation), and `align` must be a
/// nonzero power of two. The guest is single-threaded, so there is no
/// aliasing.
pub unsafe fn realloc_bytes(
    old_ptr: *mut u8,
    old_size: usize,
    align: usize,
    new_size: usize,
) -> *mut u8 {
    let scratch_base = SCRATCH.bytes.get().cast::<u8>();
    if new_size == 0 {
        if !old_ptr.is_null() && old_ptr != scratch_base {
            // SAFETY: caller's layout contract holds (`align` a nonzero
            // power of two), and `old_ptr` is a live global allocation —
            // the scratch base is excluded above, so this only frees global
            // memory, never the `static` scratch.
            unsafe {
                let layout = Layout::from_size_align_unchecked(old_size, align);
                dealloc(old_ptr, layout);
            }
        }
        // Freeing the scratch base reclaims nothing (it is a `static`); the
        // host never frees the small region anyway (ADR-0095 point 6).
        return null_mut();
    }
    if old_ptr.is_null() {
        // SAFETY: single-threaded guest + serialized delivery — no other
        // live reference to the claim flag (the `Scratch` `Sync` argument).
        let already_claimed = unsafe { *SCRATCH.claimed.get() };
        if !already_claimed && new_size <= SCRATCH_REGION_BYTES && align <= SCRATCH_ALIGN {
            // SAFETY: same single-threaded / serialized argument; this is
            // the only writer of the flag, and it transitions once.
            unsafe { *SCRATCH.claimed.get() = true };
            return scratch_base;
        }
        // SAFETY: caller's layout contract holds; a fresh global allocation.
        return unsafe {
            let layout = Layout::from_size_align_unchecked(new_size, align);
            alloc(layout)
        };
    }
    if old_ptr == scratch_base {
        // Contractually unreachable — the host grows the large region, not
        // the scratch-backed small one (ADR-0095 points 4/7). Handle it
        // without UB by allocating fresh and copying rather than passing the
        // `static` pointer to `realloc`.
        // SAFETY: caller's layout contract holds; `scratch_base` is a live,
        // initialized `static` of `SCRATCH_REGION_BYTES` bytes, `fresh` is a
        // fresh global allocation of `new_size`, and the copy length is the
        // min of `old_size` / `new_size`, in-bounds of both regions.
        return unsafe {
            let layout = Layout::from_size_align_unchecked(new_size, align);
            let fresh = alloc(layout);
            if !fresh.is_null() {
                copy_nonoverlapping(scratch_base, fresh, old_size.min(new_size));
            }
            fresh
        };
    }
    // SAFETY: caller's layout contract holds; `old_ptr` / `old_size` /
    // `align` describe a live global allocation (scratch base excluded
    // above), matched by the global `realloc`.
    unsafe {
        let old_layout = Layout::from_size_align_unchecked(old_size, align);
        realloc(old_ptr, old_layout, new_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scratch serves the first eligible sub-threshold allocation and
    /// only that one; oversized and over-aligned requests bypass it, and a
    /// second eligible request falls through to the global allocator. Only
    /// this test claims the process-global single-shot flag, so the
    /// sequence is deterministic without a reset hook.
    #[test]
    fn scratch_serves_first_small_alloc_only() {
        let base = SCRATCH.bytes.get().cast::<u8>();
        let null = null_mut();
        // SAFETY: every call obeys `realloc_bytes`' contract — `null`/`0`
        // for fresh allocs, and a matching `(ptr, size, align)` triple to
        // free what a prior call returned. One test thread, no aliasing.
        unsafe {
            // Oversized: larger than the scratch → global allocator. The
            // flag stays unclaimed because the size gate fails.
            let big = realloc_bytes(null, 0, SCRATCH_ALIGN, SCRATCH_REGION_BYTES + 1);
            assert_ne!(big, base, "oversized alloc bypasses the scratch");
            realloc_bytes(big, SCRATCH_REGION_BYTES + 1, SCRATCH_ALIGN, 0);

            // Over-aligned: fits by size but needs more alignment than the
            // scratch provides → global allocator, flag still unclaimed.
            let aligned = realloc_bytes(null, 0, SCRATCH_ALIGN * 2, 64);
            assert_ne!(aligned, base, "over-aligned alloc bypasses the scratch");
            realloc_bytes(aligned, 64, SCRATCH_ALIGN * 2, 0);

            // First eligible small alloc (the host's instantiate-time small
            // region) → served from the scratch.
            let first = realloc_bytes(null, 0, SCRATCH_ALIGN, SCRATCH_REGION_BYTES);
            assert_eq!(
                first, base,
                "first eligible small alloc returns the scratch base"
            );

            // Single-shot: the next eligible alloc falls through to global.
            let second = realloc_bytes(null, 0, SCRATCH_ALIGN, 64);
            assert_ne!(second, base, "scratch is spent after the first claim");
            realloc_bytes(second, 64, SCRATCH_ALIGN, 0);
        }
    }

    /// Freeing the scratch base is a no-op that returns null and must not
    /// reach the global allocator's `dealloc` (UB on a `static`).
    #[test]
    fn freeing_scratch_base_is_a_noop() {
        let base = SCRATCH.bytes.get().cast::<u8>();
        // SAFETY: `base` is the scratch `static`; the free branch guards it
        // and never calls `dealloc` on it.
        let freed = unsafe { realloc_bytes(base, SCRATCH_REGION_BYTES, SCRATCH_ALIGN, 0) };
        assert!(freed.is_null(), "freeing the scratch returns null");
    }
}
