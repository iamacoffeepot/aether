//! Issue 6400: a replaced guest must never reuse a request id still pending
//! from before the swap (ADR-0139 §3 / §4).
//!
//! `CarryRequester` sends one [`CarriedRequest`] per [`RunCarriedRequest`] to
//! `ReplyHolder` by type, binding a fixture-local [`CarriedContext`] with the
//! same tag. `ReplyHolder` parks each request's reply handle until
//! [`ReleaseCarried`], then answers them all in arrival order. A scenario
//! holds the tag-1 request open across a `replace_component` of the
//! requester, sends tag 2 from the replacement, and releases both: each reply
//! reports [`CarriedReplyMatched`] only when it recovers its own request's
//! context, which holds only when the replacement's request id continues past
//! its predecessor's.
//!
//! Issue 6409: a reply handle stays answerable across a `replace_component`
//! of the guest that holds it. `ReplyHolder` carries its parked handles
//! through `on_dehydrate` / `on_rehydrate`, so a second scenario replaces the
//! holder instead, with the tag-1 handle parked, and sends tag 2 to the
//! replacement: both replies match only when the replacement answers the
//! carried handle to its own requester and numbers the tag-2 handle past it.

#![allow(clippy::unused_self)] // aether-suppression-request: the ADR-0033 dispatch ABI fixes the handler signature at `&mut self`, and `CarryRequester` is stateless so its first request takes the mailbox's first id — the same allow `source_forwarder` carries

use aether_actor::{
    ActorInitError, Erased, Manual, OutboundReply, PriorState, ReplyHandle, WasmActor, WasmCtx, WasmDropCtx,
    WasmInitCtx, actor,
};
use aether_test_fixtures_kinds::{
    CarriedReplyMatched, CarriedRequest, CarriedRequestResult, ReleaseCarried, RunCarriedRequest,
    SubstrateHarnessObserver,
};

#[aether_data::kind(name = "aether.test_fixtures.carried_context", no_serde)]
struct CarriedContext {
    tag: u32,
}

/// The reply handles `ReplyHolder` has parked, with their tags, carried
/// across a replace of the holder.
#[aether_data::kind(name = "aether.test_fixtures.parked_replies")]
struct ParkedReplies {
    handles: Vec<ReplyHandle>,
    tags: Vec<u32>,
}

/// Sends nothing from `init` or `wire`, so its first request takes the first
/// id its mailbox mints.
pub struct CarryRequester;

#[actor]
impl WasmActor for CarryRequester {
    const NAMESPACE: &'static str = "test.carry.requester";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(CarryRequester)
    }

    #[handler::single]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, run: RunCarriedRequest) {
        let _ = ctx
            .actor::<ReplyHolder>()
            .with_context(&CarriedContext { tag: run.tag })
            .send(&CarriedRequest { tag: run.tag });
    }

    #[handler::single]
    fn on_reply(&mut self, ctx: &mut WasmCtx<'_>, reply: CarriedRequestResult) {
        match ctx.take_context::<CarriedContext>() {
            Some(context) if context.tag == reply.tag => {
                ctx.actor::<SubstrateHarnessObserver>().send(&CarriedReplyMatched);
            }
            other => tracing::warn!(
                target: "test.carry.requester",
                reply_tag = reply.tag,
                context_tag = ?other.map(|context| context.tag),
                "carried reply did not recover its own request's context",
            ),
        }
    }
}

/// Parks each request's reply handle with its tag until told to answer.
pub struct ReplyHolder {
    parked: Vec<(ReplyHandle, u32)>,
}

#[actor]
impl WasmActor for ReplyHolder {
    const NAMESPACE: &'static str = "test.carry.holder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ReplyHolder { parked: Vec::new() })
    }

    #[handler::manual]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, request: CarriedRequest) {
        if let Some(handle) = ctx.reply_target() {
            self.parked.push((handle, request.tag));
        }
    }

    #[handler::manual]
    fn on_release(&mut self, ctx: &mut WasmCtx<'_, Erased, Manual>, _release: ReleaseCarried) {
        for (handle, tag) in self.parked.drain(..) {
            ctx.reply_to(handle, &CarriedRequestResult { tag });
        }
    }

    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        let (handles, tags) = self.parked.drain(..).unzip();
        ctx.save_state_kind::<ParkedReplies>(0, &ParkedReplies { handles, tags });
    }

    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
        if let Some(saved) = prior.decode_kind::<ParkedReplies>() {
            self.parked = saved.handles.into_iter().zip(saved.tags).collect();
        }
    }
}
