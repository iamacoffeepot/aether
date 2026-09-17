use aether_bloomery_reactor::{Output, reactor};

#[aether_data::kind(name = "test.bloomery.reactor.ui.ctx_out", eq)]
struct PublicationProposal {
    marker: u32,
}

impl Output for PublicationProposal {}

struct EngineCtx;

struct SourcePublisher;

#[reactor]
impl aether_bloomery_reactor::Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "source.publisher";

    #[rule]
    fn publish(&self, _change: aether_bloomery_kinds::HeadMoved<aether_bloomery_kinds::Tree>, _ctx: &mut EngineCtx) -> PublicationProposal {
        PublicationProposal { marker: 1 }
    }
}

fn main() {}
