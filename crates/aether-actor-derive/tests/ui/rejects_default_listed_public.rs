//! A `default` (or `boot`) type is exported already, so listing it again under
//! `public` is a compile error: the `export!` would mark it twice, which is a
//! conflicting marker impl rather than a silently deduplicated export.

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};

pub struct A;

#[actor]
impl WasmActor for A {
    const NAMESPACE: &'static str = "test.default_listed_public.a";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

pub struct B;

#[actor]
impl WasmActor for B {
    const NAMESPACE: &'static str = "test.default_listed_public.b";

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
    aether_actor::export!(default = super::A, public = [super::A, super::B]);
}
