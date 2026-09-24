// A `default` type is exported already, so listing it again under `public` is
// refused by name rather than silently deduplicated by the generator pipeline.

use aether_actor::{ActorInitError, WasmActor, WasmCtx, WasmInitCtx, actor, export};
use aether_bloomery_kinds::{Head, HeadMoved, SetHead, Tree};
use aether_bloomery_reactor::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

pub struct Probe;

#[actor]
impl WasmActor for Probe {
    const NAMESPACE: &'static str = "test.bloomery.export.default_public_probe";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Probe)
    }

    #[fallback]
    fn on_other(&mut self, _ctx: &mut WasmCtx<'_>, _mail: aether_actor::Mail<'_>) {}
}

struct Publisher;

#[reactor]
impl Reactor for Publisher {
    const NAMESPACE: &'static str = "test.bloomery.export.default_public_publisher";

    #[rule]
    fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

export!(default = Probe, public = [Probe, Publisher], generators = [aether_bloomery_bundle::bundle]);

fn main() {
    let _ = Probe;
}
