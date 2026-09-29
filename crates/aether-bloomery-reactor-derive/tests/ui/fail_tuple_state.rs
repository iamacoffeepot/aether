use aether_bloomery_kinds::{Head, SetHeads, Tree};
use aether_bloomery_reactor::reactor;

const PUBLISHED: Head<Tree> = Head::new("published");

struct StatefulReactor(u32);

#[reactor]
impl aether_bloomery_reactor::Reactor for StatefulReactor {
    const NAMESPACE: &'static str = "stateful.tuple";

    #[rule]
    fn publish(&self, change: aether_bloomery_kinds::HeadMoved<aether_bloomery_kinds::Tree>) -> SetHeads {
        let _ = self.0;
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, change.to())])
    }
}

fn main() {}
