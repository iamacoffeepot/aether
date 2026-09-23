use aether_actor::{Addressable, Contract, DependsOn, One, Silent, WasmCtx};
use aether_kinds::{Key, Tick};
use aether_lifecycle::LifecycleCapability;
use aether_window::WindowCapability;

struct Subscriber;

impl Addressable for Subscriber {
    const NAMESPACE: &'static str = "test.subscriber";
    type Resolver = One;
}

impl DependsOn<LifecycleCapability> for Subscriber {}
impl DependsOn<WindowCapability> for Subscriber {}

impl Contract<Tick> for Subscriber {
    type Reply = Silent;
}

impl Contract<Key> for Subscriber {
    type Reply = Silent;
}

fn rejects_window_stage_subscription(ctx: &mut WasmCtx<'_, Subscriber>) {
    ctx.subscribe::<WindowCapability, Tick>();
}

fn rejects_lifecycle_device_subscription(ctx: &mut WasmCtx<'_, Subscriber>) {
    ctx.subscribe::<LifecycleCapability, Key>();
}

fn main() {}
