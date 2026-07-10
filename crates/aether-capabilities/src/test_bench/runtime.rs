//! The runtime half — the whole `aether_substrate`-typed surface (imports,
//! `UnsupportedTestBenchCapabilityState`, and the `#[runtime] impl`) for
//! `aether.test_bench` (ADR-0122 identity/runtime split). Compiled only
//! under `feature = "runtime"` (the `mod runtime;` declaration in the
//! parent carries the gate).

use super::{Advance, AdvanceResult, UnsupportedTestBenchCapability};
use aether_actor::runtime;

pub use std::io;
pub use std::sync::Arc;

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::outbound::HubOutbound;

/// Runtime state for `UnsupportedTestBenchCapability` (ADR-0122 split).
/// Holds the `HubOutbound` captured at `init`; read in `on_advance` to
/// send the fail-fast reply. Living in this private module keeps it
/// `pub`-enough to satisfy the `NativeActor::State` interface without
/// exposing it as crate-public API.
pub struct UnsupportedTestBenchCapabilityState {
    pub outbound: Arc<HubOutbound>,
}

#[runtime]
impl NativeActor for UnsupportedTestBenchCapability {
    /// Runtime state: the `HubOutbound` captured at `init` and used by
    /// `on_advance` to send the fail-fast reply (ADR-0122 split).
    type State = UnsupportedTestBenchCapabilityState;

    type Config = ();

    /// ADR-0074 Phase 4 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.test_bench";

    fn init(_config: (), ctx: &mut NativeInitCtx<'_>) -> Result<UnsupportedTestBenchCapabilityState, BootError> {
        let outbound = ctx.mailer().outbound().cloned().ok_or_else(|| {
            BootError::Other(Box::new(io::Error::other(
                "HubOutbound must be wired on Mailer before \
                 UnsupportedTestBenchCapability::init (chassis main connects the hub before \
                 the Builder chain)",
            )))
        })?;
        Ok(UnsupportedTestBenchCapabilityState { outbound })
    }

    /// Reply `Err` so MCP `advance` fails fast on chassis that don't
    /// drive ticks via the embedder loop.
    #[handler::single]
    fn on_advance(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: Advance) {
        state.outbound.send_reply(
            ctx.reply_target(),
            &AdvanceResult::Err {
                error: "unsupported on this chassis — aether.test_bench.advance is \
                    test-bench-only (ADR-0067)"
                    .to_owned(),
            },
        );
    }
}
