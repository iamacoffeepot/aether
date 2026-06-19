//! Host current-slots backend for `aether_actor::Local`
//! (iamacoffeepot/aether#2070).
//!
//! The substrate multiplexes many actors over a worker pool (ADR-0087), so a
//! free `Local::with` call must resolve *which* actor the current thread is
//! serving. The dispatcher trampoline stamps a `*const ActorSlots` into a
//! thread-local around each handler call (and around `init`) via
//! [`with_stamped`]; the installed provider reads that stamp.
//!
//! The storage type ([`ActorSlots`]) lives in `aether-actor` and is shared —
//! the same type the guest holds in a `static`. Only this routing layer
//! (the `thread_local!`, the stamp, the RAII restore) is host-specific, and
//! it installs itself as the process [`aether_actor::local::SlotsProvider`]
//! once at boot, beside `log_install::init_subscriber()`.

use core::cell::Cell;
use core::ptr;
use std::sync::Once;

use aether_actor::local::{ActorSlots, install_slots_provider};

thread_local! {
    /// The current actor's slot map for this worker thread, stamped by
    /// [`with_stamped`] for the span of a handler call. Null between
    /// dispatches (host-code: boot, scheduler, panic hook) — the provider
    /// reports `None` then, so `Local::try_with` returns `None` and
    /// `Local::with` panics.
    static CURRENT_SLOTS: Cell<*const ActorSlots> = const { Cell::new(ptr::null()) };
}

/// RAII guard restoring the prior `CURRENT_SLOTS` value on drop. Covers a
/// panic from the wrapped closure so the stamp doesn't leak a dangling
/// pointer past a panicking handler.
struct StampGuard {
    prev: *const ActorSlots,
}

impl Drop for StampGuard {
    fn drop(&mut self) {
        CURRENT_SLOTS.set(self.prev);
    }
}

/// Stamp `slots` as the current actor's slot map for the duration of `f`.
/// The chassis dispatcher trampoline calls this around each handler dispatch
/// (and around `init`); the pointer is restored to its prior value (almost
/// always null) before this returns, via the guard's drop — so a panicking
/// handler still restores cleanly.
pub fn with_stamped<R>(slots: &ActorSlots, f: impl FnOnce() -> R) -> R {
    // Stamping implies the host backend is live — ensure the provider is
    // installed so a `Local` access inside `f` resolves. Idempotent and
    // cheap after the first call (a `Once` acquire-load); boot calls
    // `install()` before any dispatch, so in production this is already done.
    install();
    let _guard = StampGuard {
        prev: CURRENT_SLOTS.replace(ptr::from_ref(slots)),
    };
    f()
}

/// The provider the substrate installs: read the thread-local stamp, or
/// `None` when no actor is stamped on this thread.
fn host_lookup() -> Option<*const ActorSlots> {
    let p = CURRENT_SLOTS.get();
    if p.is_null() { None } else { Some(p) }
}

/// Install the host thread-local backend as the process-global `Local`
/// provider. Called at boot before any actor dispatch, and ensured by every
/// [`with_stamped`]. Gated by a `Once` so the install runs exactly once and
/// later calls are a single acquire-load.
pub fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // SAFETY: `host_lookup` returns the thread-local stamp, which is
        // valid exactly for the stamped scope — the only time a `Local`
        // access reads it. Outside a stamp it returns `None` and nothing is
        // dereferenced.
        unsafe {
            install_slots_provider(host_lookup);
        }
    });
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test scaffolding — the spawned thread holds no settlement contract
mod tests {
    use super::*;
    use aether_actor::Local;
    use std::boxed::Box;
    use std::panic;
    use std::panic::AssertUnwindSafe;
    use std::string::String;
    use std::thread;

    #[derive(Default)]
    struct Probe(u64);
    impl Local for Probe {}

    #[derive(Default)]
    struct ProbeStr(String);
    impl Local for ProbeStr {}

    fn ensure_installed() {
        install();
    }

    #[test]
    fn per_actor_isolation() {
        ensure_installed();
        let a = ActorSlots::new();
        let b = ActorSlots::new();
        with_stamped(&a, || Probe::with_mut(|p| p.0 = 7));
        with_stamped(&b, || Probe::with_mut(|p| p.0 = 11));
        with_stamped(&a, || Probe::with(|p| assert_eq!(p.0, 7)));
        with_stamped(&b, || Probe::with(|p| assert_eq!(p.0, 11)));
    }

    #[test]
    fn stamp_restores_prior_on_panic() {
        ensure_installed();
        let slots = ActorSlots::new();
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            with_stamped(&slots, || panic!("handler trapped"));
        }));
        assert!(outcome.is_err(), "inner panic propagates");
        // After the panicking stamp, no actor is stamped on this thread, so
        // `try_with` reports `None` rather than seeing a stale pointer.
        assert!(Probe::try_with(|p| p.0).is_none());
    }

    #[test]
    fn try_with_is_none_outside_a_stamp() {
        ensure_installed();
        assert!(ProbeStr::try_with(|p| p.0.clone()).is_none());
    }

    #[test]
    fn slots_follow_box_across_threads() {
        // The binding is to the actor (its `Box<ActorSlots>`), not the
        // thread: a migrated actor's data is still reachable. ADR-0087's
        // work-stealing moves an actor between workers, so this must hold.
        ensure_installed();
        let slots = Box::new(ActorSlots::new());
        with_stamped(&slots, || Probe::with_mut(|p| p.0 = 42));
        let observed = thread::spawn(move || with_stamped(&slots, || Probe::with(|p| p.0)))
            .join()
            .expect("worker thread joined cleanly");
        assert_eq!(observed, 42, "Local data follows the Box, not the thread");
    }
}
