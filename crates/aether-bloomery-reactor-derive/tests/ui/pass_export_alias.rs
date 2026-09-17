// Import aliases carry the same-name companion macro.

use aether_actor::export;
use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Output, Reactor, reactor};

#[aether_data::kind(name = "test.bloomery.export.alias_out", eq)]
struct Publication {
    marker: u32,
}

impl Output for Publication {}

mod inner {
    use super::*;

    pub struct Publisher;

    #[reactor]
    impl Reactor for Publisher {
        const NAMESPACE: &'static str = "test.bloomery.export.alias";

        #[rule]
        fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
            Publication { marker: 1 }
        }
    }
}

use inner::Publisher as Alias;

export!(Alias, generators = [aether_bloomery_reactor::ReactorBundle]);

fn main() {}
