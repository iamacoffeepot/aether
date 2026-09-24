//! A typed inline spawn requires the spawner to declare the child in its
//! `#[actor(spawns(..))]`, so every `export!` that lists the spawner can check
//! that it lists the child. Spawning an undeclared child is a compile error at
//! the spawn site, naming `spawns(..)`, through either verb, even when every
//! type is listed.

use aether_actor::{ActorInitError, Mail, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};

struct Parent;

#[actor]
impl WasmActor for Parent {
    const NAMESPACE: &'static str = "test.undeclared_inline.parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct Exact;

#[actor(instanced, child_of(Parent))]
impl WasmActor for Exact {
    const NAMESPACE: &'static str = "test.undeclared_inline.exact";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct Composable;

#[actor(instanced, composable)]
impl WasmActor for Composable {
    const NAMESPACE: &'static str = "test.undeclared_inline.composable";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

fn spawn_undeclared(ctx: &mut WasmCtx<'_, Parent>) {
    let _ = ctx.spawn_inline_child::<Parent, Exact>(Subname::Named("exact"), &());
    let _ = ctx.spawn_inline::<Composable>(Subname::Named("composable"), &());
}

fn main() {
    let _ = spawn_undeclared;
}

// Every type is listed, so only the missing declarations fail. The test crate
// declares no `library` feature, so the shim's `cfg(feature = "library")` is
// allowed.
#[allow(unexpected_cfgs)] // aether-suppression-request: the trybuild crate declares no `library` feature, so the export shim's `cfg(feature = "library")` gate is an unknown value here
mod listed {
    aether_actor::export!(public = [super::Parent], private = [super::Exact, super::Composable]);
}
