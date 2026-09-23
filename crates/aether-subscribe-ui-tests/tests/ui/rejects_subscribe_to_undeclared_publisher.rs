use aether_actor::{Addressable, Contract, One, Silent, WasmCtx};
use aether_kinds::Tick;
use aether_lifecycle::LifecycleCapability;

struct Subscriber;

impl Addressable for Subscriber {
    const NAMESPACE: &'static str = "test.subscriber";
    type Resolver = One;
}

impl Contract<Tick> for Subscriber {
    type Reply = Silent;
}

fn rejects_undeclared_lifecycle_subscription(ctx: &mut WasmCtx<'_, Subscriber>) {
    ctx.subscribe::<LifecycleCapability, Tick>();
}

fn main() {}
