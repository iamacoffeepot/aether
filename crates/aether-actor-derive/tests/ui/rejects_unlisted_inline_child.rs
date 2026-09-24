//! Every `export!` checks that it lists each inline child a type it lists
//! declares in `#[actor(spawns(..))]`, because that listing is the set a
//! `replace_component` rebuilds. A declared child the `export!` lists neither
//! as exported nor under `private = [..]` is a compile error at the `export!`,
//! naming the `private` key, for either typed verb.

use aether_actor::{ActorInitError, Mail, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};

struct Parent;

#[actor(spawns(Exact, Composable))]
impl WasmActor for Parent {
    const NAMESPACE: &'static str = "test.unlisted_inline.parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct Exact;

#[actor(instanced, child_of(Parent))]
impl WasmActor for Exact {
    const NAMESPACE: &'static str = "test.unlisted_inline.exact";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct Composable;

#[actor(instanced, composable)]
impl WasmActor for Composable {
    const NAMESPACE: &'static str = "test.unlisted_inline.composable";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

fn spawn_declared(ctx: &mut WasmCtx<'_, Parent>) {
    let _ = ctx.spawn_inline_child::<Parent, Exact>(Subname::Named("exact"), &());
    let _ = ctx.spawn_inline::<Composable>(Subname::Named("composable"), &());
}

fn main() {
    let _ = spawn_declared;
}

// Only `Parent` is listed. The test crate declares no `library` feature, so the
// shim's `cfg(feature = "library")` is allowed.
#[allow(unexpected_cfgs)] // aether-suppression-request: the trybuild crate declares no `library` feature, so the export shim's `cfg(feature = "library")` gate is an unknown value here
mod listed {
    aether_actor::export!(public = [super::Parent]);
}
