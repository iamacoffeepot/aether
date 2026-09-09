//! Smoke component for input subscriptions. It subscribes to the
//! substrate-published input kinds (`Key`, `MouseMove`, `MouseButton`) through
//! the `aether.window` facade and gives each one a handler, which is all it
//! takes to receive them: nothing subscribes on a component's behalf, so a
//! component that wants input asks for it from `wire`.
//!
//! The handlers are empty. Running this component exercises the subscription
//! and dispatch path end to end, visible in whatever tracing the substrate
//! captures, without producing output of its own.
//!
//! `Tick` is a frame-lifecycle stage on `aether.lifecycle`, not an input
//! stream, so it is not part of this demo.

// Stateless logger: each handler keeps `&mut self` for the dispatch ABI but
// touches no fields.
#![allow(clippy::unused_self)]

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::{Key, MouseButton, MouseMove};
use aether_window::{WindowCapability, WindowManagerMailboxExt, WindowSelector};

pub struct InputLogger;

#[actor]
impl WasmActor for InputLogger {
    const NAMESPACE: &'static str = "example.input_logger";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(InputLogger)
    }

    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) {
        let window = ctx.actor::<WindowCapability>();
        window.subscribe::<Key>(WindowSelector::All);
        window.subscribe::<MouseMove>(WindowSelector::All);
        window.subscribe::<MouseButton>(WindowSelector::All);
    }

    #[handler::single]
    fn on_key(&mut self, _ctx: &mut WasmCtx<'_>, _key: Key) {}

    #[handler::single]
    fn on_mouse_button(&mut self, _ctx: &mut WasmCtx<'_>, _mb: MouseButton) {}

    #[handler::single]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, _m: MouseMove) {}
}

aether_actor::export!(InputLogger);
