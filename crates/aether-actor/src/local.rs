//! `Local` — ambient per-actor type-keyed storage over an injected backend.
//! Issue 582; backend inversion iamacoffeepot/aether#2070.
//!
//! `Local` gives an actor ambient, type-keyed, per-actor scratch storage
//! reachable from a *free function* — no `ctx` threading. That ambient
//! reach is the whole point: the first consumers are framework subsystems
//! that hold no actor reference on their call stack — ADR-0081's per-actor
//! log/trace rings (pushed into by the host tracing subscriber) and the
//! per-handler cost EWMA. A consumer writes `struct AppLog(Vec<u8>); impl
//! Local for AppLog {}` and reaches its slot with `AppLog::with_mut(|l|
//! …)`; `TypeId<Self>` is the storage key, so distinct logical storages are
//! distinct types, structurally.
//!
//! Two pieces, split so neither names a target:
//!
//! - [`ActorSlots`] is the storage primitive — a `TypeId`-keyed slot map,
//!   target-blind and `no_std`. It is the same type whether the host owns
//!   one per actor on the heap or a single-actor image holds one in a
//!   `static`.
//! - *Which* actor's slots a free `Local::with` call resolves is answered
//!   by an injected [`SlotsProvider`] the **driver** installs once, via
//!   [`install_slots_provider`]. The host (`aether-substrate`) installs a
//!   `thread_local!`-routed provider at boot, because it multiplexes many
//!   actors over a worker pool and must resolve *which* actor a given
//!   thread is serving. A single-actor image (the wasm guest, where the
//!   linear memory *is* the actor) installs the turnkey [`install_static_backend`]
//!   below — there is one actor, so a `static` slot map is unambiguous and
//!   no routing is needed.
//!
//! This mirrors the substrate's existing injection seams: `InlineRegistry`
//! (a guest-owned `static` the `export!` macro creates) and `MailboxWakeSlot`
//! (a host-installed hook whose hot-path read is a single relaxed load). The
//! provider read here is the same shape — an `Acquire` load plus an indirect
//! call ahead of the storage op.
//!
//! Single-threaded-per-actor (ADR-0038) is what makes the inner `RefCell`
//! sound on every target: concurrent borrows of the same `Self` panic via
//! `RefCell::borrow_mut` ("already borrowed"), a free runtime check covering
//! a cross-actor leak, a dispatcher bug, and recursion from the same handler.
//! Before any provider is installed, or when the provider reports no actor
//! in scope, `with` / `with_mut` panic ("Local accessed outside actor
//! dispatch") and `try_with` / `try_with_mut` return `None` — the host-code
//! path (substrate boot, the panic hook, the actor-aware tracing layer).

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use core::any::{Any, TypeId};
use core::cell::RefCell;
use core::mem::transmute;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

/// Per-actor type-keyed slot map — the storage primitive, target-blind.
///
/// `RefCell` (not `Mutex`) is sound because an actor is single-threaded at
/// any instant (ADR-0038), so nothing concurrent reaches a slot; the borrow
/// panic is a feature, catching reentrancy. `BTreeMap` rather than `HashMap`
/// keeps the crate `no_std` without pulling `hashbrown` and gives a `const`
/// constructor so a single-actor image can hold one in a `static`. Each slot
/// stores `Box<RefCell<T>>` erased as `Box<dyn Any + Send>`: `Send` so a
/// host slot map can move with its actor between worker threads (ADR-0087);
/// trivially satisfied on the single-threaded guest.
pub struct ActorSlots {
    by_type: RefCell<BTreeMap<TypeId, Box<dyn Any + Send>>>,
}

impl ActorSlots {
    /// Construct an empty slot map. `const` so the single-actor static
    /// backend can initialize one with no runtime cost.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            by_type: RefCell::new(BTreeMap::new()),
        }
    }

    /// Pre-insert a constructed `T` so the first `with` / `with_mut::<T>`
    /// observes it instead of `T::default()`. The chassis spawn path seeds
    /// each actor's log / trace rings at their configured capacity here.
    /// Overwrites any existing slot — a boot-time op that runs before any
    /// handler dispatch, so there is never a live value to clobber.
    pub fn seed<T: Default + Send + 'static>(&self, value: T) {
        self.by_type.borrow_mut().insert(
            TypeId::of::<T>(),
            Box::new(RefCell::new(value)) as Box<dyn Any + Send>,
        );
    }

    /// Resolve (or lazily insert) the per-type `RefCell<T>` slot and return
    /// a raw pointer to it.
    ///
    /// The outer map borrow is released before the pointer is used so a
    /// nested `with_mut::<U>` on a different type can re-enter the map. The
    /// pointer stays valid across that re-entry because the `Box<RefCell<T>>`
    /// is heap-pinned — a `BTreeMap` rebalance moves the `Box` header in the
    /// node, not the pointed-to `RefCell<T>`.
    fn slot_ptr_for<T: Default + Send + 'static>(&self) -> *const RefCell<T> {
        let mut map = self.by_type.borrow_mut();
        let entry = map
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(RefCell::new(T::default())) as Box<dyn Any + Send>);
        ptr::from_ref::<RefCell<T>>(
            entry
                .downcast_ref::<RefCell<T>>()
                .expect("TypeId<T> ⇒ RefCell<T>"),
        )
    }

    fn with_mut<T, R>(&self, f: impl FnOnce(&mut T) -> R) -> R
    where
        T: Default + Send + 'static,
    {
        let cell_ptr = self.slot_ptr_for::<T>();
        // SAFETY: pointer into a heap-pinned `Box<RefCell<T>>` owned by
        // `self.by_type`. The outer map borrow released at the end of
        // `slot_ptr_for`, so a nested `with_mut::<U>` can re-enter the map;
        // the `&RefCell<T>` reborrow is unique for the closure's run.
        let cell = unsafe { &*cell_ptr };
        let mut borrow = cell.borrow_mut();
        f(&mut borrow)
    }

    fn with<T, R>(&self, f: impl FnOnce(&T) -> R) -> R
    where
        T: Default + Send + 'static,
    {
        let cell_ptr = self.slot_ptr_for::<T>();
        // SAFETY: same justification as `with_mut`.
        let cell = unsafe { &*cell_ptr };
        let borrow = cell.borrow();
        f(&borrow)
    }
}

impl Default for ActorSlots {
    fn default() -> Self {
        Self::new()
    }
}

/// Provider signature: yields the current actor's slots for the duration of
/// the call that reads it, or `None` when no actor is in scope (host-code
/// path). A non-capturing `fn` so it lives in an atomic install slot.
pub type SlotsProvider = fn() -> Option<*const ActorSlots>;

/// Process-global current-slots provider, installed once by the driver.
/// Null = not yet installed. Stored as the fn pointer behind an
/// `AtomicPtr` so the cell is `no_std`-safe and `Sync`; set once with
/// `Release`, read with `Acquire` — a single relaxed-class load on the
/// dispatch hot path.
static SLOTS_PROVIDER: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Install the process-global current-slots provider. Set-once: the first
/// install wins and later ones are ignored. Exactly one driver installs per
/// process — the host at boot, a single-actor guest at init.
///
/// # Safety
/// `provider` must return a pointer valid for the duration of the `Local`
/// access that reads it — the contract the host's stamped pointer and the
/// guest's `'static` backend both satisfy.
pub unsafe fn install_slots_provider(provider: SlotsProvider) {
    // A `fn` pointer casts cleanly to a thin data pointer; `current_slots`
    // transmutes it back (pointer → `fn` is not a plain cast).
    let raw: *mut () = provider as *mut ();
    let _ =
        SLOTS_PROVIDER.compare_exchange(ptr::null_mut(), raw, Ordering::Release, Ordering::Relaxed);
}

/// Read the installed provider. `None` before any install, or when the
/// provider reports no actor in scope.
#[inline]
fn current_slots() -> Option<*const ActorSlots> {
    let raw = SLOTS_PROVIDER.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: `SLOTS_PROVIDER` only ever holds a `SlotsProvider` cast to a
    // thin pointer by `install_slots_provider`; this reconstructs it. A data
    // pointer → `fn` pointer is not a plain cast, so `transmute` is required.
    let provider: SlotsProvider = unsafe { transmute::<*mut (), SlotsProvider>(raw) };
    provider()
}

/// Single-actor static backend: a process-`static` slot map for an image
/// that hosts exactly one actor in its address space (the wasm guest — the
/// linear memory *is* the actor). The `unsafe impl Sync` is licensed by that
/// single logical thread, the same argument behind `crate::Slot` and
/// `crate::ffi::inline::InlineRegistry`.
struct StaticBackend(ActorSlots);

// SAFETY: a single-actor image runs on one logical thread (the wasm linear
// memory is the actor), so this static can never be racily aliased.
unsafe impl Sync for StaticBackend {}

static STATIC_SLOTS: StaticBackend = StaticBackend(ActorSlots::new());

/// Install the single-actor static backend as the current-slots provider.
/// Called from a single-actor image's init prologue (the `export!` guest
/// runtime). A multi-actor host installs its own `thread_local!`-routed
/// backend instead and never calls this.
pub fn install_static_backend() {
    // SAFETY: `STATIC_SLOTS` is `'static`, so the returned pointer is valid
    // for any access — `install_slots_provider`'s contract is satisfied.
    unsafe {
        install_slots_provider(|| Some(ptr::from_ref(&STATIC_SLOTS.0)));
    }
}

/// Per-actor scratch storage, type-keyed.
///
/// Implement `Local` on a named newtype to claim a slot: `TypeId<Self>` is
/// the storage key, so `struct AppLog(Vec<u8>);` and `struct
/// AuditLog(Vec<u8>);` get independent slots because their `TypeId`s differ —
/// distinct logical storage = distinct type, structurally.
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
/// Lazy-initialized via `Default::default()` on first access. Concurrent
/// borrows of the same `Self` panic via the inner `RefCell`; concurrent
/// borrows of *different* `Local` types succeed (each type is its own slot).
///
/// Outside an actor (no provider installed, or the provider reports no actor
/// in scope), `with` / `with_mut` panic ("Local accessed outside actor
/// dispatch"); `try_with` / `try_with_mut` return `None`.
///
/// `Send` so a host slot map can move between the chassis builder thread
/// (during `init`) and the dispatcher thread (during handler dispatch);
/// trivially met on the single-threaded guest.
pub trait Local: Default + Send + 'static {
    /// Borrow this actor's instance of `Self` immutably.
    fn with<R>(f: impl FnOnce(&Self) -> R) -> R {
        let slots = current_slots().expect("Local accessed outside actor dispatch");
        // SAFETY: the installed provider guarantees the pointer is valid for
        // the duration of this call.
        let slots = unsafe { &*slots };
        slots.with::<Self, R>(f)
    }

    /// Borrow this actor's instance of `Self` mutably. Lazily initialized via
    /// `Default::default()` on first access.
    fn with_mut<R>(f: impl FnOnce(&mut Self) -> R) -> R {
        let slots = current_slots().expect("Local accessed outside actor dispatch");
        // SAFETY: see `with`.
        let slots = unsafe { &*slots };
        slots.with_mut::<Self, R>(f)
    }

    /// Like [`Self::with`] but returns `None` when no actor is in scope
    /// (host-code path: substrate boot, scheduler, panic hook). Lets callers
    /// run on both sides of an actor boundary without panicking.
    fn try_with<R>(f: impl FnOnce(&Self) -> R) -> Option<R> {
        let slots = current_slots()?;
        // SAFETY: see `with`.
        let slots = unsafe { &*slots };
        Some(slots.with::<Self, R>(f))
    }

    /// Mutable variant of [`Self::try_with`]. Same semantics; lazily
    /// initializes via `Default::default()` on first in-actor access.
    fn try_with_mut<R>(f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        let slots = current_slots()?;
        // SAFETY: see `with`.
        let slots = unsafe { &*slots };
        Some(slots.with_mut::<Self, R>(f))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::local;
    use alloc::string::String;
    use core::cell::Cell;

    std::thread_local! {
        // The core tests drive storage through a host-shaped test provider:
        // a thread-local "current slots" pointer the test stamps, mirroring
        // the substrate's real `thread_local!` routing. The genuine host
        // backend (RAII stamp, panic-restore, cross-thread Box) is tested in
        // `aether-substrate`, where it lives.
        static TEST_CURRENT: Cell<*const ActorSlots> = const { Cell::new(ptr::null()) };
    }

    fn ensure_test_provider() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            // SAFETY: the test provider returns the thread-local stamp,
            // valid for the stamped scope; set-once for the test binary.
            unsafe {
                install_slots_provider(|| {
                    let p = TEST_CURRENT.get();
                    if p.is_null() { None } else { Some(p) }
                });
            }
        });
    }

    fn test_stamped<R>(slots: &ActorSlots, f: impl FnOnce() -> R) -> R {
        ensure_test_provider();
        let prev = TEST_CURRENT.replace(ptr::from_ref(slots));
        let out = f();
        TEST_CURRENT.set(prev);
        out
    }

    #[derive(Default)]
    #[local]
    struct Probe(u64);

    #[derive(Default)]
    #[local]
    struct OtherProbe(u64);

    #[derive(Default)]
    #[local]
    struct ProbeStr(String);

    #[test]
    fn per_actor_isolation() {
        // Two slot maps, one user type — each "actor" sees its own value;
        // the stamp routes the lookup into the current map.
        let a = ActorSlots::new();
        let b = ActorSlots::new();
        test_stamped(&a, || Probe::with_mut(|p| p.0 = 7));
        test_stamped(&b, || Probe::with_mut(|p| p.0 = 11));
        test_stamped(&a, || Probe::with(|p| assert_eq!(p.0, 7)));
        test_stamped(&b, || Probe::with(|p| assert_eq!(p.0, 11)));
    }

    #[test]
    fn seed_pre_inserts_value_seen_by_with_and_with_mut() {
        let slots = ActorSlots::new();
        slots.seed(Probe(0x5EED));
        test_stamped(&slots, || {
            Probe::with(|p| assert_eq!(p.0, 0x5EED, "seeded value, not default"));
            Probe::with_mut(|p| p.0 += 1);
            Probe::with(|p| assert_eq!(p.0, 0x5EEE));
        });
    }

    #[test]
    fn unseeded_type_still_lazily_defaults() {
        let slots = ActorSlots::new();
        slots.seed(Probe(0x5EED));
        test_stamped(&slots, || {
            OtherProbe::with(|p| assert_eq!(p.0, 0, "unseeded type defaults"));
        });
    }

    #[test]
    fn distinct_types_independent() {
        let slots = ActorSlots::new();
        test_stamped(&slots, || {
            Probe::with_mut(|p| p.0 = 42);
            ProbeStr::with_mut(|p| p.0.push_str("hello"));
            Probe::with(|p| assert_eq!(p.0, 42));
            ProbeStr::with(|p| assert_eq!(p.0, "hello"));
        });
    }

    #[test]
    fn nested_with_mut_disjoint_types_succeeds() {
        let slots = ActorSlots::new();
        test_stamped(&slots, || {
            Probe::with_mut(|a| {
                a.0 = 1;
                OtherProbe::with_mut(|b| b.0 = 2);
                assert_eq!(a.0, 1);
            });
            OtherProbe::with(|b| assert_eq!(b.0, 2));
        });
    }

    #[test]
    fn nested_with_mut_same_type_panics() {
        use std::panic;
        use std::panic::AssertUnwindSafe;
        let slots = ActorSlots::new();
        test_stamped(&slots, || {
            Probe::with_mut(|outer| {
                outer.0 = 1;
                let inner = panic::catch_unwind(AssertUnwindSafe(|| {
                    Probe::with_mut(|p| p.0 = 2);
                }));
                assert!(inner.is_err(), "nested same-type with_mut must panic");
            });
        });
    }

    #[test]
    fn try_with_returns_none_when_no_actor_in_scope() {
        ensure_test_provider();
        // No active stamp on this thread — the provider reports no actor.
        TEST_CURRENT.set(ptr::null());
        assert!(Probe::try_with(|p| p.0).is_none());
        assert!(Probe::try_with_mut(|p| p.0).is_none());
    }

    #[test]
    fn static_backend_routes_through_the_provider() {
        // `install_static_backend` is the single-actor install; once a
        // provider is set the test provider wins (set-once), so assert the
        // install call is a no-op rather than a panic and the API exists.
        install_static_backend();
        let slots = ActorSlots::new();
        test_stamped(&slots, || Probe::with_mut(|p| p.0 = 5));
        test_stamped(&slots, || Probe::with(|p| assert_eq!(p.0, 5)));
    }
}
