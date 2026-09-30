use aether_bloomery_program::reactor;

struct SourcePublisher;

#[reactor]
impl aether_bloomery_program::Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "source.publisher";

    #[rule]
    fn publish(&self, _change: aether_bloomery_kinds::HeadMoved<aether_bloomery_kinds::Tree>) {}
}

fn main() {}
