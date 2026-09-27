use aether_actor::__macro_internals::{KindId, ReplyContract};
use aether_actor::{Addressable, Contract, Contracts, Here, One, Row, Silent, WasmCtx};
use aether_kinds::Tick;
use aether_lifecycle::LifecycleCapability;

struct Subscriber;

impl Addressable for Subscriber {
    const NAMESPACE: &'static str = "test.subscriber";
    type Resolver = One;
}

impl Contracts for Subscriber {
    type Rows = (Row<Tick, Silent>, ());
    const CONTRACTS: &'static [(KindId, ReplyContract)] = &[];
}

impl Contract<Tick> for Subscriber {
    type Reply = Silent;
    type Index = Here;
}

fn rejects_undeclared_lifecycle_subscription(ctx: &mut WasmCtx<'_, Subscriber>) {
    ctx.subscribe::<LifecycleCapability, Tick>();
}

fn main() {}
