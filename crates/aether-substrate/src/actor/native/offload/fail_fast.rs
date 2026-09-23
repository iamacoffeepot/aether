//! The fail-fast runner every sanctioned off-thread spawn wraps its body in.
//!
//! ADR-0063 treats a panic as a bug that stops the chassis. The scheduler
//! enforces that for a handler panic; a worker thread has no scheduler above
//! it, so without a catch its panic is logged by the panic hook and otherwise
//! passes silently, dropping whatever the thread owed. [`run_or_abort`] closes
//! that gap: it runs the body under `catch_unwind` and escalates a caught
//! panic through the chassis aborter the spawn site took before it spawned,
//! with the panic payload in the reason.

use std::panic::{self, AssertUnwindSafe};

use crate::runtime::lifecycle::FatalAborter;
use crate::runtime::panic_hook::payload_string;

/// Run `body`, returning its output. A panic in `body` is fatal: it logs once
/// and calls `aborter` with `"{site} panicked: {payload}"`, which diverges.
pub fn run_or_abort<O>(aborter: &dyn FatalAborter, site: &str, body: impl FnOnce() -> O) -> O {
    match panic::catch_unwind(AssertUnwindSafe(body)) {
        Ok(output) => output,
        Err(payload) => {
            let reason = format!("{site} panicked: {}", payload_string(payload.as_ref()));
            tracing::error!(
                target: "aether_substrate::actor::native::offload",
                reason = %reason,
                "off-thread worker caught a panic; escalating fatal abort",
            );
            aborter.abort(reason);
        }
    }
}
