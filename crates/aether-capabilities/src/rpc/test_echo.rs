//! Shared test-support: a minimal echo actor plus its request / reply
//! kinds, used by the `rpc::server` round-trip tests (the client half
//! lives in `aether-rpc` per ADR-0102) and the `engine::proxy` test.
//!
//! The kinds live at this module's root (not nested in a `mod tests`)
//! so the `Kind` derive's inventory submission stays addressable from a
//! path the linker keeps — and so the derive registers them in
//! `aether_kinds::descriptors::all()` for the test substrate's registry
//! walk. The whole module is `#[cfg(test)]` (gated at the `mod`
//! declaration in `rpc/mod.rs`): it is test scaffolding, not part of
//! the cap's shipped surface.

use serde::{Deserialize, Serialize};

/// Echo request kind — the test driver sends one of these; the echo
/// actor replies with a [`TestEchoReply`] carrying the same `value`.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.rpc.test.echo_request")]
pub struct TestEchoRequest {
    pub value: u64,
}

/// Echo reply kind — the echo actor's response to a [`TestEchoRequest`].
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.rpc.test.echo_reply")]
pub struct TestEchoReply {
    pub value: u64,
}

/// Test-only echo actor: handles [`TestEchoRequest`] and replies with a
/// matching [`TestEchoReply`]. The minimum viable receiver for
/// exercising the RPC `Call → ReplyEvent → ReplyEnd` path without
/// coupling a test to a production cap's semantics.
#[aether_actor::bridge(singleton)]
mod test_echo_actor {
    use super::{TestEchoReply, TestEchoRequest};
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    pub struct TestEchoActor;

    #[actor]
    impl NativeActor for TestEchoActor {
        type Config = ();
        const NAMESPACE: &'static str = "aether.rpc.test.echo";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        // Stateless echo handler — keeps `&mut self` to match the
        // dispatch ABI (ADR-0033 / ADR-0038) even though the body
        // doesn't touch component state.
        #[allow(clippy::unused_self)]
        #[handler]
        fn on_echo(&mut self, _ctx: &mut NativeCtx<'_>, mail: TestEchoRequest) -> TestEchoReply {
            TestEchoReply { value: mail.value }
        }
    }
}

// `TestEchoActor` is re-exported at this module's root by the
// `#[bridge]` macro itself — no explicit `pub use` needed.

/// Deferred-echo request — like [`TestEchoRequest`] but the actor
/// answers it through the ADR-0093 hold-until-resolve dispatch
/// (`TaskQueue` over `ctx.dispatch_blocking`): the handler spawns an
/// off-thread worker, and a `#[handler(task)]` completion re-replies when
/// the worker finishes. Exercises the settlement-hold contract
/// (iamacoffeepot/aether#1031) end-to-end: the chain must stay open across
/// the spawn so the RPC `Call`'s settlement subscription only fires after
/// the deferred reply.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.rpc.test.deferred_echo_request")]
pub struct DeferredEchoRequest {
    pub value: u64,
}

/// Deferred-echo reply — the worker thread lands this on the actor's own
/// mailbox (the loopback result mail), and the actor re-replies the same
/// shape to the original caller.
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "aether.rpc.test.deferred_echo_reply")]
pub struct DeferredEchoReply {
    pub value: u64,
}

/// Test-only actor that answers [`DeferredEchoRequest`] off-thread via the
/// ADR-0093 hold-until-resolve dispatch ([`crate::contentgen::TaskQueue`]
/// over `ctx.dispatch_blocking`), reproducing the production content-gen
/// caps' deferred-reply shape (submit -> spawned worker -> completion wake
/// -> re-reply). The whole point is that the reply happens *after* the
/// handler returns, so the framework-held settlement hold must keep the
/// chain open across the gap.
#[aether_actor::bridge(singleton)]
mod deferred_echo_actor {
    use super::{DeferredEchoReply, DeferredEchoRequest};
    use crate::contentgen::task_queue::TaskQueue;
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
    use aether_substrate::chassis::error::BootError;
    use std::thread;
    use std::time::Duration;

    pub struct DeferredEchoActor {
        tasks: TaskQueue,
    }

    #[actor]
    impl NativeActor for DeferredEchoActor {
        type Config = ();
        const NAMESPACE: &'static str = "aether.rpc.test.deferred_echo";

        fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self {
                tasks: TaskQueue::new(4),
            })
        }

        /// Submit the echo off-thread via the ADR-0093 dispatch primitive.
        /// The worker sleeps briefly so the handler reliably returns
        /// (queuing its `Finished`) before the reply lands — the window the
        /// bug used to settle in. The framework-held `SettlementHold` keeps
        /// the chain open until the deferred re-reply.
        #[handler]
        fn on_deferred_echo(&mut self, ctx: &mut NativeCtx<'_>, mail: DeferredEchoRequest) {
            let value = mail.value;
            self.tasks.submit(ctx, move || {
                // Brief blocking work standing in for a provider call.
                thread::sleep(Duration::from_millis(50));
                DeferredEchoReply { value }
            });
        }

        /// ADR-0093 completion: re-reply to the original caller (drops the
        /// hold after the reply — `Sent` precedes `Release`), then free the
        /// in-flight slot.
        #[handler(task)]
        fn on_deferred_echo_done(
            &mut self,
            ctx: &mut NativeCtx<'_>,
            done: TaskDone<DeferredEchoReply>,
        ) {
            done.resolve(ctx);
            self.tasks.on_complete(ctx);
        }
    }
}

// `DeferredEchoActor` is re-exported at this module's root by the
// `#[bridge]` macro.
