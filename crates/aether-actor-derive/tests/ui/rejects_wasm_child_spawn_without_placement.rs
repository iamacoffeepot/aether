use aether_actor::{ActorInitError, Mail, Subname, WasmActor, WasmCtx, WasmInitCtx, actor};

struct Parent;

#[actor]
impl WasmActor for Parent {
    const NAMESPACE: &'static str = "test.spawn.parent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

struct UnplacedChild;

#[actor(instanced)]
impl WasmActor for UnplacedChild {
    const NAMESPACE: &'static str = "test.spawn.unplaced_child";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {}
}

fn spawn(ctx: &WasmCtx<'_>) {
    let _ = ctx.spawn_child::<Parent, UnplacedChild>(Subname::Named("child"), &());
}

fn main() {}
