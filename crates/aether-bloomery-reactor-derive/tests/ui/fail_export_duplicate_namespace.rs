use aether_actor::export;
use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Output, Reactor, reactor};

#[aether_data::kind(name = "test.bloomery.export.dup_out", eq)]
struct Publication {
    marker: u32,
}

impl Output for Publication {}

struct First;

#[reactor]
impl Reactor for First {
    const NAMESPACE: &'static str = "test.bloomery.export.dup";

    #[rule]
    fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
        Publication { marker: 1 }
    }
}

struct Second;

#[reactor]
impl Reactor for Second {
    const NAMESPACE: &'static str = "test.bloomery.export.dup";

    #[rule]
    fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
        Publication { marker: 2 }
    }
}

export!(First, Second, generators = [aether_bloomery_reactor::ReactorBundle]);

fn main() {}
