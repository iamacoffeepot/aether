//! Reactor-bundle fixture: two reactors share one views owner inside a
//! digest-loaded root, and both publish the triggering tree as a `SetHead`.
//!
//! Authors declare reactors and guards. `export!(…, generators = [aether_bloomery_bundle::bundle])`
//! generates one root at [`aether_bloomery_kinds::BUNDLE_NAMESPACE`]. Load it
//! under the journal artifact digest with empty config.

use core::error::Error;
use core::fmt;

use aether_bloomery_kinds::{Entry, Head, HeadMoved, Program, Seq, SetHead, Tree};
use aether_bloomery_reactor::{And, Guard, reactor};
use aether_bloomery_view::{Heads, Publish, PublishError, View};
use aether_data::wire::{decode_from_slice, encode_to_vec};
use aether_test_fixtures_kinds::REACTOR_FOLD_FAIL_KIND;

const CURRENT: Head<Program> = Head::new("current");
const PUBLISHED: Head<Tree> = Head::new("published");

/// Shared published fold used by both reactors.
#[derive(Clone, Debug)]
pub struct FoldTally {
    cursor: Seq,
}

#[derive(Clone, Debug, aether_data::Schema, serde::Serialize, serde::Deserialize)]
struct FoldTallyWire {
    cursor: u64,
}

#[derive(Debug)]
pub struct FoldBoom;

impl fmt::Display for FoldBoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("fold boom")
    }
}

impl Error for FoldBoom {}

impl View for FoldTally {
    type Error = FoldBoom;

    fn empty() -> Self {
        Self { cursor: Seq(0) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
        if entries.iter().any(|entry| entry.kind == REACTOR_FOLD_FAIL_KIND) {
            return Err(FoldBoom);
        }
        if let Some(last) = entries.last() {
            self.cursor = last.seq;
        }
        Ok(())
    }
}

impl Publish for FoldTally {
    fn snapshot(&self) -> Self {
        self.clone()
    }

    fn encode(&self) -> Result<Vec<u8>, PublishError> {
        Ok(encode_to_vec(&FoldTallyWire { cursor: self.cursor.0 })?)
    }

    fn decode(bytes: &[u8]) -> Result<Self, PublishError> {
        let wire = decode_from_slice::<FoldTallyWire>(bytes)?;
        Ok(Self { cursor: Seq(wire.cursor) })
    }
}

/// Declines until the `current` program head is bound.
struct CurrentCompilation;

impl Guard<HeadMoved<Tree>> for CurrentCompilation {
    type Views = And<Heads, FoldTally>;

    fn resolve(_trigger: &HeadMoved<Tree>, (heads, _tally): (&Heads, &FoldTally)) -> Option<Self> {
        heads.get(&CURRENT).map(|_| Self)
    }
}

/// Declines until the shared fold has folded past the trigger.
struct FoldAdvanced;

impl Guard<HeadMoved<Tree>> for FoldAdvanced {
    type Views = FoldTally;

    fn resolve(_trigger: &HeadMoved<Tree>, tally: &FoldTally) -> Option<Self> {
        (tally.cursor.0 > 0).then_some(Self)
    }
}

pub struct SourcePublisher;

#[reactor]
impl Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "test.bloomery.source.publisher";

    #[rule]
    fn publish_source(&self, change: HeadMoved<Tree>, _current: CurrentCompilation, heads: Heads) -> SetHead {
        SetHead::new(&PUBLISHED, heads.get(&PUBLISHED), change.to())
    }
}

pub struct SourceWitness;

#[reactor]
impl Reactor for SourceWitness {
    const NAMESPACE: &'static str = "test.bloomery.source.witness";

    #[rule]
    fn note_heads(&self, change: HeadMoved<Tree>, _advanced: FoldAdvanced) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

aether_actor::export!(SourcePublisher, SourceWitness, generators = [aether_bloomery_bundle::bundle],);
