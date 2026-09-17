#![allow(unused)]

use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Output, Reactor, reactor};

#[aether_data::kind(name = "test.bloomery.reactor.ui.option_out", eq)]
struct PublicationProposal {
    marker: u32,
}

impl Output for PublicationProposal {}

struct SourcePublisher;

#[reactor]
impl Reactor for SourcePublisher {
    const NAME: &'static str = "source.publisher";

    #[rule]
    fn publish(&self, _change: HeadMoved<Tree>) -> Option<PublicationProposal> {
        Some(PublicationProposal { marker: 1 })
    }
}

fn main() {}
