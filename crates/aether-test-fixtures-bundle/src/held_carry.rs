//! Issue 6983: a guest's held reply survives a `replace_component` of the
//! guest that holds it, in a carried request context and in saved state, and
//! a replace that would strand one is refused (ADR-0243 §6).
//!
//! `HeldRequester` sends one [`HeldRequest`] per [`RunHeldRequest`] to the
//! held actor its `target` names. The send is detached, so the held reply's
//! chain stays out of the chain the harness settles, and a scenario can
//! replace the holder while the reply is still owed. On each [`HeldRequestResult`]
//! that echoes a tag it sent, the requester reports [`HeldReplyMatched`] and
//! counts the match; it answers [`CountQuery`] with that count, which is the
//! barrier a scenario polls, since the reply lands on a chain it never joins.
//!
//! Each held actor answers its [`HeldRequest`] through a `Held<HeldRequestResult>`:
//!
//! - `HeldRelay` carries it in a request context on a [`CarriedRequest`] to
//!   the correlation-carry `ReplyHolder`, and answers from that request's
//!   [`CarriedRequestResult`];
//! - `HeldKeeper` parks it in state, saves it through `on_dehydrate` and
//!   restores it through `on_rehydrate`, and answers on [`ReleaseHeld`];
//! - `HeldForgetter` parks it in state like `HeldKeeper` but saves nothing,
//!   so a replace must refuse rather than strand the reply.

use aether_actor::{ActorInitError, Held, Pending, PriorState, WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{
    CarriedRequest, CarriedRequestResult, CountQuery, CountReport, HELD_TARGET_FORGETTER, HELD_TARGET_KEEPER,
    HELD_TARGET_RELAY, HeldReplyMatched, HeldRequest, HeldRequestResult, ReleaseHeld, RunHeldRequest,
    SubstrateHarnessObserver,
};

use crate::correlation_carry::ReplyHolder;

/// The state one relayed request carries to its reply handler: the reply
/// the relay owes, and the tag it echoes.
#[aether_data::kind(name = "aether.test_fixtures.held_relay_context")]
struct HeldRelayContext {
    held: Held<HeldRequestResult>,
    tag: u32,
}

/// The held replies `HeldKeeper` saves across a replace, with their tags.
#[aether_data::kind(name = "aether.test_fixtures.kept_helds")]
struct KeptHelds {
    helds: Vec<Held<HeldRequestResult>>,
    tags: Vec<u32>,
}

/// Sends each [`HeldRequest`] detached and counts the replies that echo a
/// tag it sent.
pub struct HeldRequester {
    sent: Vec<u32>,
    matched: u32,
}

#[actor(root, depends(HeldRelay, HeldKeeper, HeldForgetter, SubstrateHarnessObserver))]
impl WasmActor for HeldRequester {
    const NAMESPACE: &'static str = "test.held.requester";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(HeldRequester { sent: Vec::new(), matched: 0 })
    }

    #[handler::single]
    fn on_run(&mut self, ctx: &mut WasmCtx<'_>, run: RunHeldRequest) {
        let request = HeldRequest { tag: run.tag };
        match run.target {
            HELD_TARGET_RELAY => ctx.send_detached::<HeldRelay>(&request),
            HELD_TARGET_KEEPER => ctx.send_detached::<HeldKeeper>(&request),
            HELD_TARGET_FORGETTER => ctx.send_detached::<HeldForgetter>(&request),
            target => {
                tracing::warn!(target: "test.held.requester", target, "unknown held target");
                return;
            }
        }
        self.sent.push(run.tag);
    }

    #[handler::single]
    fn on_reply(&mut self, ctx: &mut WasmCtx<'_>, reply: HeldRequestResult) {
        let Some(index) = self.sent.iter().position(|tag| *tag == reply.tag) else {
            tracing::warn!(target: "test.held.requester", tag = reply.tag, "held reply echoes no tag sent");
            return;
        };
        self.sent.swap_remove(index);
        self.matched += 1;
        ctx.send::<SubstrateHarnessObserver>(&HeldReplyMatched);
    }

    #[handler::single]
    fn on_count(&mut self, _ctx: &mut WasmCtx<'_>, _query: CountQuery) -> CountReport {
        CountReport { count: self.matched }
    }
}

/// Holds each request's reply in the context of a [`CarriedRequest`] to
/// `ReplyHolder`, and answers when that request's result comes back.
pub struct HeldRelay {
    relayed: u32,
}

#[actor(root, depends(ReplyHolder))]
impl WasmActor for HeldRelay {
    const NAMESPACE: &'static str = "test.held.relay";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(HeldRelay { relayed: 0 })
    }

    #[handler::single]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_>, request: HeldRequest) -> Pending<HeldRequestResult> {
        let (pending, held) = ctx.hold::<HeldRequestResult>();
        let _ = ctx.send_with_context::<ReplyHolder>(
            &CarriedRequest { tag: request.tag },
            HeldRelayContext { held, tag: request.tag },
        );
        self.relayed += 1;
        pending
    }

    #[handler::single]
    fn on_result(&mut self, ctx: &mut WasmCtx<'_>, result: CarriedRequestResult) {
        let Some(context) = ctx.take_context::<HeldRelayContext>() else {
            tracing::warn!(
                target: "test.held.relay",
                tag = result.tag,
                relayed = self.relayed,
                "carried result has no held relay context",
            );
            return;
        };
        context.held.answer(ctx, &HeldRequestResult { tag: context.tag });
    }
}

/// Parks each request's reply in state until [`ReleaseHeld`], and carries the
/// parked replies across a replace through saved state.
pub struct HeldKeeper {
    kept: Vec<(Held<HeldRequestResult>, u32)>,
}

#[actor(root)]
impl WasmActor for HeldKeeper {
    const NAMESPACE: &'static str = "test.held.keeper";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(HeldKeeper { kept: Vec::new() })
    }

    #[handler::single]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_>, request: HeldRequest) -> Pending<HeldRequestResult> {
        let (pending, held) = ctx.hold::<HeldRequestResult>();
        self.kept.push((held, request.tag));
        pending
    }

    #[handler::single]
    fn on_release(&mut self, ctx: &mut WasmCtx<'_>, _release: ReleaseHeld) {
        for (held, tag) in self.kept.drain(..) {
            held.answer(ctx, &HeldRequestResult { tag });
        }
    }

    /// Hand-written, because the ADR-0113 `dehydrate(&self)` accessor cannot
    /// move a `Held` out of the actor.
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        let (helds, tags) = self.kept.drain(..).unzip();
        ctx.save_state_kind(0, &KeptHelds { helds, tags });
    }

    fn on_rehydrate(&mut self, _ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
        if let Some(saved) = prior.decode_kind::<KeptHelds>() {
            self.kept = saved.helds.into_iter().zip(saved.tags).collect();
        }
    }
}

/// [`HeldKeeper`] without the dehydrate override: its parked replies are
/// never saved, so a replace while one is live must be refused.
pub struct HeldForgetter {
    kept: Vec<(Held<HeldRequestResult>, u32)>,
}

#[actor(root)]
impl WasmActor for HeldForgetter {
    const NAMESPACE: &'static str = "test.held.forgetter";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(HeldForgetter { kept: Vec::new() })
    }

    #[handler::single]
    fn on_request(&mut self, ctx: &mut WasmCtx<'_>, request: HeldRequest) -> Pending<HeldRequestResult> {
        let (pending, held) = ctx.hold::<HeldRequestResult>();
        self.kept.push((held, request.tag));
        pending
    }

    #[handler::single]
    fn on_release(&mut self, ctx: &mut WasmCtx<'_>, _release: ReleaseHeld) {
        for (held, tag) in self.kept.drain(..) {
            held.answer(ctx, &HeldRequestResult { tag });
        }
    }
}
