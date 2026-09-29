//! Actors every version of a republish fixture family shares (issue 7109).
//!
//! Each module version is an example cdylib of this crate and lists these
//! actors in its own `export!`, so a republish of one version by the other
//! keeps every namespace its predecessor exported (ADR-0241 §4). This lib has
//! no `export!` of its own.

use aether_actor::{
    ActorInitError, Erased, Manual, OutboundReply, PriorState, ReplyHandle, WasmActor, WasmCtx, WasmDropCtx,
    WasmInitCtx, actor,
};
use aether_test_fixtures_kinds::{CarriedRequest, CarriedRequestResult, ReleaseCarried};

/// The reply handles `ReplyHolder` has parked, with their tags, carried
/// across a replace of the holder.
#[aether_data::kind(name = "aether.test_fixtures.republish_parked_replies", no_serde)]
struct ParkedReplies {
    handles: Vec<ReplyHandle>,
    tags: Vec<u32>,
}

/// Parks each [`CarriedRequest`]'s reply handle with its tag until
/// [`ReleaseCarried`], then answers them all in arrival order. The carry
/// family's requesters and held relays send to it by type, and it keeps its
/// parked handles across a replace.
pub struct ReplyHolder {
    parked: Vec<(ReplyHandle, u32)>,
}

#[actor(root)]
impl WasmActor for ReplyHolder {
    const NAMESPACE: &'static str = "test.republish.carry.holder";

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
