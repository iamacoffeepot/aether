use aether_bloomery_kinds::{Head, SetHeads, Tree};
use aether_bloomery_program::reactor;

const PUBLISHED: Head<Tree> = Head::new("published");

struct SourcePublisher;

#[reactor]
impl aether_bloomery_program::Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "source.publisher";

    #[rule]
    fn publish(&self, change: aether_bloomery_kinds::HeadMoved<aether_bloomery_kinds::Tree>) -> Option<SetHeads> {
        Some(SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, change.to())]))
    }
}

fn main() {}
