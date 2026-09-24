//! A typed inline spawn requires the child to be listed by its module's
//! `export!`, exported or under `private = [..]`, because that listing is the
//! set a `replace_component` rebuilds. Spawning an unlisted child is a compile
//! error naming the `private` slot, through either verb.

use aether_actor::{ActorInitError, Mail, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};

struct Parent;

#[actor]
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

fn spawn_unlisted(ctx: &mut WasmCtx<'_>) {
    let _ = ctx.spawn_inline_child::<Parent, Exact>(Subname::Named("exact"), &());
    let _ = ctx.spawn_inline::<Composable>(Subname::Named("composable"), &());
}

fn main() {}

// Only `Parent` is listed. The test crate declares no `library` feature, so the
// shim's `cfg(feature = "library")` is allowed.
#[allow(unexpected_cfgs)] // aether-suppression-request: the trybuild crate declares no `library` feature, so the export shim's `cfg(feature = "library")` gate is an unknown value here
mod listed {
    aether_actor::export!(super::Parent);
}
