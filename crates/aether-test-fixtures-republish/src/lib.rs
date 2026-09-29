//! Actors every version of a republish fixture family shares (issue 7109).
//!
//! Each module version is an example cdylib of this crate and lists these
//! actors in its own `export!`, so a republish of one version by the other
//! keeps every namespace its predecessor exported (ADR-0241 §4). This lib has
//! no `export!` of its own.

use std::mem;

use aether_actor::{
    ActorInitError, Erased, Held, Manual, OutboundReply, Pending, PriorState, ReplyHandle, WasmActor, WasmCtx,
    WasmDropCtx, WasmInitCtx, actor,
};
use aether_test_fixtures_kinds::{
    CarriedRequest, CarriedRequestResult, CountQuery, CountReport, HeldRequest, HeldRequestResult, ReleaseCarried,
};

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

    /// Saves copies, so a guest reinstated by an aborted republish still
    /// holds its parked handles (ADR-0241 §7).
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        let (handles, tags) = self.parked.iter().copied().unzip();
        ctx.save_state_kind::<ParkedReplies>(0, &ParkedReplies { handles, tags });
    }

    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
        if let Some(saved) = prior.decode_kind::<ParkedReplies>() {
            self.parked = saved.handles.into_iter().zip(saved.tags).collect();
        }
    }
}

/// What `Keeper` moves out of itself in `on_dehydrate`: its first held
/// reply with that reply's tag, and its request count.
#[aether_data::kind(name = "aether.test_fixtures.republish_kept_state")]
struct KeptState {
    held: Option<Held<HeldRequestResult>>,
    tag: u32,
    kept: u32,
}

/// Holds each [`HeldRequest`]'s reply until [`ReleaseCarried`], then answers
/// every one it holds. The first request's reply is `held`; any later one is
/// a `stray`. It answers [`CountQuery`] with the number of requests it took.
///
/// Its `on_dehydrate` moves `held`, its tag and the count out of the actor
/// into saved state, so a reinstated guest that did not get that state back
/// has lost them (issue 7125). It leaves every stray live and unsaved, so a
/// keeper holding a second request refuses its dehydrate as held-unsaved.
pub struct Keeper {
    held: Option<Held<HeldRequestResult>>,
    tag: u32,
    stray: Vec<(Held<HeldRequestResult>, u32)>,
    kept: u32,
}

#[actor(root)]
impl WasmActor for Keeper {
    const NAMESPACE: &'static str = "test.republish.keep.keeper";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Keeper { held: None, tag: 0, stray: Vec::new(), kept: 0 })
    }

    #[handler::single]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_>, request: HeldRequest) -> Pending<HeldRequestResult> {
        let (pending, held) = ctx.hold::<HeldRequestResult>();
        if self.held.is_none() {
            self.held = Some(held);
            self.tag = request.tag;
        } else {
            self.stray.push((held, request.tag));
        }
        self.kept += 1;
        pending
    }

    #[handler::single]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.kept }
    }

    #[handler::single]
    fn on_release(&mut self, ctx: &mut WasmCtx<'_>, _release: ReleaseCarried) {
        if let Some(held) = self.held.take() {
            held.answer(ctx, &HeldRequestResult { tag: self.tag });
        }
        for (held, tag) in self.stray.drain(..) {
            held.answer(ctx, &HeldRequestResult { tag });
        }
    }

    /// Moves the saved fields out rather than copying them, and leaves every
    /// stray behind.
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        let state =
            KeptState { held: self.held.take(), tag: mem::take(&mut self.tag), kept: mem::take(&mut self.kept) };
        ctx.save_state_kind(0, &state);
    }

    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
        if let Some(KeptState { held, tag, kept }) = prior.decode_kind::<KeptState>() {
            self.held = held;
            self.tag = tag;
            self.kept = kept;
        }
    }
}
