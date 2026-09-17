// Qualified reactor paths invoke the same-name companion in that module.

use aether_actor::export;
use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Output, Reactor, reactor};

#[aether_data::kind(name = "test.bloomery.export.qualified_out", eq)]
struct Publication {
    marker: u32,
}

impl Output for Publication {}

mod inner {
    use super::*;

    pub struct Publisher;

    #[reactor]
    impl Reactor for Publisher {
        const NAMESPACE: &'static str = "test.bloomery.export.qualified";

        #[rule]
        fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
            Publication { marker: 1 }
        }
    }
}

export!(inner::Publisher, generators = [aether_bloomery_reactor::ReactorBundle]);

fn main() {}
