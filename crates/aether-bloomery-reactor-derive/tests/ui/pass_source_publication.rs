// Named current-head guard plus a direct Heads parameter. The generated
// Arg chain must typecheck with Rust inferring roles.

use aether_bloomery_kinds::{Head, HeadMoved, Program, Ref, SetHead, Tree};
use aether_bloomery_reactor::{ArmVisitor, Guard, Output, Params, Reactor, Trigger, reactor};
use aether_bloomery_view::Heads;

const CURRENT: Head<Program> = Head::new("current");
const PUBLISHED: Head<Tree> = Head::new("published");

struct CurrentCompilation {
    program: Ref<Program>,
}

impl Guard<HeadMoved<Tree>> for CurrentCompilation {
    type Views = Heads;

    fn resolve(_trigger: &HeadMoved<Tree>, heads: &Heads) -> Option<Self> {
        Some(Self { program: heads.get(&CURRENT)? })
    }
}

struct SourcePublisher;

#[reactor]
impl Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "source.publisher";

    #[rule]
    fn publish(
        &self,
        change: HeadMoved<Tree>,
        current: CurrentCompilation,
        heads: Heads,
    ) -> SetHead {
        let _ = (current, heads);
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

struct Nop;

impl ArmVisitor for Nop {
    fn visit<T, L, O>(&mut self, _name: &'static str)
    where
        T: Trigger,
        L: Params<T>,
        O: Output,
    {
    }
}

fn main() {
    SourcePublisher::visit_arms(&mut Nop);
}
