use aether_bloomery_kinds::{Digest, Head, Ref, SetHeads, Tree};
use aether_bloomery_program::reactor;

const PUBLISHED: Head<Tree> = Head::new("published");

struct SourcePublisher;

#[reactor]
impl aether_bloomery_program::Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "source.publisher";

    #[rule]
    fn publish(&self) -> SetHeads {
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, Ref::from_digest(Digest::from_bytes([0; 32])))])
    }
}

fn main() {}
