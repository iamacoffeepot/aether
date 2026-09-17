// A mixed module must not use a reactor as the ordinary default.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, export};
use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Output, Reactor, reactor};

#[aether_data::kind(name = "test.bloomery.export.default_reactor_out", eq)]
struct Publication {
    marker: u32,
}

impl Output for Publication {}

pub struct Sink;

#[actor]
impl WasmActor for Sink {
    const NAMESPACE: &'static str = "test.bloomery.export.default_sink";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Sink)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

struct Publisher;

#[reactor]
impl Reactor for Publisher {
    const NAMESPACE: &'static str = "test.bloomery.export.default_publisher";

    #[rule]
    fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
        Publication { marker: 1 }
    }
}

export!(
    default = Publisher,
    Sink,
    generators = [aether_bloomery_reactor::ReactorBundle],
);

fn main() {}
