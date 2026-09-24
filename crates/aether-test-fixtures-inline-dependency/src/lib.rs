//! ADR-0230 §3 fixture: an inline-spawnable actor with a declared dependency.
//!
//! `Holder` is the module's default export. Its `wire` spawns `Needy`
//! inline, and `Needy` is `composable`, so the module's lineage section
//! carries a `ModuleChild` record for it. `Needy` declares
//! `depends(ClipboardCapability)`: the host checks that dependency when the
//! module loads, before `Holder` runs, and refuses the load while no
//! clipboard actor is live.

#![forbid(unsafe_code)]
#![allow(clippy::unused_self)] // aether-suppression-request: the ADR-0033 dispatch ABI fixes the handler signature at `&mut self`, and both actors are stateless — the same allow the defaultless fixture carries

use aether_actor::{ActorInitError, Subname, WasmActor, WasmCtx, WasmInitCtx, WireCtx, actor};
use aether_clipboard::ClipboardCapability;
use aether_kinds::Ping;

/// Default export: spawns the composable `Needy` inline once wired.
pub struct Holder;

#[actor(spawns(Needy))]
impl WasmActor for Holder {
    const NAMESPACE: &'static str = "test.inline_dependency.holder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Holder)
    }

    fn wire(&mut self, ctx: &mut WireCtx<'_, '_>) {
        let _ = ctx.spawn_inline::<Needy>(Subname::Named("needy"), &());
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

/// The composable child whose declared dependency the host checks at module
/// load.
pub struct Needy;

#[actor(instanced, composable, depends(ClipboardCapability))]
impl WasmActor for Needy {
    const NAMESPACE: &'static str = "test.inline_dependency.needy";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Needy)
    }

    #[handler::single]
    fn on_ping(&mut self, _ctx: &mut WasmCtx<'_>, _ping: Ping) {}
}

aether_actor::export!(default = Holder, public = [Needy]);
