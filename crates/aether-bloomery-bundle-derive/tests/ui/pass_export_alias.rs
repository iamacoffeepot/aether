// Import aliases carry the same-name companion macro.

use aether_actor::export;
use aether_bloomery_kinds::{Head, HeadMoved, SetHead, Tree};
use aether_bloomery_reactor::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

mod inner {
    use super::*;

    pub struct Publisher;

    #[reactor]
    impl Reactor for Publisher {
        const NAMESPACE: &'static str = "test.bloomery.export.alias";

        #[rule]
        fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
            SetHead::new(&PUBLISHED, None, change.to())
        }
    }
}

use inner::Publisher as Alias;

export!(public = [Alias], generators = [aether_bloomery_bundle::bundle]);

fn main() {}
