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

        fn init(_: (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self)
        }

        #[handler]
        fn on_echo(&mut self, ctx: &mut NativeCtx<'_>, mail: TestEchoRequest) {
            ctx.reply(&TestEchoReply { value: mail.value });
        }
    }
}

// `TestEchoActor` is re-exported at this module's root by the
// `#[bridge]` macro itself — no explicit `pub use` needed.
