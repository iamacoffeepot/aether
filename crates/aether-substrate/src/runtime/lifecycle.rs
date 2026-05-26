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

use std::sync::Arc;

use crate::mail::outbound::HubOutbound;
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
/// chassis built without an explicit aborter (tests, the `TestBench`
/// in-process driver) so an abort surfaces as a panic the harness
/// catches.
pub struct PanicAborter;

impl FatalAborter for PanicAborter {
    fn abort(&self, reason: String) -> ! {
        panic!("aether-substrate fatal abort: {reason}");
    }
}
