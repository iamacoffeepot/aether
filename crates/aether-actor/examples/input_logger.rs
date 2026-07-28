//! Smoke component for ADR-0021 input subscriptions. Observes the
//! substrate-published input kinds (Key / `MouseMove` / `MouseButton`)
//! and counts each dispatch. `Tick` is a frame-lifecycle stage
//! (`aether.lifecycle`, ADR-0082), not an input stream, so it is not
//! part of this input-streams demo.
//!
//! Pre-issue-775 the example emitted a `demo.input_observed { stream,
//! code }` to `hub.claude.broadcast` so the driving Claude session
//! saw each dispatch land via `receive_mail`. With
//! `BroadcastCapability` retired the broadcast goes away; handlers
//! still run (and trigger any tracing the substrate captures), but no
//! observation kind is emitted.
//!
//! Each input kind has its own `#[handler]` method. Issue 640 retired
//! the cap-side manifest auto-subscribe walker (and the macro-side
//! walker retired earlier in #403); components subscribe from the
//! `wire` hook through the typed `aether.window` facade.

// Stateless logger: each `#[handler]` keeps `&mut self` for the
// ADR-0033 / ADR-0038 dispatch ABI but doesn't need any field access.
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
