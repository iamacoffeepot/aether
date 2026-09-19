use aether_bloomery_kinds::{Head, SetHead, Tree};
use aether_bloomery_reactor::reactor;

const PUBLISHED: Head<Tree> = Head::new("published");

struct EngineCtx;

struct SourcePublisher;

#[reactor]
impl aether_bloomery_reactor::Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "source.publisher";

    #[rule]
    fn publish(&self, change: aether_bloomery_kinds::HeadMoved<aether_bloomery_kinds::Tree>, _ctx: &mut EngineCtx) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

fn main() {}
