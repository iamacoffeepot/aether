use core::cell::Cell;

use aether_bloomery_reactor::{Output, reactor};

#[aether_data::kind(name = "test.bloomery.reactor.ui.interior_mutable_state_out", eq)]
struct PublicationProposal {
    marker: u32,
}

impl Output for PublicationProposal {}

struct StatefulReactor {
    marker: Cell<u32>,
}

#[reactor]
impl aether_bloomery_reactor::Reactor for StatefulReactor {
    const NAMESPACE: &'static str = "stateful.interior_mutable";

    #[rule]
    fn publish(&self, _change: aether_bloomery_kinds::HeadMoved<aether_bloomery_kinds::Tree>) -> PublicationProposal {
        PublicationProposal { marker: self.marker.get() }
    }
}

fn main() {}
