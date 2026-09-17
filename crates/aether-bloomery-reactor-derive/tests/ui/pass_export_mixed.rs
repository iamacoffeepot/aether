// Mixed ordinary actors + reactors: Probe stays default, sink stays an export,
// only Publisher/Witness become the cluster.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, export};
use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Output, Reactor, reactor};

#[aether_data::kind(name = "test.bloomery.export.mixed_out", eq)]
struct Publication {
    marker: u32,
}

impl Output for Publication {}

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
    fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
        Publication { marker: 1 }
    }
}

struct Witness;

#[reactor]
impl Reactor for Witness {
    const NAMESPACE: &'static str = "test.bloomery.export.mixed_witness";

    #[rule]
    fn note(&self, _change: HeadMoved<Tree>) -> Publication {
        Publication { marker: 2 }
    }
}

export!(
    default = Probe,
    Publisher,
    Witness,
    Sink,
    generators = [aether_bloomery_reactor::ReactorBundle],
);

fn main() {
    let _ = Probe;
    let _ = Sink;
}
