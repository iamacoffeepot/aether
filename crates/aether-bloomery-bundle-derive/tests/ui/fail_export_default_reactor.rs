// A mixed module must not use a reactor as the ordinary default.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, export};
use aether_bloomery_kinds::{Head, HeadMoved, SetHead, Tree};
use aether_bloomery_reactor::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

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
    fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

export!(default = Publisher, public = [Sink], generators = [aether_bloomery_bundle::bundle]);

fn main() {}
