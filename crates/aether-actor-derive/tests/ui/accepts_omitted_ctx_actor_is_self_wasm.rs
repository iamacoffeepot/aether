//! ADR-0231 §7 (#6533): a ctx that omits its actor is typed by the actor
//! `#[actor]` dispatches for, on every method the macro hands a ctx to — a
//! handler, the `#[fallback]`, `wire`, `unwire` and `on_rehydrate` — and a
//! `#[handler_set]` member is typed by its adopter. The flat `ctx.send::<R>`
//! verb is bounded `A: DependsOn<R>`, so each call below compiles only when
//! that method's ctx names the adopting actor, never on the erased view.

use aether_actor::{ActorInitError, Mail, PriorState, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor, handler_set};

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.omitted_ctx.ping")]
struct Ping {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.omitted_ctx.pong")]
struct Pong {
    seq: u32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.omitted_ctx.trigger")]
struct Trigger {
    seq: u32,
}

struct Peer;

#[actor]
impl WasmActor for Peer {
    const NAMESPACE: &'static str = "test.omitted_ctx.peer";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Ping) {}
}

// The set states its default body's reach as a supertrait, so the member's
// `send::<Peer>` holds for every adopter.
#[handler_set]
trait Reach: aether_actor::DependsOn<Peer> {
    #[handler::single]
    fn on_pong(&mut self, ctx: &mut WasmCtx<'_>, pong: Pong) {
        ctx.send::<Peer>(&Ping { seq: pong.seq });
    }
}

struct Typed;

impl Reach for Typed {}

#[actor(depends(Peer), handler_set(Reach))]
impl WasmActor for Typed {
    const NAMESPACE: &'static str = "test.omitted_ctx.typed";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        ctx.send::<Peer>(&Ping { seq: 1 });
    }

    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        ctx.send::<Peer>(&Ping { seq: 2 });
    }

    fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_>, _prior: PriorState<'_>) {
        ctx.send::<Peer>(&Ping { seq: 3 });
    }

    #[handler::single]
    fn on_trigger(&mut self, ctx: &mut WasmCtx<'_>, trigger: Trigger) {
        ctx.send::<Peer>(&Ping { seq: trigger.seq });
    }
}

// The `#[fallback]` rides a second actor: a wasm actor adopting a handler set
// cannot also carry a `#[fallback]`, because the set delegation moves the
// inbound `Mail` the fallback then reads.
struct CatchAll;

#[actor(depends(Peer))]
impl WasmActor for CatchAll {
    const NAMESPACE: &'static str = "test.omitted_ctx.catch_all";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_trigger(&mut self, _ctx: &mut WasmCtx<'_>, _trigger: Trigger) {}

    #[fallback]
    fn on_other(&mut self, ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {
        ctx.send::<Peer>(&Ping { seq: 4 });
    }
}

fn main() {}
