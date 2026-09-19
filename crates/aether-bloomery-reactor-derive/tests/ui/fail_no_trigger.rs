use aether_bloomery_kinds::{Digest, Head, Ref, SetHead, Tree};
use aether_bloomery_reactor::reactor;

const PUBLISHED: Head<Tree> = Head::new("published");

struct SourcePublisher;

#[reactor]
impl aether_bloomery_reactor::Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "source.publisher";

    #[rule]
    fn publish(&self) -> SetHead {
        SetHead::new(&PUBLISHED, None, Ref::from_digest(Digest::from_bytes([0; 32])))
    }
}

fn main() {}
