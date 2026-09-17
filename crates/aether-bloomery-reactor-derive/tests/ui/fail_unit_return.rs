use aether_bloomery_reactor::reactor;

struct SourcePublisher;

#[reactor]
impl aether_bloomery_reactor::Reactor for SourcePublisher {
    const NAME: &'static str = "source.publisher";

    #[rule]
    fn publish(&self, _change: aether_bloomery_kinds::HeadMoved<aether_bloomery_kinds::Tree>) {}
}

fn main() {}
