//! Shared test-support: a minimal echo actor plus its request / reply
//! kinds, used by both the `rpc::server` and `rpc::client` round-trip
//! tests.
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
    use aether_actor::{actor, actor::ctx::OutboundReply};
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
        fn on_echo(&mut self, ctx: &mut NativeCtx<'_>, mail: TestEchoRequest) {
            ctx.reply(&TestEchoReply { value: mail.value });
        }
    }
}

// `TestEchoActor` is re-exported at this module's root by the
// `#[bridge]` macro itself — no explicit `pub use` needed.

/// Deferred-echo request — like [`TestEchoRequest`] but the actor
/// answers it through the content-gen [`InFlightDispatch`] two-hop path:
/// the handler spawns an off-thread worker that lands a result mail back
/// on the actor, and a second handler re-replies. Exercises the
/// settlement-hold contract (iamacoffeepot/aether#1031) end-to-end: the
/// chain must stay open across the spawn so the RPC `Call`'s
/// settlement subscription only fires after the deferred reply.
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

/// Test-only actor that answers [`DeferredEchoRequest`] off-thread via
/// the content-gen [`crate::contentgen::dispatch::InFlightDispatch`],
/// reproducing the production content-gen caps' deferred-reply shape
/// (submit -> spawned worker -> loopback result -> re-reply). The whole
/// point is that the reply happens *after* the handler returns, so the
/// settlement hold must keep the chain open across the gap.
#[aether_actor::bridge(singleton)]
mod deferred_echo_actor {
    use super::{DeferredEchoReply, DeferredEchoRequest};
    use crate::contentgen::dispatch::{BlockingCall, InFlightDispatch};
    use aether_actor::{actor, actor::ctx::OutboundReply};
    use aether_data::{Kind, KindId, MailboxId, ReplyTo};
    use aether_substrate::Mailer;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    pub struct DeferredEchoActor {
        dispatch: InFlightDispatch,
        mailer: Arc<Mailer>,
        self_mailbox: MailboxId,
    }

    #[actor]
    impl NativeActor for DeferredEchoActor {
        type Config = ();
        const NAMESPACE: &'static str = "aether.rpc.test.deferred_echo";

        fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self {
                dispatch: InFlightDispatch::new(4),
                mailer: ctx.mailer(),
                self_mailbox: ctx.self_id(),
            })
        }

        /// Submit the echo off-thread. The worker sleeps briefly so the
        /// handler reliably returns (queuing its `Finished`) before the
        /// reply lands — the window the bug used to settle in. With the
        /// hold, settlement waits for the re-reply.
        #[handler]
        fn on_deferred_echo(&mut self, ctx: &mut NativeCtx<'_>, mail: DeferredEchoRequest) {
            let request_id = mail.value;
            let reply_to = OutboundReply::reply_target(ctx).unwrap_or(ReplyTo::NONE);
            let root = ctx.in_flight_root();
            let value = mail.value;
            let call: BlockingCall = Box::new(move || {
                // Brief blocking work standing in for a provider call.
                thread::sleep(Duration::from_millis(50));
                let reply = DeferredEchoReply { value };
                (
                    KindId(<DeferredEchoReply as Kind>::ID.0),
                    reply.encode_into_bytes(),
                )
            });
            self.dispatch.submit(
                &self.mailer,
                self.self_mailbox,
                root,
                request_id,
                reply_to,
                call,
            );
        }

        /// Loopback landing for the worker's result mail: re-reply to the
        /// original caller, then drop the settlement hold (ADR-0080 §12
        /// ordering — `Sent` precedes `Release`).
        #[allow(clippy::needless_pass_by_value)]
        #[handler]
        fn on_deferred_echo_result(&mut self, ctx: &mut NativeCtx<'_>, mail: DeferredEchoReply) {
            if let Some(landed) = self.dispatch.take_landed(mail.value) {
                OutboundReply::reply_to(ctx, landed.reply_to, &mail);
                drop(landed);
            }
            let _ = self
                .dispatch
                .on_reply_landed(&self.mailer, self.self_mailbox);
        }
    }
}

// `DeferredEchoActor` is re-exported at this module's root by the
// `#[bridge]` macro.
