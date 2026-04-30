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

use aether_hub_protocol::{ClaudeAddress, EngineMailFrame, EngineToHub};
use aether_kinds::SubstrateDying;

use crate::hub_client::HubOutbound;

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
        outbound.send(EngineToHub::Mail(EngineMailFrame {
            address: ClaudeAddress::Broadcast,
            kind_name: <SubstrateDying as aether_data::Kind>::NAME.to_owned(),
            payload,
            origin: None,
            correlation_id: 0,
        }));
    }

    // Drain whatever's in the capture ring onto the engine TCP
    // before we go. The 250 ms background flusher in `log_capture`
    // can't be relied on during abort — we exit the process below
    // and the flusher's loop never sees the next tick.
    crate::log_capture::flush_now();

    std::process::exit(FATAL_EXIT_CODE);
}
