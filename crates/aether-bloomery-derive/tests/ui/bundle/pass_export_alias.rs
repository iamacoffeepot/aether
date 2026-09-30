// Import aliases carry the same-name companion macro.

use aether_actor::export;
use aether_bloomery_kinds::{Head, HeadMoved, SetHeads, Tree};
use aether_bloomery_program::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

mod inner {
    use super::*;

    pub struct Publisher;

    #[reactor]
    impl Reactor for Publisher {
        const NAMESPACE: &'static str = "test.bloomery.export.alias";

        #[rule]
        fn publish(&self, change: HeadMoved<Tree>) -> SetHeads {
            SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, change.to())])
        }
    }
}

use inner::Publisher as Alias;

export!(public = [Alias], generators = [aether_bloomery_program::bundle]);

fn main() {}
