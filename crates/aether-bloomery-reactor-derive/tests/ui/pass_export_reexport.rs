// `pub use` of a reactor type carries the companion into the reexporting module.

use aether_actor::export;
use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Output, Reactor, reactor};

#[aether_data::kind(name = "test.bloomery.export.reexport_out", eq)]
struct Publication {
    marker: u32,
}

impl Output for Publication {}

mod inner {
    use super::*;

    pub struct Publisher;

    #[reactor]
    impl Reactor for Publisher {
        const NAMESPACE: &'static str = "test.bloomery.export.reexport";

        #[rule]
        fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
            Publication { marker: 1 }
        }
    }
}

pub use inner::Publisher;

export!(Publisher, generators = [aether_bloomery_reactor::ReactorBundle]);

fn main() {}
