// Distinct modules may share a short type name; identities are namespace-based.

use aether_actor::export;
use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Output, Reactor, reactor};

#[aether_data::kind(name = "test.bloomery.export.dup_short_out", eq)]
struct Publication {
    marker: u32,
}

impl Output for Publication {}

mod one {
    use super::*;

    pub struct Publisher;

    #[reactor]
    impl Reactor for Publisher {
        const NAMESPACE: &'static str = "test.bloomery.export.dup_short_one";

        #[rule]
        fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
            Publication { marker: 1 }
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
        fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
            Publication { marker: 2 }
        }
    }
}

export!(one::Publisher, two::Publisher, generators = [aether_bloomery_bundle::bundle]);

fn main() {}
