// Two arms share Heads, and refutable enum trigger cases compile as declines.

use aether_bloomery_kinds::{Head, HeadMoved, Tree};
use aether_bloomery_reactor::{Guard, Output, Reactor, reactor};
use aether_bloomery_view::Heads;

const SOURCE: Head<Tree> = Head::new("source");

#[derive(Debug, Clone, PartialEq, aether_data::Storage)]
#[kind(name = "test.bloomery.reactor.ui.compilation")]
enum Compilation {
    Succeeded { output: u32 },
    Failed { code: u32 },
}

struct BoundSource;

impl Guard<HeadMoved<Tree>> for BoundSource {
    type Views = Heads;

    fn resolve(_trigger: &HeadMoved<Tree>, heads: &Heads) -> Option<Self> {
        heads.get(&SOURCE).map(|_| Self)
    }
}

impl Guard<Compilation> for BoundSource {
    type Views = Heads;

    fn resolve(_trigger: &Compilation, heads: &Heads) -> Option<Self> {
        heads.get(&SOURCE).map(|_| Self)
    }
}

#[aether_data::kind(name = "test.bloomery.reactor.ui.shared_out", eq)]
struct PublicationProposal {
    marker: u32,
}

impl Output for PublicationProposal {}

struct Publisher;

#[reactor]
impl Reactor for Publisher {
    const NAME: &'static str = "shared.publisher";

    #[rule]
    fn from_heads(&self, _change: HeadMoved<Tree>, _heads: Heads) -> PublicationProposal {
        PublicationProposal { marker: 1 }
    }

    #[rule]
    fn from_guard(&self, _change: HeadMoved<Tree>, _bound: BoundSource) -> PublicationProposal {
        PublicationProposal { marker: 2 }
    }

    #[rule]
    fn on_success(
        &self,
        Compilation::Succeeded { output }: Compilation,
        _bound: BoundSource,
    ) -> PublicationProposal {
        PublicationProposal { marker: output }
    }

    #[rule]
    fn on_failure(&self, Compilation::Failed { code }: Compilation) -> PublicationProposal {
        PublicationProposal { marker: code }
    }
}

fn main() {
    let _ = Publisher::NAME;
}
