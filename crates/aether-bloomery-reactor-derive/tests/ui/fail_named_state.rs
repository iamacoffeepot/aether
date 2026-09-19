use aether_bloomery_kinds::{Head, SetHead, Tree};
use aether_bloomery_reactor::reactor;

const PUBLISHED: Head<Tree> = Head::new("published");

struct StatefulReactor {
    marker: u32,
}

#[reactor]
impl aether_bloomery_reactor::Reactor for StatefulReactor {
    const NAMESPACE: &'static str = "stateful.named";

    #[rule]
    fn publish(&self, change: aether_bloomery_kinds::HeadMoved<aether_bloomery_kinds::Tree>) -> SetHead {
        let _ = self.marker;
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

fn main() {}
