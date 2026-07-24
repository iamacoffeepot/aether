//! `aether.substrate_harness` cap stub for chassis without substrate-harness drive
//! (issue 603 Phase 4).
//!
//! Desktop and headless run their own frame loops rather than driving
//! ticks through `aether.substrate_harness.advance`, so they compose this cap
//! to fail-fast with `Err`-replies instead of letting the mail
//! warn-drop and hang the agent's await-reply slot.
//!
//! Companion: [`SubstrateHarnessCapability`](super::cap::SubstrateHarnessCapability)
//! claims the same mailbox on the substrate-harness chassis and dispatches
//! `Advance` for real. Both live here so the mailbox's two chassis
//! profiles read together — the same shape as
//! `RenderCapability` / `HeadlessRenderCapability`.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root because
// `#[actor]` emits `impl HandlesKind<K> for X {}` markers against the
// identity always-on, outside the `feature = "runtime"` gate.
use aether_kinds::Advance;

/// `aether.substrate_harness` cap **identity** on chassis without substrate-harness
/// drive (ADR-0122 identity/runtime split). A ZST carrying only the
/// addressing — `Addressable` (`NAMESPACE`, `Resolver`), the per-handler
/// `HandlesKind` markers, and the name-inventory entry, all emitted
/// always-on by `#[actor]`. Replies `AdvanceResult::Err` so MCP
/// `aether.substrate_harness.advance` mail fails fast instead of hanging on a
/// reply that never comes.
pub struct UnsupportedSubstrateHarnessCapability;

// The `#[actor]` / `#[handler]` attribute path stays always-on (the
// macro divides what it emits). Everything that names an
// `aether_substrate` type — the handler/init ctx, the runtime state —
// lives in the `runtime` module below, gated once by `feature =
// "runtime"`; the `#[actor] impl` reaches all of it through the single
// `use runtime::*` glob.
use aether_actor::actor;
#[cfg(feature = "runtime")]
#[allow(clippy::wildcard_imports)]
use runtime::*;

#[actor(singleton, root)]
impl NativeActor for UnsupportedSubstrateHarnessCapability {
    type State = UnsupportedSubstrateHarnessCapabilityState;

    type Config = ();

    /// ADR-0074 Phase 4 chassis-owned mailbox.
    const NAMESPACE: &'static str = "aether.substrate_harness";

    fn init(_config: (), ctx: &mut NativeInitCtx<'_>) -> Result<UnsupportedSubstrateHarnessCapabilityState, BootError> {
        let outbound = ctx.mailer().outbound().cloned().ok_or_else(|| {
            BootError::Other(Box::new(io::Error::other(
                "HubOutbound must be wired on Mailer before \
                 UnsupportedSubstrateHarnessCapability::init (chassis main connects the hub before \
                 the Builder chain)",
            )))
        })?;
        Ok(UnsupportedSubstrateHarnessCapabilityState { outbound })
    }

    /// Reply `Err` so MCP `advance` fails fast on chassis that don't
    /// drive ticks via the embedder loop.
    #[handler::single]
    fn on_advance(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: Advance) {
        state.outbound.send_reply(
            ctx.reply_target(),
            &AdvanceResult::Err {
                error: "unsupported on this chassis — aether.substrate_harness.advance is \
                    substrate-harness-only (ADR-0067)"
                    .to_owned(),
            },
        );
    }
}

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `UnsupportedSubstrateHarnessCapabilityState`) — gated once here. The
// `#[actor] impl` above reaches it through the `use runtime::*` glob, so
// the items the impl names are re-exported with `pub use`.
#[cfg(feature = "runtime")]
mod runtime {
    use std::sync::Arc;

    pub use aether_kinds::AdvanceResult;
    pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    pub use aether_substrate::chassis::error::BootError;
    pub use aether_substrate::mail::outbound::HubOutbound;
    pub use std::io;

    /// Runtime state for `UnsupportedSubstrateHarnessCapability` (ADR-0122
    /// split). Holds the `HubOutbound` captured at `init`; read in
    /// `on_advance` to send the fail-fast reply. The dispatcher holds
    /// this as the cap's state; the addressing identity is the distinct
    /// ZST `UnsupportedSubstrateHarnessCapability`.
    pub struct UnsupportedSubstrateHarnessCapabilityState {
        pub(super) outbound: Arc<HubOutbound>,
    }
}
