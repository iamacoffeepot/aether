//! `aether.test_bench` cap stub (issue 603 Phase 4).
//!
//! The test-bench chassis hosts a real `TestBenchCapability` (in
//! `aether-substrate-bundle::test_bench`) that dispatches `Advance`
//! by pushing to the embedder's event channel. Desktop and headless
//! don't drive ticks via `aether.test_bench.advance` — they have
//! their own frame loops — so they compose this cap to fail-fast
//! with `Err`-replies instead of letting the mail warn-drop and
//! hang the agent's await-reply slot.
//!
//! Mirrors the pattern from `HeadlessRenderCapability` /
//! `HeadlessWindowCapability`: same mailbox name across chassis,
//! cap variants per chassis profile.

// Handler-signature kinds resolve at file root — `#[actor]` emits the
// `impl HandlesKind<K> for X {}` markers always-on against the identity,
// outside the `feature = "runtime"` gate, so they reference these kinds
// from here. `AdvanceResult` is used in the `on_advance` handler body
// inside the `#[actor] impl` at file root.
use aether_kinds::{Advance, AdvanceResult};

use aether_actor::actor;

// The `runtime` module is this cap's private runtime-half namespace; the
// impl reaches all of it (state, ctx types, substrate imports) through
// this single seam, so the glob is intentional rather than a dozen
// one-line imports.
#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

/// Stub cap for `aether.test_bench` on chassis without test-bench drive
/// (desktop, headless). Replies `AdvanceResult::Err` so MCP
/// `aether.test_bench.advance` mail fails fast instead of hanging on a
/// reply that never comes (ADR-0122 identity/runtime split).
pub struct UnsupportedTestBenchCapability;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the macro
// divides what it emits). Everything that names an `aether_substrate` type —
// the handler/init ctx, the runtime state — lives in the `runtime` module
// below, gated once by `feature = "runtime"` and written cfg-free within;
// the `#[actor] impl` reaches all of it through the single `use runtime::*`
// glob above.
#[actor(singleton)]
impl NativeActor for UnsupportedTestBenchCapability {
    /// Runtime state: the `HubOutbound` captured at `init` and used by
    /// `on_advance` to send the fail-fast reply (ADR-0122 split).
    type State = UnsupportedTestBenchCapabilityState;

    type Config = ();

    /// ADR-0074 Phase 4 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.test_bench";

    fn init(
        _config: (),
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<UnsupportedTestBenchCapabilityState, BootError> {
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
    #[handler]
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

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `UnsupportedTestBenchCapabilityState`) — lives in this inline module, gated
// once here. The `#[actor] impl` above reaches it through the `use runtime::*`
// glob.
#[cfg(feature = "runtime")]
mod runtime {
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
        pub(super) outbound: Arc<HubOutbound>,
    }
}
