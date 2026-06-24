//! `aether.test_bench` cap on the test-bench chassis (issue 603 Phase 4).
//!
//! Test-bench is the only chassis that drives ticks via mail rather
//! than a frame loop — `aether.test_bench.advance { ticks }` runs N
//! cycles and replies once they complete. The cap claims the
//! `aether.test_bench` mailbox and dispatches `Advance` by pushing a
//! `ChassisEvent::Advance` onto the embedder's event channel; the
//! embedder's `run_frame` loop processes the event and replies via
//! outbound when the requested ticks finish.
//!
//! Companion: `aether-capabilities::UnsupportedTestBenchCapability`
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

// `EventSender` is a bundle-local channel sender (not an
// `aether_substrate` type), so the always-on config carries it at file
// root.
use crate::test_bench::events::EventSender;

/// Configuration for [`TestBenchCapability`]. Carries the
/// `EventSender` the embedder loop reads on, so the handler can hand
/// the embedder a request + reply target. Always-on at file root — it
/// names no `aether_substrate` type.
pub struct TestBenchCapConfig {
    pub events: EventSender,
}

/// `aether.test_bench` cap **identity** (ADR-0122 identity/runtime
/// split). A ZST carrying only the addressing — `Addressable`
/// (`NAMESPACE`, `Resolver`), the per-handler `HandlesKind` markers, and
/// the name-inventory entry, all emitted always-on by `#[actor]`. The
/// state-bearing runtime (`TestBenchCapabilityState`, which holds the
/// `aether_substrate`-typed `HubOutbound`) lives behind the one
/// `feature = "runtime"` gate.
pub struct TestBenchCapability;

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

#[actor(singleton)]
impl NativeActor for TestBenchCapability {
    type State = TestBenchCapabilityState;

    type Config = TestBenchCapConfig;

    const NAMESPACE: &'static str = "aether.test_bench";

    fn init(
        config: TestBenchCapConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<TestBenchCapabilityState, BootError> {
        let outbound = ctx.mailer().outbound().cloned().ok_or_else(|| {
            BootError::Other(Box::new(io::Error::other(
                "HubOutbound must be wired on Mailer before \
                 TestBenchCapability::init (test-bench attaches its loopback before \
                 the Builder chain)",
            )))
        })?;
        Ok(TestBenchCapabilityState {
            events: config.events,
            outbound,
        })
    }

    /// Push `ChassisEvent::Advance` onto the embedder loop. If the
    /// receiver is gone (chassis shutting down) reply `Err` inline
    /// so the caller doesn't hang.
    #[handler]
    fn on_advance(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: Advance) {
        let sender = ctx.reply_target();
        if state
            .events
            .send(ChassisEvent::Advance {
                reply_to: sender,
                ticks: mail.ticks,
            })
            .is_err()
        {
            state.outbound.send_reply(
                sender,
                &AdvanceResult::Err {
                    error: "test-bench chassis shutting down — advance aborted".to_owned(),
                },
            );
        }
    }
}

// The runtime half — the whole `aether_substrate`-typed surface (imports,
// `TestBenchCapabilityState`) — gated once here. The `#[actor] impl`
// above reaches it through the `use runtime::*` glob, so the items the
// impl names are re-exported with `pub use`.
#[cfg(feature = "runtime")]
mod runtime {
    use super::EventSender;
    use std::sync::Arc;

    pub use crate::test_bench::events::ChassisEvent;
    pub use aether_kinds::AdvanceResult;
    pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    pub use aether_substrate::chassis::error::BootError;
    pub use aether_substrate::mail::outbound::HubOutbound;
    pub use std::io;

    /// `aether.test_bench` runtime state (ADR-0122 split). Holds the
    /// embedder event channel the handler pushes onto plus the
    /// `HubOutbound` it replies through when the channel is gone. The
    /// dispatcher holds this as the cap's state; the addressing identity
    /// is the distinct ZST `TestBenchCapability`.
    pub struct TestBenchCapabilityState {
        pub(super) events: EventSender,
        pub(super) outbound: Arc<HubOutbound>,
    }
}
