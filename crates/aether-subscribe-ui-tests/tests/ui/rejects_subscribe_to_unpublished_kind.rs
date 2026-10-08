use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_kinds::Tick;
use aether_window::Key;
use aether_lifecycle::LifecycleCapability;
use aether_window::WindowCapability;

struct Subscriber;

#[actor(depends(LifecycleCapability, WindowCapability))]
impl WasmActor for Subscriber {
    const NAMESPACE: &'static str = "test.subscriber";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[handler::event]
    fn on_tick(&mut self, _ctx: &mut WasmCtx<'_>, _tick: Tick) {}

    #[handler::event]
    fn on_key(&mut self, _ctx: &mut WasmCtx<'_>, _key: Key) {}
}

fn rejects_window_stage_subscription(ctx: &mut WasmCtx<'_, Subscriber>) {
    ctx.subscribe::<WindowCapability, Tick>();
}

fn rejects_lifecycle_device_subscription(ctx: &mut WasmCtx<'_, Subscriber>) {
    ctx.subscribe::<LifecycleCapability, Key>();
}

fn main() {}
