//! Generated reactor-bundle fixture: two source-publication reactors share one
//! views owner inside a WASM cluster.
//!
//! Authors declare reactors and guards. `reactor_bundle!` generates the views
//! owner and inline peers. Load `SourcePublicationViews` by NAMESPACE; packing
//! the peer types in `export!` does not instantiate them. `ReactorOutputSink`
//! is the external mailbox that records typed outputs for the host test.

use core::convert::Infallible;
use core::sync::atomic::{AtomicU32, Ordering};

use aether_actor::{ActorInitError, Manual, OutboundReply, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_bloomery_kinds::{Entry, Head, HeadMoved, Program, Ref, Seq, Tree};
use aether_bloomery_reactor::{And, BundledView, Guard, Reactor, reactor, reactor_bundle};
use aether_bloomery_view::{Heads, Publish, PublishError, View};
use aether_data::wire::{decode_from_slice, encode_to_vec};
use aether_test_fixtures_kinds::{
    CollectReactorOutputs, CollectReactorOutputsResult, ReactorGuardedPublication, ReactorOpenPublication,
};

const CURRENT: Head<Program> = Head::new("current");
static FOLD_IDS: AtomicU32 = AtomicU32::new(1);

/// Shared published fold used by both reactors. `empty` mints a cluster-local
/// id so two reactors sharing one owner emit the same id, and two cluster
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

impl View for FoldTally {
    type Error = Infallible;

    fn empty() -> Self {
        Self { cursor: Seq(0), folds: 0, id: FOLD_IDS.fetch_add(1, Ordering::Relaxed) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
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

impl BundledView for FoldTally {
    const NAME: &'static str = "test.bloomery.reactor.fold_tally";
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
    const NAME: &'static str = "source.publisher";

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
    const NAME: &'static str = "source.witness";

    #[rule]
    fn note_heads(&self, change: HeadMoved<Tree>, tally: FoldTally) -> ReactorOpenPublication {
        assert!(tally.cursor.0 > 0);
        ReactorOpenPublication { digest: *change.to().digest().as_bytes(), fold_id: tally.id, folds: tally.folds }
    }
}

reactor_bundle! {
    default = SourcePublicationViews,
    namespace = "test.bloomery.reactor",
    SourcePublisher,
    SourceWitness,
}

/// External output mailbox for one loaded cluster.
pub struct ReactorOutputSink {
    guarded: Vec<ReactorGuardedPublication>,
    open: Vec<ReactorOpenPublication>,
}

#[actor]
impl WasmActor for ReactorOutputSink {
    const NAMESPACE: &'static str = "test.bloomery.reactor.sink";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self { guarded: Vec::new(), open: Vec::new() })
    }

    #[handler::single]
    fn on_guarded(&mut self, _ctx: &mut WasmCtx<'_>, publication: ReactorGuardedPublication) {
        self.guarded.push(publication);
    }

    #[handler::single]
    fn on_open(&mut self, _ctx: &mut WasmCtx<'_>, publication: ReactorOpenPublication) {
        self.open.push(publication);
    }

    #[handler::manual]
    fn on_collect(&mut self, ctx: &mut WasmCtx<'_, Manual>, _query: CollectReactorOutputs) {
        if ctx.reply_target().is_some() {
            ctx.reply(&CollectReactorOutputsResult { guarded: self.guarded.clone(), open: self.open.clone() });
        }
    }
}
