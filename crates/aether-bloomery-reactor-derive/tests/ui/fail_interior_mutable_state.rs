use core::cell::Cell;

use aether_bloomery_kinds::{Head, SetHead, Tree};
use aether_bloomery_reactor::reactor;

const PUBLISHED: Head<Tree> = Head::new("published");

struct StatefulReactor {
    marker: Cell<u32>,
}

#[reactor]
impl aether_bloomery_reactor::Reactor for StatefulReactor {
    const NAMESPACE: &'static str = "stateful.interior_mutable";

    #[rule]
    fn publish(&self, change: aether_bloomery_kinds::HeadMoved<aether_bloomery_kinds::Tree>) -> SetHead {
        let _ = self.marker.get();
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

fn main() {}
