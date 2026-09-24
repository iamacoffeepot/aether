// Distinct modules may share a short type name; identities are namespace-based.

use aether_actor::export;
use aether_bloomery_kinds::{Head, HeadMoved, SetHead, Tree};
use aether_bloomery_reactor::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

mod one {
    use super::*;

    pub struct Publisher;

    #[reactor]
    impl Reactor for Publisher {
        const NAMESPACE: &'static str = "test.bloomery.export.dup_short_one";

        #[rule]
        fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
            SetHead::new(&PUBLISHED, None, change.to())
        }
    }
}

mod two {
    use super::*;

    pub struct Publisher;

    #[reactor]
    impl Reactor for Publisher {
        const NAMESPACE: &'static str = "test.bloomery.export.dup_short_two";

        #[rule]
        fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
            SetHead::new(&PUBLISHED, None, change.to())
        }
    }
}

export!(public = [one::Publisher, two::Publisher], generators = [aether_bloomery_bundle::bundle]);

fn main() {}
