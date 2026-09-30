//! Issue 7204: a context stored with a request reaches its reply when the
//! request goes from an inline child to the root actor hosting its cluster.
//!
//! `InlineContextHost` spawns `InlineContextAsker` as an inline child on the
//! first [`RunCarriedRequest`] and forwards the trigger to it. The asker sends
//! a [`CarriedRequest`] to its host with `send_with_context`, binding a
//! fixture-local [`InlineCarriedContext`] with the same tag, and its response
//! handler reports [`CarriedReplyMatched`] only when the reply arrives with
//! that context. The host is a member of the asker's own cluster, so the
//! scenario proves a tracked send to a cluster member still gets a correlated
//! reply.
//!
//! The spawn runs from a handler rather than `wire`, so the child's
//! `depends(InlineContextHost)` is checked while the host is live (ADR-0165,
//! ADR-0241 §4).

use aether_actor::{ActorInitError, InlineChild, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{
    CarriedReplyMatched, CarriedRequest, CarriedRequestResult, RunCarriedRequest, SubstrateHarnessObserver,
};

#[aether_data::kind(name = "aether.test_fixtures.inline_carried_context", no_serde)]
struct InlineCarriedContext {
    tag: u32,
}

/// The cluster root: spawns the asker on demand and answers its requests.
pub struct InlineContextHost {
    asker: Option<InlineChild<InlineContextAsker>>,
}

#[actor(root, spawns(InlineContextAsker))]
impl WasmActor for InlineContextHost {
    const NAMESPACE: &'static str = "test.inline_context.host";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(InlineContextHost { asker: None })
    }

    #[handler::tell]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, run: RunCarriedRequest) {
        if self.asker.is_none() {
            self.asker =
                ctx.spawn_inline_child::<InlineContextHost, InlineContextAsker>(Subname::Named("asker"), &()).ok();
        }
        if let Some(asker) = self.asker {
            asker.send(ctx, &run);
        }
    }

    #[handler::request]
    fn on_request(&mut self, _ctx: &mut WasmCtx<'_>, request: CarriedRequest) -> CarriedRequestResult {
        CarriedRequestResult { tag: request.tag }
    }
}

/// The inline child: asks its hosting root with a stored context.
pub struct InlineContextAsker;

#[actor(instanced, child_of(InlineContextHost), depends(InlineContextHost, SubstrateHarnessObserver))]
impl WasmActor for InlineContextAsker {
    const NAMESPACE: &'static str = "test.inline_context.asker";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(InlineContextAsker)
    }

    #[handler::tell]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, run: RunCarriedRequest) {
        let _ = ctx.send_with_context::<InlineContextHost>(
            &CarriedRequest { tag: run.tag },
            InlineCarriedContext { tag: run.tag },
        );
    }

    #[handler::response]
    fn on_reply(&mut self, ctx: &mut WasmCtx<'_>, reply: CarriedRequestResult, context: InlineCarriedContext) {
        if context.tag == reply.tag {
            ctx.send::<SubstrateHarnessObserver>(&CarriedReplyMatched);
        }
    }
}
