//! Substrate lifecycle helpers (ADR-0063).
//!
//! `fatal_abort` is the chassis-facing exit path for abnormal
//! component lifecycle events: a wasm trap or host panic during
//! `deliver`, or a `drain_with_budget` returning `Wedged`. The
//! function logs the abort reason, emits a final `SubstrateDying`
//! broadcast to attached hub sessions, synchronously flushes the
//! capture ring (so the abort log lands in `engine_logs`), and
//! exits the process with code `2`.
//!
//! The function is `-> !`. It does not unwind — by the time we're
//! here we've already decided the substrate is going down, and any
//! caller-side cleanup would race the hub's reaping of the engine
//! anyway.
//!
//! [`FatalAborter`] is the indirection that lets call sites that
//! don't naturally hold a [`HubOutbound`] (the cross-class wait_reply
//! guard in [`crate::native_transport`], future ADR-0074 §Decision-7
//! checks) request an abort without plumbing outbound through every
//! layer. Production chassis construct an [`OutboundFatalAborter`];
//! tests use [`PanicAborter`] so a misuse panics the test thread
//! instead of `process::exit`-ing the test runner.

use std::sync::Arc;

use aether_kinds::SubstrateDying;

use crate::outbound::HubOutbound;

/// Process exit code on fatal abort. Distinct from `0` (clean exit)
/// and `1` (which Rust uses for panics from `main`).
pub const FATAL_EXIT_CODE: i32 = 2;

/// Emit a final `SubstrateDying` broadcast and exit the process. The
/// reason string is what lands in `engine_logs` — make it specific
/// enough that an operator reading the logs knows what triggered the
/// abort (e.g. `"component died: <kind> ..."` vs. `"dispatcher
/// wedged: mailbox=... waited=5s"`).
///
/// The broadcast uses `outbound` directly (bypassing the mailer) so
/// it works even when the abort cause is an in-mailer wedge. Send is
/// best-effort: a closed connection silently drops the frame, which
/// is the right disposition during abort.
pub fn fatal_abort(outbound: &HubOutbound, reason: String) -> ! {
    tracing::error!(
        target: "aether_substrate::lifecycle",
        reason = %reason,
        "substrate fatal abort",
    );

    // SubstrateDying carries the same reason; postcard-encode and
    // ship as a broadcast frame on the engine TCP. Encoding is
    // infallible for `String`-only structs; the `if let` is just
    // defensive against a future schema change.
    if let Ok(payload) = postcard::to_allocvec(&SubstrateDying {
        reason: reason.clone(),
    }) {
        outbound.egress_broadcast(
            <SubstrateDying as aether_data::Kind>::NAME,
            payload,
            None,
            0,
        );
    }

    // Drain whatever's in the capture ring onto the engine TCP
    // before we go. The 250 ms background flusher in `log_capture`
    // can't be relied on during abort — we exit the process below
    // and the flusher's loop never sees the next tick.
    crate::log_capture::flush_now();

    std::process::exit(FATAL_EXIT_CODE);
}

/// Indirection over [`fatal_abort`] for call sites that don't
/// naturally hold a [`HubOutbound`]. The chassis injects one of these
/// into [`crate::ChassisCtx`]; capabilities thread it into their
/// [`crate::NativeTransport`] so the cross-class `wait_reply` guard
/// (ADR-0074 §Decision 5) can abort without each capability needing
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
/// chassis's [`HubOutbound`] so the abort emits a final
/// `SubstrateDying` broadcast before `process::exit`. Constructed by
/// chassis drivers (desktop, headless) that already own outbound.
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
/// chassis built without an explicit aborter (tests, the TestBench
/// in-process driver) so an abort surfaces as a panic the harness
/// catches.
pub struct PanicAborter;

impl FatalAborter for PanicAborter {
    fn abort(&self, reason: String) -> ! {
        panic!("aether-substrate fatal abort: {reason}");
    }
}
