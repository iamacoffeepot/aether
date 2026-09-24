//! `export!` takes keyed entries only. A bare call, a bare list, a bare list
//! followed by keys, and a bare entry after a key are compile errors that show
//! the keyed spelling or name the offending token; a repeated key and an
//! unknown key (the retired `foreign`) are refused by name.

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};

pub struct A;

#[actor]
impl WasmActor for A {
    const NAMESPACE: &'static str = "test.malformed_export.a";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

pub struct B;

#[actor]
impl WasmActor for B {
    const NAMESPACE: &'static str = "test.malformed_export.b";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

fn main() {}

mod bare_one {
    aether_actor::export!(super::A);
}

mod bare_list {
    aether_actor::export!(super::A, super::B);
}

mod bare_then_key {
    aether_actor::export!(super::A, private = [super::B]);
}

mod bare_after_key {
    aether_actor::export!(default = super::A, super::B);
}

mod repeated_key {
    aether_actor::export!(public = [super::A], public = [super::B]);
}

mod unknown_key {
    aether_actor::export!(public = [super::A], foreign = [super::B]);
}
