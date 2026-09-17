use aether_bloomery_reactor::{Output, reactor};

#[aether_data::kind(name = "test.bloomery.reactor.ui.no_trigger_out", eq)]
struct PublicationProposal {
    marker: u32,
}

impl Output for PublicationProposal {}

struct SourcePublisher;

#[reactor]
impl aether_bloomery_reactor::Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "source.publisher";

    #[rule]
    fn publish(&self) -> PublicationProposal {
        PublicationProposal { marker: 1 }
    }
}

fn main() {}
