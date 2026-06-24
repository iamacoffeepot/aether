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

// The actor halves are substrate-typed (ADR-0122 split). The whole module
// is `#[cfg(test)]` (gated at its `mod` declaration in `rpc/mod.rs`) and
// tests always carry `runtime`, so these resolve; the `#[actor]` macro
// additionally gates the emitted `NativeActor` / `Dispatch` runtime impls
// behind `feature = "runtime"`. The kind types above stay always-on so
// their `Kind`-derived inventory submissions register for the test
// substrate's registry walk.
use crate::shared::contentgen::task_queue::TaskQueue;
use aether_actor::actor;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
use aether_substrate::chassis::error::BootError;
use std::thread;
use std::time::Duration;

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
/// `aether.rpc.test.echo` identity (ADR-0122 split). A ZST carrying only
/// the addressing markers `#[actor]` emits always-on; the (empty) runtime
/// state lives in `TestEchoActorState`.
pub struct TestEchoActor;

/// Runtime state for [`TestEchoActor`]: a named empty stand-in (ADR-0122
/// hard rule — never `()` / `Self`) for an actor that holds nothing.
pub struct TestEchoActorState;

#[actor(singleton)]
impl NativeActor for TestEchoActor {
    type State = TestEchoActorState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.rpc.test.echo";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<TestEchoActorState, BootError> {
        Ok(TestEchoActorState)
    }

    /// Stateless echo handler — the empty state is unused.
    #[handler]
    fn on_echo(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: TestEchoRequest,
    ) -> TestEchoReply {
        TestEchoReply { value: mail.value }
    }
}

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
/// ADR-0093 hold-until-resolve dispatch ([`crate::shared::contentgen::TaskQueue`]
/// over `ctx.dispatch_blocking`), reproducing the production content-gen
/// caps' deferred-reply shape (submit -> spawned worker -> completion wake
/// -> re-reply). The whole point is that the reply happens *after* the
/// handler returns, so the framework-held settlement hold must keep the
/// chain open across the gap.
/// `aether.rpc.test.deferred_echo` identity (ADR-0122 split). A ZST
/// carrying the addressing markers; its runtime state — the
/// `TaskQueue` backing the off-thread dispatch — lives in
/// `DeferredEchoActorState`.
pub struct DeferredEchoActor;

/// Runtime state for [`DeferredEchoActor`]: the ADR-0093 hold-until-resolve
/// task queue the deferred handler submits onto.
pub struct DeferredEchoActorState {
    tasks: TaskQueue,
}

#[actor(singleton)]
impl NativeActor for DeferredEchoActor {
    type State = DeferredEchoActorState;
    type Config = ();
    const NAMESPACE: &'static str = "aether.rpc.test.deferred_echo";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<DeferredEchoActorState, BootError> {
        Ok(DeferredEchoActorState {
            tasks: TaskQueue::new(4),
        })
    }

    /// Submit the echo off-thread via the ADR-0093 dispatch primitive.
    /// The worker sleeps briefly so the handler reliably returns
    /// (queuing its `Finished`) before the reply lands — the window the
    /// bug used to settle in. The framework-held `SettlementHold` keeps
    /// the chain open until the deferred re-reply.
    #[handler]
    fn on_deferred_echo(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: DeferredEchoRequest,
    ) {
        let value = mail.value;
        state.tasks.submit(ctx, move || {
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
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<DeferredEchoReply>,
    ) {
        done.resolve(ctx);
        state.tasks.on_complete(ctx);
    }
}
