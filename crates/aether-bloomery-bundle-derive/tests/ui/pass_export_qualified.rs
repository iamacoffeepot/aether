// Qualified reactor paths invoke the same-name companion in that module.

use aether_actor::export;
use aether_bloomery_kinds::{Head, HeadMoved, SetHead, Tree};
use aether_bloomery_reactor::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

mod inner {
    use super::*;

    pub struct Publisher;

    #[reactor]
    impl Reactor for Publisher {
        const NAMESPACE: &'static str = "test.bloomery.export.qualified";

        #[rule]
        fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
            SetHead::new(&PUBLISHED, None, change.to())
        }
    }
}

export!(public = [inner::Publisher], generators = [aether_bloomery_bundle::bundle]);

fn main() {}
