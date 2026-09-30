//! Reactor-bundle fixture: two reactors share one views owner inside a
//! digest-loaded root, and both publish the triggering tree as a `SetHeads`.
//!
//! Authors declare reactors and guards. `export!(public = […], generators = [aether_bloomery_bundle::bundle])`
//! generates one root at [`aether_bloomery_kinds::BUNDLE_NAMESPACE`]. Load it
//! under the journal artifact digest with empty config.

use std::collections::BTreeMap;

use core::error::Error;
use core::fmt;

use aether_bloomery_kinds::{Entry, Head, HeadMoved, Program, Seq, SetHeads, Tree};
use aether_bloomery_reactor::{And, Guard, reactor};
use aether_bloomery_view::{At, Heads, Publish, PublishError, View, ViewCursor, view};
use aether_data::wire::{decode_from_slice, encode_to_vec};
use aether_test_fixtures_kinds::REACTOR_FOLD_FAIL_KIND;

const CURRENT: Head<Program> = Head::new("current");
const PUBLISHED: Head<Tree> = Head::new("published");
const MIRRORED: Head<Tree> = Head::new("mirrored");

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

/// Generated aggregate shared by both reactor guards.
#[derive(Default)]
struct AuthoredMoves {
    cursor: ViewCursor,
    by_head: BTreeMap<String, u64>,
}

#[view(cursor = cursor)]
impl View for AuthoredMoves {
    #[fold]
    fn moved(&mut self, event: HeadMoved<Tree>) {
        *self.by_head.entry(event.head().as_str().to_owned()).or_default() += 1;
    }
}

impl AuthoredMoves {
    fn count(&self, head: &Head<Tree>) -> u64 {
        self.by_head.get(head.as_str()).copied().unwrap_or_default()
    }
}

/// Declines until the `current` program head is bound.
struct CurrentCompilation;

impl Guard<HeadMoved<Tree>> for CurrentCompilation {
    type Views = And<Heads, And<FoldTally, AuthoredMoves>>;

    fn resolve(
        trigger: &HeadMoved<Tree>,
        _at: At,
        (heads, (_tally, moves)): (&Heads, (&FoldTally, &AuthoredMoves)),
    ) -> Option<Self> {
        heads.get(&CURRENT)?;
        (moves.count(trigger.head()) == 1).then_some(Self)
    }
}

/// Declines until the shared fold has folded past the trigger.
struct FoldAdvanced;

impl Guard<HeadMoved<Tree>> for FoldAdvanced {
    type Views = And<FoldTally, AuthoredMoves>;

    fn resolve(trigger: &HeadMoved<Tree>, _at: At, (tally, moves): (&FoldTally, &AuthoredMoves)) -> Option<Self> {
        (tally.cursor.0 > 0 && moves.count(trigger.head()) > 0).then_some(Self)
    }
}

pub struct SourcePublisher;

#[reactor]
impl Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "test.bloomery.source.publisher";

    #[rule]
    fn publish_source(&self, change: HeadMoved<Tree>, _current: CurrentCompilation, heads: Heads) -> SetHeads {
        SetHeads::new(vec![
            aether_bloomery_kinds::HeadChange::new(&PUBLISHED, heads.get(&PUBLISHED), change.to()),
            aether_bloomery_kinds::HeadChange::new(&MIRRORED, heads.get(&MIRRORED), change.to()),
        ])
    }
}

pub struct SourceWitness;

#[reactor]
impl Reactor for SourceWitness {
    const NAMESPACE: &'static str = "test.bloomery.source.witness";

    #[rule]
    fn note_heads(&self, change: HeadMoved<Tree>, _advanced: FoldAdvanced) -> SetHeads {
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, change.to())])
    }
}

aether_actor::export!(public = [SourcePublisher, SourceWitness], generators = [aether_bloomery_bundle::bundle]);
