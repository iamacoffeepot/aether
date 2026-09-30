//! ADR-0231 §10: a `Rebuildable<M>` impl names the type's position in the
//! module's one `ListedModule::Listed` list, which `export!` writes from the
//! types it lists. A hand-written impl for a type the `export!` does not list
//! names a position that holds another type, so it is refused with `E0277`,
//! and no child passes the coverage check while the rebuild arm omits it.

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};

struct A;

#[actor]
impl WasmActor for A {
    const NAMESPACE: &'static str = "test.hand_written_rebuildable.a";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct B;

#[actor(instanced, child_of(A))]
impl WasmActor for B {
    const NAMESPACE: &'static str = "test.hand_written_rebuildable.b";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

fn main() {}

// The test crate declares no `library` feature, so the shim's
// `cfg(feature = "library")` is allowed.
#[allow(unexpected_cfgs)] // aether-suppression-request: the trybuild crate declares no `library` feature, so the export shim's `cfg(feature = "library")` gate is an unknown value here
mod listed {
    aether_actor::export!(public = [super::A]);

    impl aether_actor::Rebuildable<__AetherModule> for super::B {
        type Index = aether_actor::Here;
    }
}
