//! Substrate lifecycle helpers (ADR-0063).
//!
//! `fatal_abort` is the chassis-facing exit path for abnormal
//! component lifecycle events: a wasm trap or host panic during
//! `deliver`, or a `drain_with_budget` returning `Wedged`. The
//! function logs the abort reason, synchronously flushes the
//! per-actor capture buffers (so the abort log lands in
//! `engine_logs`), and exits the process with code `2`.
//!
//! Issue 775 retired the final `SubstrateDying` broadcast that
//! preceded `process::exit`: with `BroadcastCapability` gone the
//! chassis has no fan-out for the kind, so the abort relies on
//! the log capture path alone.
//!
//! The function is `-> !`. It does not unwind — by the time we're
//! here we've already decided the substrate is going down, and any
//! caller-side cleanup would race the hub's reaping of the engine
//! anyway.
//!
//! [`FatalAborter`] is the indirection that lets call sites that
//! don't naturally hold a [`HubOutbound`] (the wasm-trap abort path in
//! [`crate::actor::native::binding`], future ADR-0074 §Decision-7
//! checks) request an abort without plumbing outbound through every
//! layer. Production chassis construct an [`OutboundFatalAborter`];
//! tests use [`PanicAborter`] so a misuse panics the test thread
//! instead of `process::exit`-ing the test runner.

use std::sync::{Arc, Mutex, PoisonError};

use crate::mail::outbound::HubOutbound;
use crossbeam_channel::{Receiver, Sender};
use std::process;

/// Process exit code on fatal abort. Distinct from `0` (clean exit)
/// and `1` (which Rust uses for panics from `main`).
pub const FATAL_EXIT_CODE: i32 = 2;

/// Log the abort reason, flush per-actor log buffers, and exit the
/// process. The reason string is what lands in `engine_logs` — make
/// it specific enough that an operator reading the logs knows what
/// triggered the abort (e.g. `"component died: <kind> ..."` vs.
/// `"dispatcher wedged: mailbox=... waited=5s"`).
///
/// `_outbound` is kept on the signature because the [`FatalAborter`]
/// trait threads one through. Pre-#775 it carried the final
/// `SubstrateDying` broadcast; today the only sink that observed it
/// retired, and the parameter is unused at this call site.
// `reason` is owned because every call site constructs it via
// `format!(...)` directly — taking `&str` would force callers to
// either bind a `let s = format!(...); &s` first or stamp `&format!`
// at every site. The aborter consumes the value into a logged
// `%reason` tracing field; the diverging return means no further use.
#[allow(clippy::needless_pass_by_value)]
pub fn fatal_abort(_outbound: &HubOutbound, reason: String) -> ! {
    tracing::error!(
        target: "aether_substrate::lifecycle",
        reason = %reason,
        "substrate fatal abort",
    );

    // ADR-0081 retired the chassis-pushed flush hop. Each actor's
    // `ActorLogRing` lives in its own `ActorSlots`; the panic-hook
    // path (ADR-0081 §4 / P2) is the post-mortem dump surface — no
    // synchronous drain is needed here.

    process::exit(FATAL_EXIT_CODE);
}

/// Indirection over [`fatal_abort`] for call sites that don't
/// naturally hold a [`HubOutbound`]. The chassis injects one of these
/// into [`crate::ChassisCtx`]; capabilities thread it into their
/// [`crate::NativeBinding`] so the wasm-trap abort path
/// (ADR-0063) can abort without each capability needing
/// to plumb outbound itself.
///
/// Implementors must be `Send + Sync` so the aborter can be cloned
/// into capability dispatcher threads, and the [`Self::abort`] method
/// must be diverging — the chassis is going down, no caller-side
/// cleanup runs after.
pub trait FatalAborter: Send + Sync + 'static {
    fn abort(&self, reason: String) -> !;
}

/// Production [`FatalAborter`] backed by [`fatal_abort`]. Holds the
/// chassis's [`HubOutbound`] for symmetry with the trait; the
/// outbound itself is unused since issue 775 retired the
/// `SubstrateDying` broadcast.
pub struct OutboundFatalAborter {
    outbound: Arc<HubOutbound>,
}

impl OutboundFatalAborter {
    pub fn new(outbound: Arc<HubOutbound>) -> Self {
        Self { outbound }
    }
}

impl FatalAborter for OutboundFatalAborter {
    fn abort(&self, reason: String) -> ! {
        fatal_abort(&self.outbound, reason);
    }
}

/// Test [`FatalAborter`] that panics instead of `process::exit`-ing.
/// Lets a `#[should_panic]` test assert the cross-class guard fires
/// without taking down the whole test runner. Also the default for
/// chassis built without an explicit aborter (tests, the `SubstrateHarness`
/// in-process driver) so an abort surfaces as a panic the harness
/// catches.
pub struct PanicAborter;

impl FatalAborter for PanicAborter {
    fn abort(&self, reason: String) -> ! {
        panic!("aether-substrate fatal abort: {reason}");
    }
}

/// The first fatal abort reason seen on a chassis, plus a tripwire its
/// internal gates watch.
///
/// [`FatalAborter::abort`] is diverging, so the thread that aborts never
/// hands its reason to anyone: [`OutboundFatalAborter`] exits the process
/// with it, and [`PanicAborter`] unwinds the thread it fired on — usually
/// a scheduler pool worker, which is exactly the thread that would have
/// run the actor close cycles chassis teardown then waits for. The wait
/// is what converts a fully attributed handler panic into a bare timeout
/// at whatever ceiling truncates it first (iamacoffeepot/aether#4193,
/// mis-triaged as a bring-up stall on iamacoffeepot/aether#3752).
///
/// This record is the missing path from the aborting thread to the ones
/// that outlive it: [`RecordingAborter`] writes the reason here on the
/// way through, and a gate reads it instead of waiting out a budget for a
/// signal nothing is left to fire. It changes nothing about what
/// constitutes a fatal abort or when one fires — only what a later
/// observer can say about it.
///
/// The tripwire is a channel that never carries a value. The record holds
/// the only sender and drops it on the first abort, so every cloned
/// receiver observes the abort as a disconnect and a gate parked in a
/// `select!` wakes on it without polling.
pub struct FatalAbortRecord {
    reason: Mutex<Option<String>>,
    tripwire_tx: Mutex<Option<Sender<()>>>,
    tripwire_rx: Receiver<()>,
}

impl FatalAbortRecord {
    #[must_use]
    pub fn new() -> Self {
        let (tripwire_tx, tripwire_rx) = crossbeam_channel::bounded(0);
        Self { reason: Mutex::new(None), tripwire_tx: Mutex::new(Some(tripwire_tx)), tripwire_rx }
    }

    /// Record `reason` as the abort taking this chassis down and trip the
    /// wire. First write wins: one abort can cascade into others (a second
    /// worker picking up the same poisoned slot, the teardown gate routing
    /// its own wedge through the aborter), and the reader wants the one
    /// that started it, not the last echo.
    pub fn record(&self, reason: &str) {
        {
            let mut recorded = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
            if recorded.is_some() {
                return;
            }
            *recorded = Some(reason.to_owned());
        }

        // Only the thread that won the write above reaches here, so the
        // sole sender drops exactly once. Dropping it disconnects every
        // cloned tripwire, waking whatever gates are parked on one.
        drop(self.tripwire_tx.lock().unwrap_or_else(PoisonError::into_inner).take());
    }

    /// The recorded abort reason, or `None` while the chassis is healthy.
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        self.reason.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// A receiver that never yields a value and disconnects on the first
    /// abort — the wake a blocking gate selects on alongside its own
    /// signal.
    #[must_use]
    pub fn tripwire(&self) -> Receiver<()> {
        self.tripwire_rx.clone()
    }
}

impl Default for FatalAbortRecord {
    fn default() -> Self {
        Self::new()
    }
}

/// [`FatalAborter`] decorator that writes the reason into a
/// [`FatalAbortRecord`] before delegating to the chassis's real aborter.
/// Boot installs one over whatever aborter the builder was configured
/// with — [`OutboundFatalAborter`] in production, [`PanicAborter`] under
/// test and the substrate harness — so every abort path a chassis owns
/// (pool worker, capability dispatcher, wasm trap) leaves the same trace
/// for teardown to read.
pub struct RecordingAborter {
    inner: Arc<dyn FatalAborter>,
    record: Arc<FatalAbortRecord>,
}

impl RecordingAborter {
    #[must_use]
    pub fn new(inner: Arc<dyn FatalAborter>, record: Arc<FatalAbortRecord>) -> Self {
        Self { inner, record }
    }
}

impl FatalAborter for RecordingAborter {
    fn abort(&self, reason: String) -> ! {
        self.record.record(&reason);
        self.inner.abort(reason);
    }
}
