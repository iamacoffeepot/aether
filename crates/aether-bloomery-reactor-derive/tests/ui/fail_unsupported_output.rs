// Catches a rule output outside `CallProgram` / `SetHead` compiling into a bundle whose every intent the driver refuses at runtime.

use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::reactor;

#[aether_data::kind(name = "test.bloomery.reactor.ui.unsupported_out", eq)]
struct Publication {
    marker: u32,
}

struct SourcePublisher;

#[reactor]
impl aether_bloomery_reactor::Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "source.publisher";

    #[rule]
    fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
        Publication { marker: 1 }
    }
}

fn main() {}
