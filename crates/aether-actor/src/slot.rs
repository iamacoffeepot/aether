//! Single-instance backing store for the macro-emitted `static`
//! component slot. WASM components are single-threaded per instance
//! (ADR-0010 §5 — the substrate holds a read lock across `deliver`),
//! so an `UnsafeCell` with a blanket `Sync` impl is sound *provided
//! the consumer macro is the only caller*. The `aether-component
//! ::export!` macro orchestrates `set` / `get_mut` from within
//! `init` / `receive` shims that the substrate serializes.

/// Macro-use backing store for the one component instance per guest.
pub struct Slot<C> {
    inner: core::cell::UnsafeCell<Option<C>>,
}

impl<C> Slot<C> {
    /// Build an empty slot. `const` so it can live in a `static`.
    pub const fn new() -> Self {
        Slot {
            inner: core::cell::UnsafeCell::new(None),
        }
    }

    /// # Safety
    /// Caller must guarantee no aliasing access. Intended to be called
    /// exactly once, from within the `init` shim, before any other
    /// access.
    pub unsafe fn set(&self, value: C) {
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
        unsafe { (*self.inner.get()).as_mut() }
    }
}

impl<C> Default for Slot<C> {
    fn default() -> Self {
        Slot::new()
    }
}

// Single-threaded WASM + serialized FFI entry points mean the
// `UnsafeCell` is only ever touched from one thread at a time. The
// `Sync` impl unlocks `static SLOT: Slot<MyComponent>` without
// needing `std::sync` types the `no_std` surface can't provide.
unsafe impl<C> Sync for Slot<C> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_set_then_get_mut_returns_value() {
        let slot: Slot<u32> = Slot::new();
        unsafe {
            slot.set(42);
        }
        let got = unsafe { slot.get_mut() };
        assert_eq!(got.copied(), Some(42));
    }

    #[test]
    fn slot_get_mut_before_set_is_none() {
        let slot: Slot<u32> = Slot::new();
        let got = unsafe { slot.get_mut() };
        assert!(got.is_none());
    }
}
