use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Output, reactor};

#[aether_data::kind(name = "test.bloomery.export.invalid_names_out", eq)]
struct Publication {
    marker: u32,
}

impl Output for Publication {}

struct BadNamespace;

#[reactor]
impl Reactor for BadNamespace {
    const NAMESPACE: &'static str = "Bad.Name";

    #[rule]
    fn publish(&self, _change: HeadMoved<Tree>) -> Publication {
        Publication { marker: 1 }
    }
}

struct HiddenRule;

#[reactor]
impl Reactor for HiddenRule {
    const NAMESPACE: &'static str = "test.bloomery.hidden";

    #[rule]
    fn _hidden(&self, _change: HeadMoved<Tree>) -> Publication {
        Publication { marker: 1 }
    }
}

fn main() {}
