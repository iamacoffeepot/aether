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

// Handler-signature kinds must be importable at file root because
// `#[bridge]` emits `impl HandlesKind<K> for X {}` markers as siblings
// of the mod (always-on, outside the cfg gate).
use aether_kinds::Advance;

#[aether_actor::bridge(singleton)]
mod native {
    use std::sync::Arc;

    use aether_actor::actor;
    use aether_kinds::AdvanceResult;
    use aether_substrate::capability::BootError;
    use aether_substrate::native_actor::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::outbound::HubOutbound;

    use super::Advance;

    /// Stub cap for `aether.test_bench` on chassis without test-bench
    /// drive (desktop, headless). Replies `AdvanceResult::Err` so MCP
    /// `aether.test_bench.advance` mail fails fast instead of hanging
    /// on a reply that never comes.
    pub struct UnsupportedTestBenchCapability {
        outbound: Arc<HubOutbound>,
    }

    #[actor]
    impl NativeActor for UnsupportedTestBenchCapability {
        type Config = ();

        const NAMESPACE: &'static str = "aether.test_bench";

        fn init(_config: (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let outbound = ctx.mailer().outbound().cloned().ok_or_else(|| {
                BootError::Other(Box::new(std::io::Error::other(
                    "HubOutbound must be wired on Mailer before \
                     UnsupportedTestBenchCapability::init (chassis main connects the hub before \
                     the Builder chain)",
                )))
            })?;
            Ok(Self { outbound })
        }

        /// Reply `Err` so MCP `advance` fails fast on chassis that don't
        /// drive ticks via the embedder loop.
        #[handler]
        fn on_advance(&self, ctx: &mut NativeCtx<'_>, _mail: Advance) {
            self.outbound.send_reply(
                ctx.reply_target(),
                &AdvanceResult::Err {
                    error: "unsupported on this chassis — aether.test_bench.advance is \
                        test-bench-only (ADR-0067)"
                        .to_owned(),
                },
            );
        }
    }
}
