//! Generated reactor-bundle fixture: two source-publication reactors share one
//! views owner inside a WASM cluster.
//!
//! Authors declare reactors and guards. `reactor_bundle!` generates the views
//! owner and inline peers. Load `SourcePublicationViews` by NAMESPACE; packing
//! the peer types in `export!` does not instantiate them.

#![allow(clippy::unused_self)]

use aether_bloomery_kinds::{Head, HeadMoved, Program, Ref, Tree};
use aether_bloomery_reactor::{Guard, Output, Reactor, reactor, reactor_bundle};
use aether_bloomery_view::Heads;
use aether_test_fixtures_kinds::{ReactorGuardedPublication, ReactorOpenPublication};

const CURRENT: Head<Program> = Head::new("current");

struct CurrentCompilation {
    program: Ref<Program>,
}

impl Guard<HeadMoved<Tree>> for CurrentCompilation {
    type Views = Heads;

    fn resolve(_trigger: &HeadMoved<Tree>, heads: &Heads) -> Option<Self> {
        Some(Self { program: heads.get(&CURRENT)? })
    }
}

impl Output for ReactorGuardedPublication {}
impl Output for ReactorOpenPublication {}

pub struct SourcePublisher;

#[reactor]
impl Reactor for SourcePublisher {
    const NAME: &'static str = "source.publisher";

    #[rule]
    fn publish_source(
        &self,
        change: HeadMoved<Tree>,
        current: CurrentCompilation,
        heads: Heads,
    ) -> ReactorGuardedPublication {
        assert_eq!(heads.get(&CURRENT), Some(current.program));
        ReactorGuardedPublication { digest: *change.to().digest().as_bytes() }
    }
}

pub struct SourceWitness;

#[reactor]
impl Reactor for SourceWitness {
    const NAME: &'static str = "source.witness";

    #[rule]
    fn note_heads(&self, change: HeadMoved<Tree>, heads: Heads) -> ReactorOpenPublication {
        assert!(heads.cursor().0 > 0);
        ReactorOpenPublication { digest: *change.to().digest().as_bytes() }
    }
}

reactor_bundle! {
    default = SourcePublicationViews,
    namespace = "test.bloomery.reactor",
    SourcePublisher,
    SourceWitness,
}
