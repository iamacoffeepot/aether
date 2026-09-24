// Reactor-only export! becomes a single coordinator (existing single-actor export).

use aether_actor::export;
use aether_bloomery_kinds::{Head, HeadMoved, SetHead, Tree};
use aether_bloomery_reactor::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

struct Publisher;

#[reactor]
impl Reactor for Publisher {
    const NAMESPACE: &'static str = "test.bloomery.export.publisher";

    #[rule]
    fn publish(&self, change: HeadMoved<Tree>) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

export!(public = [Publisher], generators = [aether_bloomery_bundle::bundle]);

fn main() {
    let _ = Publisher::NAMESPACE;
    let _ = aether_bloomery_bundle::BUNDLE_NAMESPACE;
}
