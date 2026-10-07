//! ADR-0231 §7, §11: a ctx's type arguments are receiver, sender, mode. A
//! two-argument spelling from before the ctx carried its sender puts a reply
//! mode where the sender goes, and `#[actor]` refuses it on a handler and on
//! a `#[fallback]` with the three-argument spelling to write. Reading it as a
//! sender requirement would refuse every piece of mail the handler exists to
//! take, since no actor covers a reply mode.
//!
//! Off a handler the same spelling fails where the ctx is used: a helper
//! that takes it has no `sender()` and none of the `Unchecked` mode's
//! methods, which the last case pins.

use aether_actor::{ActorInitError, Mail, OutboundReply, Unchecked, WasmActor, WasmCtx, WasmInitCtx, actor};

#[aether_data::kind(name = "test.stale_ctx.tick", copy)]
struct Tick;

struct OnUnchecked;

#[actor(root)]
impl WasmActor for OnUnchecked {
    const NAMESPACE: &'static str = "test.stale_ctx.on_unchecked";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::unchecked(reason = "test: replies by hand")]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_, Self, Unchecked>, _mail: Tick) {}
}

struct OnTell;

#[actor(root)]
impl WasmActor for OnTell {
    const NAMESPACE: &'static str = "test.stale_ctx.on_tell";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::tell]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_, Self, aether_actor::Single>, _mail: Tick) {}
}

struct OnFallback;

#[actor(root)]
impl WasmActor for OnFallback {
    const NAMESPACE: &'static str = "test.stale_ctx.on_fallback";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_any(&mut self, _ctx: &mut WasmCtx<'_, Self, Unchecked>, _mail: Mail<'_>) {}
}

fn stale_helper<A>(ctx: &mut WasmCtx<'_, A, Unchecked>) {
    let _ = ctx.sender();
    ctx.reply(&Tick);
}

fn main() {
    let _ = stale_helper::<OnUnchecked>;
}
