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
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::Advance;

// `TestBenchCapConfig` lives inside the bridge mod (carries the
// substrate-side `EventSender`); re-export at file root so callers
// don't have to reach into `native::`.
pub use native::TestBenchCapConfig;

#[aether_actor::bridge(singleton)]
mod native {
    use std::sync::Arc;

    use aether_actor::actor;
    use aether_kinds::AdvanceResult;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::outbound::HubOutbound;

    use super::Advance;
    use crate::test_bench::events::{ChassisEvent, EventSender};
    use std::io;

    /// Configuration for [`TestBenchCapability`]. Carries the
    /// `EventSender` the embedder loop reads on, so the handler can
    /// hand the embedder a request + reply target.
    pub struct TestBenchCapConfig {
        pub events: EventSender,
    }

    /// `aether.test_bench` cap on the test-bench chassis. Handles
    /// `Advance` by pushing onto the embedder event channel; the
    /// embedder's `run_frame` loop runs the requested ticks then
    /// replies via outbound.
    pub struct TestBenchCapability {
        events: EventSender,
        outbound: Arc<HubOutbound>,
    }

    #[actor]
    impl NativeActor for TestBenchCapability {
        type Config = TestBenchCapConfig;

        const NAMESPACE: &'static str = "aether.test_bench";

        fn init(
            config: TestBenchCapConfig,
            ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            let outbound = ctx.mailer().outbound().cloned().ok_or_else(|| {
                BootError::Other(Box::new(io::Error::other(
                    "HubOutbound must be wired on Mailer before \
                     TestBenchCapability::init (test-bench attaches its loopback before \
                     the Builder chain)",
                )))
            })?;
            Ok(Self {
                events: config.events,
                outbound,
            })
        }

        /// Push `ChassisEvent::Advance` onto the embedder loop. If the
        /// receiver is gone (chassis shutting down) reply `Err` inline
        /// so the caller doesn't hang.
        #[handler]
        fn on_advance(&self, ctx: &mut NativeCtx<'_>, mail: Advance) {
            let sender = ctx.reply_target();
            if self
                .events
                .send(ChassisEvent::Advance {
                    reply_to: sender,
                    ticks: mail.ticks,
                })
                .is_err()
            {
                self.outbound.send_reply(
                    sender,
                    &AdvanceResult::Err {
                        error: "test-bench chassis shutting down — advance aborted".to_owned(),
                    },
                );
            }
        }
    }
}
