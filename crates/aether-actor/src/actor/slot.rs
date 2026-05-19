//! Single-instance backing store for the macro-emitted `static`
//! component slot. WASM components are single-threaded per instance
//! (ADR-0010 §5 — the substrate holds a read lock across `deliver`),
//! so an `UnsafeCell` with a blanket `Sync` impl is sound *provided
//! the consumer macro is the only caller*. The `aether-component
//! ::export!` macro orchestrates `set` / `get_mut` from within
//! `init` / `receive` shims that the substrate serializes.

/// Macro-use backing store for the one component instance per guest.
use core::cell::UnsafeCell;
pub struct Slot<C> {
    inner: UnsafeCell<Option<C>>,
}

impl<C> Slot<C> {
    /// Build an empty slot. `const` so it can live in a `static`.
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(None),
        }
    }

    /// # Safety
    /// Caller must guarantee no aliasing access. Intended to be called
    /// exactly once, from within the `init` shim, before any other
    /// access.
    pub unsafe fn set(&self, value: C) {
        // SAFETY: caller upholds the `# Safety` contract above — no
        // other live reference to the cell. The single-threaded wasm
        // guest + serialized FFI shim path means there is no race; the
        // dereference of the `UnsafeCell` raw pointer is a unique-
        // access write because the only other caller (`get_mut`) is
        // gated behind the same caller-side serialization.
        unsafe {
            *self.inner.get() = Some(value);
        }
    }

    /// # Safety
    /// Caller must guarantee no aliasing access. Intended to be called
    /// from within the `receive` shim, after `init` has completed.
    // Returning `&mut C` from `&self` is the load-bearing pattern
    // here — the `UnsafeCell` makes this sound under the substrate's
    // serialized-dispatch guarantee. Clippy's `mut_from_ref` lint
    // catches this as a footgun in general; we're the exception the
    // lint is designed around.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_mut(&self) -> Option<&mut C> {
        // SAFETY: caller upholds the `# Safety` contract above — no
        // other live reference to the cell. The dispatcher serializes
        // `receive` against `init`/itself, so the `&mut` derived from
        // the `UnsafeCell` is never aliased.
        unsafe { (*self.inner.get()).as_mut() }
    }
}

impl<C> Default for Slot<C> {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: single-threaded WASM + serialized FFI entry points mean the
// `UnsafeCell` is only ever touched from one thread at a time. The
// `Sync` impl unlocks `static SLOT: Slot<MyComponent>` without
// needing `std::sync` types the `no_std` surface can't provide. No
// concurrent access is possible inside a single wasm linear memory.
unsafe impl<C> Sync for Slot<C> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_set_then_get_mut_returns_value() {
        let slot: Slot<u32> = Slot::new();
        // SAFETY: test thread holds the only reference to `slot`;
        // no aliasing access exists.
        unsafe {
            slot.set(42);
        }
        // SAFETY: `set` has completed and returned; no other reference
        // to the cell is live on this test thread.
        let got = unsafe { slot.get_mut() };
        assert_eq!(got.copied(), Some(42));
    }

    #[test]
    fn slot_get_mut_before_set_is_none() {
        let slot: Slot<u32> = Slot::new();
        // SAFETY: test thread holds the only reference to `slot`;
        // no aliasing access exists.
        let got = unsafe { slot.get_mut() };
        assert!(got.is_none());
    }
}
