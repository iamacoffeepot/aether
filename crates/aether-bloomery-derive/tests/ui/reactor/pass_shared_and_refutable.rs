// Two arms share Heads, and refutable enum trigger cases compile as declines.

use aether_bloomery_kinds::{Head, HeadMoved, SetHeads, Tree};
use aether_data::{Digest, Ref};
use aether_bloomery_program::{Guard, Reactor, reactor, At, Heads};

const SOURCE: Head<Tree> = Head::new("source");
const PUBLISHED: Head<Tree> = Head::new("published");

#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
#[kind(name = "test.bloomery.reactor.ui.compilation")]
enum Compilation {
    Succeeded { source: [u8; 32] },
    Failed { source: [u8; 32] },
}

struct BoundSource;

impl Guard<HeadMoved<Tree>> for BoundSource {
    type Views = Heads;

    fn resolve(_trigger: &HeadMoved<Tree>, _at: At, heads: &Heads) -> Option<Self> {
        heads.get(&SOURCE).map(|_| Self)
    }
}

impl Guard<Compilation> for BoundSource {
    type Views = Heads;

    fn resolve(_trigger: &Compilation, _at: At, heads: &Heads) -> Option<Self> {
        heads.get(&SOURCE).map(|_| Self)
    }
}

struct Publisher;

#[reactor]
impl Reactor for Publisher {
    const NAMESPACE: &'static str = "shared.publisher";

    #[rule]
    fn from_heads(&self, change: HeadMoved<Tree>, _heads: Heads) -> SetHeads {
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, change.to())])
    }

    #[rule]
    fn from_guard(&self, change: HeadMoved<Tree>, _bound: BoundSource) -> SetHeads {
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, change.to())])
    }

    #[rule]
    fn on_success(
        &self,
        Compilation::Succeeded { source }: Compilation,
        _bound: BoundSource,
    ) -> SetHeads {
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, Ref::from_digest(Digest::from_bytes(source)))])
    }

    #[rule]
    fn on_failure(&self, Compilation::Failed { source }: Compilation) -> SetHeads {
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, Ref::from_digest(Digest::from_bytes(source)))])
    }
}

fn main() {
    let _ = Publisher::NAMESPACE;
}
