use aether_actor::export;
use aether_bloomery_kinds::{Head, HeadMoved, SetHead, Tree};
use aether_bloomery_reactor::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

struct First;

#[reactor]
impl Reactor for First {
    const NAMESPACE: &'static str = "test.bloomery.export.dup";

    #[rule]
    fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

struct Second;

#[reactor]
impl Reactor for Second {
    const NAMESPACE: &'static str = "test.bloomery.export.dup";

    #[rule]
    fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

export!(public = [First, Second], generators = [aether_bloomery_bundle::bundle]);

fn main() {}
