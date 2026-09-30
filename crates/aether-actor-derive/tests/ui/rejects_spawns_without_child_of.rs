//! ADR-0114 / ADR-0166 (issue 7210): a spawner's `spawns(C)` and the child's
//! `child_of(..)` agree by construction. Each emitted `Spawns<C>` impl names
//! the spawner's position in `C`'s `Declared::Parents` as `Placement`, so a
//! `spawns(C)` whose `C` does not list the spawner fails at the declaration.

use aether_actor::{ActorInitError, Mail, WasmActor, WasmCtx, WasmInitCtx, actor};

struct Spawner;

#[actor(spawns(Child))]
impl WasmActor for Spawner {
    const NAMESPACE: &'static str = "test.spawns_without_child_of.spawner";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct Elsewhere;

#[actor]
impl WasmActor for Elsewhere {
    const NAMESPACE: &'static str = "test.spawns_without_child_of.elsewhere";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct Child;

#[actor(instanced, child_of(Elsewhere))]
impl WasmActor for Child {
    const NAMESPACE: &'static str = "test.spawns_without_child_of.child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

fn main() {}
