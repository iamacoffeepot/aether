use core::cell::Cell;

use aether_bloomery_kinds::{Head, SetHeads, Tree};
use aether_bloomery_program::reactor;

const PUBLISHED: Head<Tree> = Head::new("published");

struct StatefulReactor {
    marker: Cell<u32>,
}

#[reactor]
impl aether_bloomery_program::Reactor for StatefulReactor {
    const NAMESPACE: &'static str = "stateful.interior_mutable";

    #[rule]
    fn publish(&self, change: aether_bloomery_kinds::HeadMoved<aether_bloomery_kinds::Tree>) -> SetHeads {
        let _ = self.marker.get();
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, change.to())])
    }
}

fn main() {}
