//! ADR-0231 §10: a `Spawns<C>` impl names `C`'s position in the spawner's one
//! `Declared::Spawns` list, which `#[actor(spawns(..))]` writes from the same
//! list as the bound the `export!` coverage check reads. A hand-written impl
//! for an undeclared child names a position that holds another child, so it
//! is refused with `E0277` and no spawn skips the check.

use aether_actor::{ActorInitError, Here, Mail, Spawns, WasmActor, WasmCtx, WasmInitCtx, actor};

struct Parent;

#[actor(spawns(Exact))]
impl WasmActor for Parent {
    const NAMESPACE: &'static str = "test.hand_written_spawns.parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct Exact;

#[actor(instanced, child_of(Parent))]
impl WasmActor for Exact {
    const NAMESPACE: &'static str = "test.hand_written_spawns.exact";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct Other;

#[actor(instanced, child_of(Parent))]
impl WasmActor for Other {
    const NAMESPACE: &'static str = "test.hand_written_spawns.other";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

impl Spawns<Other> for Parent {
    type Index = Here;
}

fn main() {}
