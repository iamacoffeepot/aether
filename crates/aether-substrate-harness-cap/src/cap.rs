//! `aether.substrate_harness` cap on the substrate-harness chassis (issue 603 Phase 4).
//!
//! Test-harness is the only chassis that drives ticks via mail rather
//! than a frame loop — `aether.substrate_harness.advance { ticks }` runs N
//! cycles and replies once they complete. The cap claims the
//! `aether.substrate_harness` mailbox and dispatches `Advance` by pushing a
//! `ChassisEvent::Advance` onto the embedder's event channel; the
//! embedder's `run_frame` loop processes the event and replies through the
//! request's retained inbound when the requested ticks finish.
//!
//! Companion: [`UnsupportedSubstrateHarnessCapability`](crate::unsupported_cap::UnsupportedSubstrateHarnessCapability)
//! claims the same mailbox on desktop / headless and replies `Err` so
//! agents fail fast. Mirrors the pattern from
//! `RenderCapability` / `HeadlessRenderCapability`.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root because
// `#[actor]` emits `impl HandlesKind<K> for X {}` markers against the
// identity always-on, outside the `feature = "runtime"` gate.
use aether_kinds::Advance;

// `EventSender` is a crate-local channel sender, but the events it carries
// hold an `aether_substrate::InboundMail` reply guard, so the channel and
// the params that hold it ride the `runtime` gate with the rest of the
// substrate-typed surface.
#[cfg(feature = "runtime")]
use crate::events::EventSender;

/// Composer-supplied params for [`SubstrateHarnessCapability`] (ADR-0156 §3
/// `Params` channel). Carries the `EventSender` the embedder loop reads on, so
/// the handler can hand the embedder a request + its inbound guard — construction
/// wiring, not an operator-resolvable knob.
#[cfg(feature = "runtime")]
pub struct SubstrateHarnessCapParams {
    pub events: EventSender,
}

/// `aether.substrate_harness` cap **identity** (ADR-0122 identity/runtime
/// split). A ZST carrying only the addressing — `Addressable`
/// (`NAMESPACE`, `Resolver`), the per-handler `HandlesKind` markers, and
/// the name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`SubstrateHarnessCapabilityState`, which holds the
/// `aether_substrate`-typed embedder channel) lives behind the one
/// `feature = "runtime"` gate.
pub struct SubstrateHarnessCapability;

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
impl NativeActor for SubstrateHarnessCapability {
    type State = SubstrateHarnessCapabilityState;

    // ADR-0156 §3: the embedder `EventSender` is construction wiring, not
    // operator config, so it rides the `Params` channel; `Config` is `()`.
    type Config = ();
    type Params = SubstrateHarnessCapParams;

    const NAMESPACE: &'static str = "aether.substrate_harness";

    fn init(
        (): (),
        params: SubstrateHarnessCapParams,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<SubstrateHarnessCapabilityState, BootError> {
        Ok(SubstrateHarnessCapabilityState { events: params.events })
    }

    /// Push `ChassisEvent::Advance` onto the embedder loop, carrying the
    /// retained inbound so the loop replies through it once the ticks
    /// complete. The guard reaches every sender kind — an rpc `Call` names
    /// the rpc server's mailbox, which the hub outbound drops (issue 6419) —
    /// and keeps the request's chain open until the reply is sent. If the
    /// receiver is gone (chassis shutting down) the send hands the event
    /// back and the handler replies `Err` through the recovered guard so the
    /// caller doesn't hang.
    #[handler::manual]
    fn on_advance(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, mail: Advance) {
        let event = ChassisEvent::Advance {
            reply: Box::new(ctx.take_inbound()),
            ticks: mail.ticks,
            delta_micros: mail.delta_micros,
        };
        // The handler sends only `Advance`, so a refused send hands back that
        // variant; `RenderMail` cannot come back and needs no arm.
        if let Err(mpsc::SendError(ChassisEvent::Advance { reply, .. })) = state.events.send(event) {
            reply.reply(&AdvanceResult::Err {
                error: "substrate-harness chassis shutting down — advance aborted".to_owned(),
            });
        }
    }
}

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `SubstrateHarnessCapabilityState`) — gated once here. The `#[actor] impl`
// above reaches it through the `use runtime::*` glob, so the items the
// impl names are re-exported with `pub use`.
#[cfg(feature = "runtime")]
mod runtime {
    use super::EventSender;

    pub use crate::events::ChassisEvent;
    pub use aether_actor::Manual;
    pub use aether_kinds::AdvanceResult;
    pub use aether_substrate::Erased;
    pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    pub use aether_substrate::chassis::error::BootError;
    pub use std::sync::mpsc;

    /// `aether.substrate_harness` runtime state (ADR-0122 split). Holds the
    /// embedder event channel the handler pushes onto; the reply rides the
    /// event's inbound guard, so nothing else is kept. The dispatcher holds
    /// this as the cap's state; the addressing identity is the distinct ZST
    /// `SubstrateHarnessCapability`.
    pub struct SubstrateHarnessCapabilityState {
        pub(super) events: EventSender,
    }
}
