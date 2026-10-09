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
//!
//! Issue 6422: a third scenario has the holder answer once before the replace
//! and once after, so the two replies' trace `MailId`s differ only when the
//! replacement's reply-lineage counter continues past its predecessor's.
//!
//! Issue 7662: `ClosingRequester` also sends a request from `wire` and one
//! from `unwire`, so a republish has the retiring guest and its successor
//! both mint an id between prepare and commit. A fourth scenario republishes
//! it and sends one more request: every reply but the one to the `unwire`
//! request matches only when the retiring guest mints past its successor's
//! `wire` and the successor then mints past the retiring guest's `unwire`.

use aether_actor::{
    ActorInitError, Anyone, Erased, OutboundReply, PriorState, ReplyHandle, Unchecked, WasmActor, WasmCtx, WasmDropCtx,
    WasmInitCtx, WireCtx, actor,
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

#[actor(root, depends(ReplyHolder, SubstrateHarnessObserver))]
impl WasmActor for CarryRequester {
    const NAMESPACE: &'static str = "test.carry.requester";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(CarryRequester)
    }

    #[handler::tell]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, run: RunCarriedRequest) {
        let _ = ctx.send_with_context::<ReplyHolder>(&CarriedRequest { tag: run.tag }, CarriedContext { tag: run.tag });
    }

    #[handler::response]
    fn on_reply(&mut self, ctx: &mut WasmCtx<'_>, reply: CarriedRequestResult, context: Option<CarriedContext>) {
        match context {
            Some(context) if context.tag == reply.tag => {
                ctx.send::<SubstrateHarnessObserver>(&CarriedReplyMatched);
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

/// The tag of the request `ClosingRequester` sends from `wire`.
const WIRE_TAG: u32 = 0x7662_0001;

/// The tag of the request `ClosingRequester` sends from `unwire`.
const UNWIRE_TAG: u32 = 0x7662_0002;

/// A requester that also sends one request from `wire` and one from `unwire`,
/// each with a context of its own tag, so a republish has both of its guests
/// mint a request id before the commit ends.
pub struct ClosingRequester;

#[actor(root, depends(ReplyHolder, SubstrateHarnessObserver))]
impl WasmActor for ClosingRequester {
    const NAMESPACE: &'static str = "test.carry.closing_requester";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ClosingRequester)
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        let _ =
            ctx.send_with_context::<ReplyHolder>(&CarriedRequest { tag: WIRE_TAG }, CarriedContext { tag: WIRE_TAG });
        Ok(())
    }

    /// The context stored here dies with this guest: the answer reaches
    /// whichever guest holds the mailbox then, under an id that guest never
    /// stored.
    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        let _ = ctx
            .send_with_context::<ReplyHolder>(&CarriedRequest { tag: UNWIRE_TAG }, CarriedContext { tag: UNWIRE_TAG });
    }

    #[handler::tell]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, run: RunCarriedRequest) {
        let _ = ctx.send_with_context::<ReplyHolder>(&CarriedRequest { tag: run.tag }, CarriedContext { tag: run.tag });
    }

    #[handler::response]
    fn on_reply(&mut self, ctx: &mut WasmCtx<'_>, reply: CarriedRequestResult, context: Option<CarriedContext>) {
        match context {
            Some(context) if context.tag == reply.tag => {
                ctx.send::<SubstrateHarnessObserver>(&CarriedReplyMatched);
            }
            // The answer to a predecessor's `unwire` request: its context
            // died with the predecessor.
            None if reply.tag == UNWIRE_TAG => {}
            other => tracing::warn!(
                target: "test.carry.closing_requester",
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

#[actor(root)]
impl WasmActor for ReplyHolder {
    const NAMESPACE: &'static str = "test.carry.holder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(ReplyHolder { parked: Vec::new() })
    }

    #[handler::unchecked(reason = "test: parks the reply target for a later release")]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>, request: CarriedRequest) {
        if let Some(handle) = ctx.reply_target() {
            self.parked.push((handle, request.tag));
        }
    }

    #[handler::unchecked(reason = "test: answers parked requests from another handler")]
    fn on_release(&mut self, ctx: &mut WasmCtx<'_, Erased, Anyone, Unchecked>, _release: ReleaseCarried) {
        for (handle, tag) in self.parked.drain(..) {
            ctx.reply_to(handle, &CarriedRequestResult { tag });
        }
    }

    /// Saves copies, so a guest reinstated by an aborted republish still
    /// holds its parked handles (ADR-0241 §7).
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) -> Result<(), ActorInitError> {
        let (handles, tags) = self.parked.iter().copied().unzip();
        ctx.save_state_kind::<ParkedReplies>(0, &ParkedReplies { handles, tags })
    }

    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) -> Result<(), ActorInitError> {
        if let Some(saved) = prior.decode_kind::<ParkedReplies>() {
            self.parked = saved.handles.into_iter().zip(saved.tags).collect();
        }
        Ok(())
    }
}
