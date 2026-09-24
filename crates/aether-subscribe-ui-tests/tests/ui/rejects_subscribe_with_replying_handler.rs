use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::{Key, Tick};
use aether_lifecycle::LifecycleCapability;

struct Subscriber;

#[actor(depends(LifecycleCapability))]
impl WasmActor for Subscriber {
    const NAMESPACE: &'static str = "test.subscriber";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_>, _tick: Tick) -> Key {
        Key::default()
    }
}

fn rejects_replying_tick_handler(ctx: &mut WasmCtx<'_, Subscriber>) {
    ctx.subscribe::<LifecycleCapability, Tick>();
}

fn main() {}
