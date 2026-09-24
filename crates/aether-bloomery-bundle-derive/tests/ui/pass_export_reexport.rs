// `pub use` of a reactor type carries the companion into the reexporting module.

use aether_actor::export;
use aether_bloomery_kinds::{Head, HeadMoved, SetHead, Tree};
use aether_bloomery_reactor::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

mod inner {
    use super::*;

    pub struct Publisher;

    #[reactor]
    impl Reactor for Publisher {
        const NAMESPACE: &'static str = "test.bloomery.export.reexport";

        #[rule]
        fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
            SetHead::new(&PUBLISHED, None, change.to())
        }
    }
}

pub use inner::Publisher;

export!(public = [Publisher], generators = [aether_bloomery_bundle::bundle]);

fn main() {}
