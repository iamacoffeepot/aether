#![allow(unused)]

use aether_bloomery_reactor::{Output, Reactor, reactor};

#[aether_data::kind(name = "test.bloomery.reactor.ui.no_trigger_out", eq)]
struct PublicationProposal {
    marker: u32,
}

impl Output for PublicationProposal {}

struct SourcePublisher;

#[reactor]
impl Reactor for SourcePublisher {
    const NAME: &'static str = "source.publisher";

    #[rule]
    fn publish(&self) -> PublicationProposal {
        PublicationProposal { marker: 1 }
    }
}

fn main() {}
