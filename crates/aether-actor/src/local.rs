//! `Local` — per-actor scratch storage, type-keyed.
//! Issue 582.
//!
//! Storage is *semantically mailbox-keyed*: the chassis dispatcher
//! trampoline stamps a `*const ActorSlots` into TLS via
//! [`with_stamped`] before each handler call (and around `init`),
//! and `with` / `with_mut` read the stamped pointer to find the
//! current actor's storage. Within an actor's storage the lookup
//! is keyed on `TypeId<Self>` — distinct logical storages map to
//! distinct types. This is the convention: `struct AppLog(Vec<u8>);
//! struct AuditLog(Vec<u8>);` get independent slots because their
//! `TypeId`s differ. The type system makes "I want distinct
//! storage" a structural fact rather than a runtime convention.
//!
//! TLS is a per-handler-call routing pointer, not the data:
//! `ActorSlots` lives in a `Box` owned by the actor's dispatcher
//! closure for the actor's lifetime. If the scheduler shape ever
//! changed and an actor migrated between threads, the `Box` would
//! transfer with the actor, the new thread would `with_stamped`
//! the same heap pointer, and `with_mut` callers would see the
//! same data — the binding is to the actor, not to the thread.
//! See `native_slots_follow_box_across_threads`.
//!
//! Single-threaded-per-actor (ADR-0038) is what makes `RefCell`
//! safe on both targets:
//!
//! - Concurrent borrows of the same `Self` panic via
//!   `RefCell::borrow_mut` ("already borrowed"). Free runtime
//!   check covering cross-actor leak, dispatcher bug, and
//!   recursive-from-the-same-handler.
//! - Native panics with `debug_assert!` if `with` / `with_mut`
//!   is called outside an active stamp (substrate boot code,
//!   init-before-stamp, etc.).
//!
//! First consumer: issue 581's per-actor log buffer (a named
//! `LogBuffer` newtype around `Vec<LogEvent>`), drained to
//! `LogCapability` at handler exit by an actor-aware composite
//! tracing subscriber.

#[cfg(target_arch = "wasm32")]
mod wasm {
    extern crate alloc;

    use alloc::boxed::Box;
    use alloc::collections::BTreeMap;
    use core::any::{Any, TypeId};
    use core::cell::{RefCell, UnsafeCell};

    /// Single-actor type-keyed storage in wasm linear memory. The
    /// `unsafe impl Sync` is justified by the structural single-
    /// thread of the wasm linear memory: every component runs on
    /// one logical thread inside its own linear memory, so the
    /// static can never be racily aliased across threads. Same
    /// loophole `WASM_TRANSPORT` uses.
    ///
    /// `BTreeMap` instead of `HashMap` because `BTreeMap::new()`
    /// is `const fn` and `aether-actor` is `no_std + alloc` — we
    /// don't pull in `hashbrown`. The map holds at most a handful
    /// of entries per actor in practice (one per `Local`-
    /// implementing type), so the log-N lookup cost is irrelevant.
    pub(super) struct WasmActorSlots {
        inner: UnsafeCell<RefCell<BTreeMap<TypeId, Box<dyn Any>>>>,
    }

    unsafe impl Sync for WasmActorSlots {}

    impl WasmActorSlots {
        const fn new() -> Self {
            Self {
                inner: UnsafeCell::new(RefCell::new(BTreeMap::new())),
            }
        }

        pub(super) fn with_mut<T, R>(&self, f: impl FnOnce(&mut T) -> R) -> R
        where
            T: Default + 'static,
        {
            // SAFETY: the wasm linear memory is single-threaded;
            // the static is reachable only from this actor's code.
            let map_cell = unsafe { &*self.inner.get() };
            let cell_ptr: *const RefCell<T> = {
                let mut map = map_cell.borrow_mut();
                let entry = map
                    .entry(TypeId::of::<T>())
                    .or_insert_with(|| Box::new(RefCell::new(T::default())) as Box<dyn Any>);
                entry
                    .downcast_ref::<RefCell<T>>()
                    .expect("TypeId<T> ⇒ RefCell<T>") as *const RefCell<T>
            };
            // SAFETY: stable heap pointer (Box never moves the
            // pointed-to RefCell when the BTreeMap rebalances);
            // we never remove entries; the outer borrow released
            // at end of the inner block so nested with_mut on a
            // different T re-enters the map fine.
            let cell = unsafe { &*cell_ptr };
            let mut borrow = cell.borrow_mut();
            f(&mut *borrow)
        }

        pub(super) fn with<T, R>(&self, f: impl FnOnce(&T) -> R) -> R
        where
            T: Default + 'static,
        {
            let map_cell = unsafe { &*self.inner.get() };
            let cell_ptr: *const RefCell<T> = {
                let mut map = map_cell.borrow_mut();
                let entry = map
                    .entry(TypeId::of::<T>())
                    .or_insert_with(|| Box::new(RefCell::new(T::default())) as Box<dyn Any>);
                entry
                    .downcast_ref::<RefCell<T>>()
                    .expect("TypeId<T> ⇒ RefCell<T>") as *const RefCell<T>
            };
            let cell = unsafe { &*cell_ptr };
            let borrow = cell.borrow();
            f(&*borrow)
        }
    }

    pub(super) static WASM_SLOTS: WasmActorSlots = WasmActorSlots::new();
}

#[cfg(not(target_arch = "wasm32"))]
mod native_impl {
    extern crate std;

    use core::any::{Any, TypeId};
    use core::cell::{Cell, RefCell};
    use std::boxed::Box;
    use std::collections::HashMap;

    /// Per-actor slot map. Owned as a `Box<ActorSlots>` by the
    /// chassis dispatcher closure for the actor's lifetime; the
    /// dispatcher stamps a `*const ActorSlots` into TLS via
    /// [`with_stamped`] for the duration of `init` and each
    /// handler call.
    ///
    /// `RefCell<HashMap>` is deliberate: the single-thread-per-
    /// actor invariant (ADR-0038) means no concurrent access can
    /// reach this, so `RefCell` suffices instead of `Mutex`. Each
    /// slot stores `Box<RefCell<T>>` erased as
    /// `Box<dyn Any + Send>` so concurrent borrows of the same
    /// slot panic via the inner cell while concurrent borrows of
    /// different slots succeed.
    ///
    /// Keyed on `TypeId<T>`. Two `struct AppLog(Vec<u8>);` and
    /// `struct AuditLog(Vec<u8>);` impls of `Local` get
    /// independent slots because their `TypeId`s differ.
    pub struct ActorSlots {
        by_type: RefCell<HashMap<TypeId, Box<dyn Any + Send>>>,
    }

    impl ActorSlots {
        /// Construct an empty slot map. The substrate's
        /// dispatcher trampoline calls this once per booted
        /// native actor.
        pub fn new() -> Self {
            Self {
                by_type: RefCell::new(HashMap::new()),
            }
        }

        pub(super) fn with_mut<T, R>(&self, f: impl FnOnce(&mut T) -> R) -> R
        where
            T: Default + Send + 'static,
        {
            let cell_ptr: *const RefCell<T> = {
                let mut map = self.by_type.borrow_mut();
                let entry = map
                    .entry(TypeId::of::<T>())
                    .or_insert_with(|| Box::new(RefCell::new(T::default())) as Box<dyn Any + Send>);
                entry
                    .downcast_ref::<RefCell<T>>()
                    .expect("TypeId<T> ⇒ RefCell<T>") as *const RefCell<T>
            };
            // SAFETY: the pointer is into a heap-allocated
            // `Box<RefCell<T>>` owned by `self.by_type`. The
            // outer borrow released at end of `cell_ptr` so a
            // nested `with_mut::<U>` can re-enter the map; the
            // box's heap address is stable (HashMap rehashes
            // move the `Box` header in the bucket, not the
            // pointed-to `RefCell<T>`); we never remove entries.
            let cell = unsafe { &*cell_ptr };
            let mut borrow = cell.borrow_mut();
            f(&mut *borrow)
        }

        pub(super) fn with<T, R>(&self, f: impl FnOnce(&T) -> R) -> R
        where
            T: Default + Send + 'static,
        {
            let cell_ptr: *const RefCell<T> = {
                let mut map = self.by_type.borrow_mut();
                let entry = map
                    .entry(TypeId::of::<T>())
                    .or_insert_with(|| Box::new(RefCell::new(T::default())) as Box<dyn Any + Send>);
                entry
                    .downcast_ref::<RefCell<T>>()
                    .expect("TypeId<T> ⇒ RefCell<T>") as *const RefCell<T>
            };
            let cell = unsafe { &*cell_ptr };
            let borrow = cell.borrow();
            f(&*borrow)
        }
    }

    impl Default for ActorSlots {
        fn default() -> Self {
            Self::new()
        }
    }

    std::thread_local! {
        static CURRENT_SLOTS: Cell<*const ActorSlots> = const { Cell::new(core::ptr::null()) };
    }

    /// RAII guard restoring the prior `CURRENT_SLOTS` value on
    /// drop. Built by [`with_stamped`]; covers panics from the
    /// wrapped closure so the TLS slot doesn't leak a dangling
    /// pointer past a panicking handler.
    struct StampGuard {
        prev: *const ActorSlots,
    }

    impl Drop for StampGuard {
        fn drop(&mut self) {
            CURRENT_SLOTS.with(|slot| slot.set(self.prev));
        }
    }

    /// Stamp `slots` as the current actor's slot map for the
    /// duration of `f`. The chassis dispatcher trampoline calls
    /// this around each handler dispatch (and around `init`); the
    /// pointer is restored to its prior value (almost always
    /// null) before this returns. Panics propagate out — the TLS
    /// slot is restored via the [`StampGuard`] drop, not via
    /// explicit unwind handling.
    pub fn with_stamped<R>(slots: &ActorSlots, f: impl FnOnce() -> R) -> R {
        let _guard = CURRENT_SLOTS.with(|slot| {
            let prev = slot.get();
            slot.set(slots as *const ActorSlots);
            StampGuard { prev }
        });
        f()
    }

    pub(super) fn with_current<R>(f: impl FnOnce(&ActorSlots) -> R) -> R {
        CURRENT_SLOTS.with(|slot| {
            let ptr = slot.get();
            debug_assert!(!ptr.is_null(), "Local accessed outside actor dispatch");
            // SAFETY: `with_stamped` only stamps a live
            // `&ActorSlots`; the StampGuard restores the prior
            // value before returning. Reading the pointer here
            // happens within the `with_stamped` call's stack
            // frame, so the slot pointee is still alive.
            let slots = unsafe { &*ptr };
            f(slots)
        })
    }

    /// Like [`with_current`] but tolerates "no actor stamped" by
    /// returning `None`. Used by callers that legitimately run on
    /// both sides of an actor boundary — issue 581's actor-aware
    /// tracing layer is the first consumer: in-actor → push to the
    /// `LogBuffer`; no actor → fall through to the host branch.
    pub(super) fn try_with_current<R>(f: impl FnOnce(&ActorSlots) -> R) -> Option<R> {
        CURRENT_SLOTS.with(|slot| {
            let ptr = slot.get();
            if ptr.is_null() {
                None
            } else {
                // SAFETY: same justification as `with_current` —
                // the stamp is live for the duration of the
                // surrounding `with_stamped`.
                let slots = unsafe { &*ptr };
                Some(f(slots))
            }
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native_impl::{ActorSlots, with_stamped};

/// Per-actor scratch storage, type-keyed.
///
/// Implement `Local` on a named newtype to claim a slot:
/// `TypeId<Self>` is the storage key. `struct AppLog(Vec<u8>);`
/// and `struct AuditLog(Vec<u8>);` get independent slots because
/// their `TypeId`s differ — distinct logical storage = distinct
/// type, structurally.
///
/// ```ignore
/// use aether_actor::Local;
///
/// #[derive(Default)]
/// struct AppLog(Vec<u8>);
/// impl Local for AppLog {}
///
/// fn append(byte: u8) {
///     AppLog::with_mut(|log| log.0.push(byte));
/// }
/// ```
///
/// Lazy-initialized via `Default::default()` on first access.
/// Concurrent borrows of the same `Self` panic via the inner
/// `RefCell`. Concurrent borrows of *different* `Local`
/// types succeed (each type is its own slot).
///
/// Native: outside an active [`with_stamped`] guard, `with` /
/// `with_mut` trip `debug_assert!` ("Local accessed outside
/// actor dispatch"). The chassis dispatcher trampoline opens that
/// guard around every handler call and around `init`.
///
/// `Send` (native only) so slot maps can move between the chassis
/// builder thread (during `init`) and the dispatcher thread
/// (during handler dispatch).
#[cfg(not(target_arch = "wasm32"))]
pub trait Local: Default + Send + 'static {
    /// Borrow this actor's instance of `Self` immutably.
    fn with<R>(f: impl FnOnce(&Self) -> R) -> R {
        native_impl::with_current(|slots| slots.with::<Self, R>(f))
    }

    /// Borrow this actor's instance of `Self` mutably. Lazily
    /// initialized via `Default::default()` on first access.
    fn with_mut<R>(f: impl FnOnce(&mut Self) -> R) -> R {
        native_impl::with_current(|slots| slots.with_mut::<Self, R>(f))
    }

    /// Issue #581: like [`Self::with`] but returns `None` when no
    /// actor is currently stamped (host-code path: substrate boot,
    /// scheduler, panic hook). Lets callers run on both sides of
    /// an actor boundary without panicking.
    fn try_with<R>(f: impl FnOnce(&Self) -> R) -> Option<R> {
        native_impl::try_with_current(|slots| slots.with::<Self, R>(f))
    }

    /// Mutable variant of [`Self::try_with`]. Same semantics: returns
    /// `None` when host-code; lazily initializes via `Default::default()`
    /// on first in-actor access.
    fn try_with_mut<R>(f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        native_impl::try_with_current(|slots| slots.with_mut::<Self, R>(f))
    }
}

#[cfg(target_arch = "wasm32")]
pub trait Local: Default + 'static {
    /// Borrow this actor's instance of `Self` immutably.
    fn with<R>(f: impl FnOnce(&Self) -> R) -> R {
        wasm::WASM_SLOTS.with::<Self, R>(f)
    }

    /// Borrow this actor's instance of `Self` mutably. Lazily
    /// initialized via `Default::default()` on first access.
    fn with_mut<R>(f: impl FnOnce(&mut Self) -> R) -> R {
        wasm::WASM_SLOTS.with_mut::<Self, R>(f)
    }

    /// Symmetric counterpart to the native `try_with`. Wasm linear
    /// memory is always "in actor" (the linear memory IS the actor),
    /// so this always succeeds — present for API symmetry with the
    /// native trait.
    fn try_with<R>(f: impl FnOnce(&Self) -> R) -> Option<R> {
        Some(wasm::WASM_SLOTS.with::<Self, R>(f))
    }

    /// Symmetric counterpart to native `try_with_mut`; always
    /// returns `Some` on wasm.
    fn try_with_mut<R>(f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        Some(wasm::WASM_SLOTS.with_mut::<Self, R>(f))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::local;
    use alloc::boxed::Box;
    use alloc::string::String;

    // Probe newtypes via the `#[local]` attribute — exercises
    // the macro and keeps the test bodies focused on the storage
    // semantics rather than boilerplate.
    #[cfg(not(target_arch = "wasm32"))]
    #[derive(Default)]
    #[local]
    struct Probe(u64);

    #[cfg(not(target_arch = "wasm32"))]
    #[derive(Default)]
    #[local]
    struct OtherProbe(u64);

    #[cfg(not(target_arch = "wasm32"))]
    #[derive(Default)]
    #[local]
    struct ProbeStr(String);

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_per_actor_isolation() {
        // Two ActorSlots, one user type — each "actor" sees its
        // own value. The TLS stamp routes the lookup into the
        // current actor's slots.
        let slots_a = ActorSlots::new();
        let slots_b = ActorSlots::new();

        with_stamped(&slots_a, || Probe::with_mut(|p| p.0 = 7));
        with_stamped(&slots_b, || Probe::with_mut(|p| p.0 = 11));

        with_stamped(&slots_a, || Probe::with(|p| assert_eq!(p.0, 7)));
        with_stamped(&slots_b, || Probe::with(|p| assert_eq!(p.0, 11)));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_distinct_types_independent() {
        // Two distinct user types, same ActorSlots, independent
        // slots keyed by TypeId.
        let slots = ActorSlots::new();
        with_stamped(&slots, || {
            Probe::with_mut(|p| p.0 = 42);
            ProbeStr::with_mut(|p| p.0.push_str("hello"));
            Probe::with(|p| assert_eq!(p.0, 42));
            ProbeStr::with(|p| assert_eq!(p.0, "hello"));
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_nested_with_mut_disjoint_types_succeeds() {
        // Different types ⇒ different TypeId slots ⇒ nested
        // borrow succeeds.
        let slots = ActorSlots::new();
        with_stamped(&slots, || {
            Probe::with_mut(|a| {
                a.0 = 1;
                OtherProbe::with_mut(|b| b.0 = 2);
                assert_eq!(a.0, 1);
            });
            OtherProbe::with(|b| assert_eq!(b.0, 2));
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_nested_with_mut_same_type_panics() {
        // Two re-entrant borrows of the *same* Local type
        // trip the inner RefCell — this is the recursion-guard
        // for hazards like a logging buffer that loops back into
        // itself.
        let slots = ActorSlots::new();
        with_stamped(&slots, || {
            Probe::with_mut(|outer| {
                outer.0 = 1;
                let inner = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    Probe::with_mut(|p| p.0 = 2)
                }));
                assert!(inner.is_err(), "nested same-type with_mut must panic");
            });
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    #[should_panic(expected = "Local accessed outside actor dispatch")]
    fn native_outside_stamp_panics_in_debug() {
        // No `with_stamped` wrapping — debug_assert! trips.
        Probe::with_mut(|p| p.0 += 1);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_stamp_restores_prior_on_panic() {
        // If a handler panics, the StampGuard drop must still
        // restore CURRENT_SLOTS so a subsequent stamped call
        // doesn't see a stale pointer. Verify by checking that
        // an out-of-stamp access *after* a panicking stamp still
        // trips debug_assert.
        let slots = ActorSlots::new();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_stamped(&slots, || panic!("handler trapped"));
        }));
        assert!(outcome.is_err(), "inner panic propagates");

        let outside = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Probe::with_mut(|p| p.0 = 1);
        }));
        assert!(
            outside.is_err(),
            "post-panic access must still panic via debug_assert"
        );
    }

    /// Issue 582: the binding is to the actor (via `Box<ActorSlots>`
    /// ownership), not to the thread. If a future scheduler
    /// migrated an actor between threads, the `Box` would transfer
    /// with it and the same data would still be reachable. Today's
    /// chassis pins one thread per actor (ADR-0038), but the API
    /// must not bake that in — this test asserts the design
    /// property explicitly so a future regression that ties data to
    /// thread identity (e.g., switching to true `thread_local!`)
    /// would fail loudly.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_slots_follow_box_across_threads() {
        let slots = Box::new(ActorSlots::new());

        // Thread A (this test thread): write 42 through the
        // stamped slots.
        with_stamped(&slots, || Probe::with_mut(|p| p.0 = 42));

        // Move the Box into a freshly-spawned thread and read
        // back — same slots, same data, despite the thread
        // boundary.
        let observed = std::thread::spawn(move || with_stamped(&slots, || Probe::with(|p| p.0)))
            .join()
            .expect("worker thread joined cleanly");

        assert_eq!(observed, 42, "Local data follows the Box, not the thread");
    }
}
