use aether_actor::{Addressable, Contract, DependsOn, One, WasmCtx};
use aether_kinds::{Key, Tick};
use aether_lifecycle::LifecycleCapability;

struct Subscriber;

impl Addressable for Subscriber {
    const NAMESPACE: &'static str = "test.subscriber";
    type Resolver = One;
}

impl DependsOn<LifecycleCapability> for Subscriber {}

impl Contract<Tick> for Subscriber {
    type Reply = Key;
}

fn rejects_replying_tick_handler(ctx: &mut WasmCtx<'_, Subscriber>) {
    ctx.subscribe::<LifecycleCapability, Tick>();
}

fn main() {}
