//! Reactor-bundle fixture: two source-publication reactors share one views
//! owner inside a digest-loaded root.
//!
//! Authors declare reactors and guards. `export!(…, generators = [bundle_reactors])`
//! generates one root at [`aether_bloomery_reactor::REACTOR_NAMESPACE`]. Load it
//! under the journal artifact digest with empty config.

use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicU32, Ordering};

use aether_bloomery_kinds::{Entry, Head, HeadMoved, Program, Ref, Seq, Tree};
use aether_bloomery_reactor::{And, Guard, reactor};
use aether_bloomery_view::{Heads, Publish, PublishError, View};
use aether_data::wire::{decode_from_slice, encode_to_vec};
use aether_test_fixtures_kinds::{REACTOR_FOLD_FAIL_KIND, ReactorGuardedPublication, ReactorOpenPublication};

const CURRENT: Head<Program> = Head::new("current");
static FOLD_IDS: AtomicU32 = AtomicU32::new(1);

/// Shared published fold used by both reactors. `empty` mints a cluster-local
/// id so two reactors sharing one owner emit the same id, and two root
/// instances do not.
#[derive(Clone, Debug)]
pub struct FoldTally {
    cursor: Seq,
    folds: u32,
    id: u32,
}

#[derive(Clone, Debug, aether_data::Schema, serde::Serialize, serde::Deserialize)]
struct FoldTallyWire {
    cursor: u64,
    folds: u32,
    id: u32,
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
        Self { cursor: Seq(0), folds: 0, id: FOLD_IDS.fetch_add(1, Ordering::Relaxed) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
        if entries.iter().any(|entry| entry.kind == REACTOR_FOLD_FAIL_KIND) {
            return Err(FoldBoom);
        }
        self.folds = self.folds.saturating_add(1);
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
        Ok(encode_to_vec(&FoldTallyWire { cursor: self.cursor.0, folds: self.folds, id: self.id })?)
    }

    fn decode(bytes: &[u8]) -> Result<Self, PublishError> {
        let wire = decode_from_slice::<FoldTallyWire>(bytes)?;
        Ok(Self { cursor: Seq(wire.cursor), folds: wire.folds, id: wire.id })
    }
}

struct CurrentCompilation {
    program: Ref<Program>,
    fold_id: u32,
    folds: u32,
}

impl Guard<HeadMoved<Tree>> for CurrentCompilation {
    type Views = And<Heads, FoldTally>;

    fn resolve(_trigger: &HeadMoved<Tree>, (heads, tally): (&Heads, &FoldTally)) -> Option<Self> {
        Some(Self { program: heads.get(&CURRENT)?, fold_id: tally.id, folds: tally.folds })
    }
}

pub struct SourcePublisher;

#[reactor]
impl Reactor for SourcePublisher {
    const NAMESPACE: &'static str = "test.bloomery.source.publisher";

    #[rule]
    fn publish_source(
        &self,
        change: HeadMoved<Tree>,
        current: CurrentCompilation,
        heads: Heads,
    ) -> ReactorGuardedPublication {
        assert_eq!(heads.get(&CURRENT), Some(current.program));
        ReactorGuardedPublication {
            digest: *change.to().digest().as_bytes(),
            fold_id: current.fold_id,
            folds: current.folds,
        }
    }
}

pub struct SourceWitness;

#[reactor]
impl Reactor for SourceWitness {
    const NAMESPACE: &'static str = "test.bloomery.source.witness";

    #[rule]
    fn note_heads(&self, change: HeadMoved<Tree>, tally: FoldTally) -> ReactorOpenPublication {
        assert!(tally.cursor.0 > 0);
        ReactorOpenPublication { digest: *change.to().digest().as_bytes(), fold_id: tally.id, folds: tally.folds }
    }
}

aether_actor::export!(SourcePublisher, SourceWitness, generators = [aether_bloomery_reactor::bundle_reactors],);
