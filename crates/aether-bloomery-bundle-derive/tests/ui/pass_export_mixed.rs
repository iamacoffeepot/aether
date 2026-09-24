// Mixed ordinary actors + reactors: Probe stays default, sink stays an export,
// only Publisher/Witness become the cluster.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, export};
use aether_bloomery_kinds::{Head, HeadMoved, SetHead, Tree};
use aether_bloomery_reactor::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

pub struct Probe;

#[actor]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "test.bloomery.export.probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Probe)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

pub struct Sink;

#[actor]
impl WasmActor for Sink {
    const NAMESPACE: &'static str = "test.bloomery.export.sink";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Sink)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

struct Publisher;

#[reactor]
impl Reactor for Publisher {
    const NAMESPACE: &'static str = "test.bloomery.export.mixed_publisher";

    #[rule]
    fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

struct Witness;

#[reactor]
impl Reactor for Witness {
    const NAMESPACE: &'static str = "test.bloomery.export.mixed_witness";

    #[rule]
    fn note(&self, change: HeadMoved<Tree>) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

export!(
    default = Probe,
    public = [Publisher, Witness, Sink],
    generators = [aether_bloomery_bundle::bundle],
);

fn main() {
    let _ = Probe;
    let _ = Sink;
}
