// Reactor-only export! becomes a single coordinator (existing single-actor export).

use aether_actor::export;
use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Output, Reactor, reactor};

#[aether_data::kind(name = "test.bloomery.export.reactor_only_out", eq)]
struct Publication {
    marker: u32,
}

impl Output for Publication {}

struct Publisher;

#[reactor]
impl Reactor for Publisher {
    const NAMESPACE: &'static str = "test.bloomery.export.publisher";

    #[rule]
    fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
        Publication { marker: 1 }
    }
}

export!(Publisher, generators = [aether_bloomery_bundle::bundle]);

fn main() {
    let _ = Publisher::NAMESPACE;
    let _ = aether_bloomery_bundle::BUNDLE_NAMESPACE;
}
