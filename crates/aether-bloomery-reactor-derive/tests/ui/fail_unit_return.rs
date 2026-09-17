#![allow(unused)]

use aether_bloomery_kinds::{HeadMoved, Tree};
use aether_bloomery_reactor::{Reactor, reactor};

struct SourcePublisher;

#[reactor]
impl Reactor for SourcePublisher {
    const NAME: &'static str = "source.publisher";

    #[rule]
    fn publish(&self, _change: HeadMoved<Tree>) {}
}

fn main() {}
