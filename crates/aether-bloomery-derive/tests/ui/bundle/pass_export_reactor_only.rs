// Reactor-only export! becomes a single coordinator (existing single-actor export).

use aether_actor::export;
use aether_bloomery_kinds::{Head, HeadMoved, SetHeads, Tree};
use aether_bloomery_program::{Reactor, reactor};

const PUBLISHED: Head<Tree> = Head::new("published");

struct Publisher;

#[reactor]
impl Reactor for Publisher {
    const NAMESPACE: &'static str = "test.bloomery.export.publisher";

    #[rule]
    fn publish(&self, change: HeadMoved<Tree>) -> SetHeads {
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, change.to())])
    }
}

export!(public = [Publisher], generators = [aether_bloomery_program::bundle]);

fn main() {
    let _ = Publisher::NAMESPACE;
    let _ = aether_bloomery_program::BUNDLE_NAMESPACE;
}
