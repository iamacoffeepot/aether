use aether_bloomery_kinds::{Head, HeadMoved, SetHead, Tree};
use aether_bloomery_reactor::reactor;

const PUBLISHED: Head<Tree> = Head::new("published");

struct BadNamespace;

#[reactor]
impl Reactor for BadNamespace {
    const NAMESPACE: &'static str = "Bad.Name";

    #[rule]
    fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

struct HiddenRule;

#[reactor]
impl Reactor for HiddenRule {
    const NAMESPACE: &'static str = "test.bloomery.hidden";

    #[rule]
    fn _hidden(&self, change: HeadMoved<Tree>) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

fn main() {}
