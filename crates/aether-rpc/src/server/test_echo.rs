//! Test-support for the RPC server round-trip path: a minimal echo actor
//! plus its request / reply kinds — the far-end receiver of an RPC `Call`
//! that actually replies. Used by this crate's `server` round-trip tests
//! and by `aether-fleet`'s proxy test, which forwards onto this same `Call`
//! path through a booted `RpcServerCapability`.
//!
//! Lives under `server` (not the crate root) because both consumers are
//! RPC-server round-trips — `aether-fleet`'s proxy already reaches into
//! `aether_rpc::server` for the server cap. The module is gated at its `mod`
//! declaration on `any(test, feature = "test-support")`, so the `pub` reaches
//! this crate's own tests and a sibling crate's dev-dependency build, never
//! the shipped surface.
//!
//! The kinds live at this module's root (not nested in a `mod tests`)
//! so the `Kind` derive's inventory submission stays addressable from a
//! path the linker keeps — and so the derive registers them in
//! `aether_kinds::descriptors::all()` for the test substrate's registry
//! walk.

use serde::{Deserialize, Serialize};

// The actors are substrate-typed; the module's own gate keeps them out of a
// shipped build. Both are the un-split `type State = Self` shape — the fixture
// form the ADR-0122 split reserves for test-only actors — so their runtime
// impls ride the macro's `not(wasm)` gate rather than a feature. The kind types
// stay always-on so their `Kind`-derived inventory submissions register for the
// test substrate's registry walk.
use aether_actor::actor;
use aether_substrate::actor::native::TaskQueue;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, TaskDone};
use aether_substrate::chassis::error::BootError;
use std::thread;
use std::time::Duration;

/// Echo request kind — the test driver sends one of these; the echo
/// actor replies with a [`TestEchoReply`] carrying the same `value`.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.rpc.test.echo_request")]
pub struct TestEchoRequest {
    pub value: u64,
}

/// Echo reply kind — the echo actor's response to a [`TestEchoRequest`].
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.rpc.test.echo_reply")]
pub struct TestEchoReply {
    pub value: u64,
}

/// Test-only echo actor: handles [`TestEchoRequest`] and replies with a
/// matching [`TestEchoReply`]. The minimum viable receiver for
/// exercising the RPC `Call → ReplyEvent → ReplyEnd` path without
/// coupling a test to a production cap's semantics. Holds nothing.
pub struct TestEchoActor;

#[actor(singleton, root)]
impl NativeActor for TestEchoActor {
    type Config = ();
    const NAMESPACE: &'static str = "aether.rpc.test.echo";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    /// Stateless echo handler.
    #[allow(clippy::unused_self)] // actor handler ABI always receives state
    #[handler::single]
    fn on_echo(&mut self, _ctx: &mut NativeCtx<'_>, mail: TestEchoRequest) -> TestEchoReply {
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
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.rpc.test.deferred_echo_request")]
pub struct DeferredEchoRequest {
    pub value: u64,
}

/// Deferred-echo reply — the worker thread lands this on the actor's own
/// mailbox (the loopback result mail), and the actor re-replies the same
/// shape to the original caller.
#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "aether.rpc.test.deferred_echo_reply")]
pub struct DeferredEchoReply {
    pub value: u64,
}

/// Test-only actor that answers [`DeferredEchoRequest`] off-thread via the
/// ADR-0093 hold-until-resolve dispatch ([`TaskQueue`]
/// over `ctx.dispatch_blocking`), reproducing the production content-gen
/// caps' deferred-reply shape (submit -> spawned worker -> completion wake
/// -> re-reply). The whole point is that the reply happens *after* the
/// handler returns, so the framework-held settlement hold must keep the
/// chain open across the gap. Holds the [`TaskQueue`] the deferred handler
/// submits onto.
pub struct DeferredEchoActor {
    tasks: TaskQueue,
}

#[actor(singleton, root)]
impl NativeActor for DeferredEchoActor {
    type Config = ();
    const NAMESPACE: &'static str = "aether.rpc.test.deferred_echo";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { tasks: TaskQueue::new(4) })
    }

    /// Submit the echo off-thread via the ADR-0093 dispatch primitive.
    /// The worker sleeps briefly so the handler reliably returns
    /// (queuing its `Finished`) before the reply lands — the window the
    /// bug used to settle in. The framework-held `SettlementHold` keeps
    /// the chain open until the deferred re-reply.
    #[handler::single]
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
    fn on_deferred_echo_done(&mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<DeferredEchoReply>) {
        done.resolve(ctx);
        self.tasks.on_complete(ctx);
    }
}
