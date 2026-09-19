//! Native `Root` state machine: contiguity, poison, attribution, shared views.

use std::cell::Cell;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use aether_bloomery_kinds::{
    Digest, Entry, Evaluated, Event, Head, HeadMoved, JournalEntry, Ref, RuleRecord, Seq, SetHead, Tree, Warm,
    WarmEntries, Warmed, reactor_record_len, write_reactor_record,
};
use aether_bloomery_reactor::{Nil, Owner, PrepareError, Reactor, Root, reactor};
use aether_bloomery_view::{Publish, PublishError, View};
use aether_data::{Kind, KindId, Storage, StorageData};

const PUBLISHED: Head<Tree> = Head::new("published");

static FAIL_B: AtomicBool = AtomicBool::new(false);

thread_local! {
    static VIEWS_BUILT: Cell<u32> = const { Cell::new(0) };
    static ENTRIES_FOLDED: Cell<u64> = const { Cell::new(0) };
}

fn reset_counts() {
    VIEWS_BUILT.with(|built| built.set(0));
    ENTRIES_FOLDED.with(|folded| folded.set(0));
}

fn counts() -> (u32, u64) {
    (VIEWS_BUILT.with(Cell::get), ENTRIES_FOLDED.with(Cell::get))
}

#[derive(Clone, Debug)]
struct CountView {
    cursor: Seq,
}

#[derive(Debug)]
struct Boom;

impl fmt::Display for Boom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("fold boom")
    }
}

impl Error for Boom {}

impl View for CountView {
    type Error = Boom;

    fn empty() -> Self {
        VIEWS_BUILT.with(|built| built.update(|count| count + 1));
        Self { cursor: Seq(0) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance(&mut self, entries: &[Entry]) -> Result<(), Self::Error> {
        if entries.iter().any(|entry| entry.kind == KindId(0xdead)) {
            return Err(Boom);
        }
        ENTRIES_FOLDED.with(|folded| folded.update(|count| count + entries.len() as u64));
        if let Some(last) = entries.last() {
            self.cursor = last.seq;
        }
        Ok(())
    }
}

impl Publish for CountView {
    fn snapshot(&self) -> Self {
        self.clone()
    }

    fn encode(&self) -> Result<Vec<u8>, PublishError> {
        Ok(Vec::new())
    }

    fn decode(_bytes: &[u8]) -> Result<Self, PublishError> {
        Ok(Self::empty())
    }
}

struct Publisher;

#[reactor]
impl Reactor for Publisher {
    const NAMESPACE: &'static str = "test.bloomery.root.publisher";

    #[rule]
    fn publish(&self, change: HeadMoved<Tree>, _view: CountView) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

struct Witness;

#[reactor]
impl Reactor for Witness {
    const NAMESPACE: &'static str = "test.bloomery.root.witness";

    #[rule]
    fn note(&self, change: HeadMoved<Tree>, _view: CountView) -> SetHead {
        SetHead::new(&PUBLISHED, None, change.to())
    }
}

struct BoomReactor;

const BOOM_RULES: &[RuleRecord<'static>] = &[RuleRecord::new("note", HeadMoved::<Tree>::ID, SetHead::ID)];
const BOOM_LEN: usize = reactor_record_len("test.bloomery.root.boom", BOOM_RULES);

impl Default for BoomReactor {
    fn default() -> Self {
        Self
    }
}

impl Reactor for BoomReactor {
    const NAMESPACE: &'static str = "test.bloomery.root.boom";
    const DECLARATION: &'static [u8] = &write_reactor_record::<BOOM_LEN>("test.bloomery.root.boom", BOOM_RULES);

    fn visit_arms(visitor: &mut impl aether_bloomery_reactor::ArmVisitor) {
        visitor.visit::<HeadMoved<Tree>, aether_bloomery_reactor::ViewArg<CountView>, SetHead>("note");
    }

    fn evaluate(&self, owner: &mut Owner) -> Result<Vec<aether_bloomery_reactor::Intent>, PrepareError> {
        if FAIL_B.swap(false, Ordering::Relaxed) {
            return Err(PrepareError::Empty);
        }
        Witness.evaluate(owner)
    }
}

fn digest_ref<K>(byte: u8) -> Ref<K> {
    Ref::from_digest(Digest::from_bytes([byte; 32]))
}

fn journal_moved(seq: u64, to: Ref<Tree>) -> JournalEntry {
    let event = Head::<Tree>::new("source").move_to(to);
    JournalEntry {
        seq,
        kind: HeadMoved::<Tree>::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: HeadMoved::<Tree>::encode_storage(&StorageData::from_value(event)).expect("storage encode"),
    }
}

fn fold_fail(seq: u64) -> JournalEntry {
    JournalEntry { seq, kind: KindId(0xdead), cause: None, recorded_at_millis: 0, bytes: Vec::new() }
}

fn warm_of(entries: Vec<JournalEntry>) -> Warm {
    Warm::new(WarmEntries::new(entries).expect("dense"))
}

type Pair = Root<(Publisher, (Witness, Nil))>;
type BoomPair = Root<(Publisher, (BoomReactor, Nil))>;

#[test]
fn out_of_sequence_changes_nothing() {
    // Catches pushing before checking.
    let mut root = Pair::new().expect("names");
    let gap = root.warm(warm_of(vec![journal_moved(2, digest_ref(1))]));
    assert!(matches!(gap, Warmed::OutOfSequence { first: 2, expected: 1 }), "{gap:?}");
    assert_eq!(root.status().cursor(), 0);

    let live_gap = root.event(Event::new(journal_moved(2, digest_ref(1))));
    assert!(matches!(live_gap, Evaluated::OutOfSequence { seq: 2, expected: 1 }), "{live_gap:?}");
    assert_eq!(root.status().cursor(), 0);

    let folded = root.warm(warm_of(vec![journal_moved(1, digest_ref(1))]));
    assert!(matches!(folded, Warmed::Folded { through: 1 }), "{folded:?}");
    let dup = root.event(Event::new(journal_moved(1, digest_ref(1))));
    assert!(matches!(dup, Evaluated::OutOfSequence { seq: 1, expected: 2 }), "{dup:?}");
    assert_eq!(root.status().cursor(), 1);
}

#[test]
fn failed_fold_poisons_for_good() {
    // Catches recovery after a failed fold.
    let mut root = Pair::new().expect("names");
    let poisoned = root.event(Event::new(fold_fail(1)));
    assert!(matches!(poisoned, Evaluated::Poisoned { seq: 1, last_trusted: 0, .. }), "{poisoned:?}");
    assert!(root.status().poisoned());
    assert_eq!(root.status().cursor(), 0);

    let later_warm = root.warm(warm_of(vec![journal_moved(1, digest_ref(1))]));
    assert!(matches!(later_warm, Warmed::Poisoned { last_trusted: 0, .. }), "{later_warm:?}");
    let later_event = root.event(Event::new(journal_moved(1, digest_ref(1))));
    assert!(matches!(later_event, Evaluated::Poisoned { last_trusted: 0, .. }), "{later_event:?}");
}

#[test]
fn failing_reactor_yields_no_intents_and_views_advance() {
    // Catches partial intents and a stalled cursor.
    FAIL_B.store(true, Ordering::Relaxed);
    let mut root = BoomPair::new().expect("names");
    let failed = root.event(Event::new(journal_moved(1, digest_ref(1))));
    match &failed {
        Evaluated::Failed { seq: 1, reactor, .. } => {
            assert_eq!(reactor.as_str(), "test.bloomery.root.boom");
        }
        other => panic!("{other:?}"),
    }
    assert!(!root.status().poisoned());
    assert_eq!(root.status().cursor(), 1);

    let next = root.event(Event::new(journal_moved(2, digest_ref(2))));
    match next {
        Evaluated::Completed { seq: 2, intents } => assert!(!intents.is_empty()),
        other => panic!("{other:?}"),
    }
    assert_eq!(root.status().cursor(), 2);
}

#[test]
fn warm_folds_without_evaluating() {
    // Catches evaluation during warmup and folding that skips entries.
    reset_counts();
    let mut root = Pair::new().expect("names");
    let folded = root.warm(warm_of(vec![journal_moved(1, digest_ref(1)), journal_moved(2, digest_ref(2))]));
    assert!(matches!(folded, Warmed::Folded { through: 2 }), "{folded:?}");
    assert_eq!(counts(), (1, 2));
    let live = root.event(Event::new(journal_moved(3, digest_ref(3))));
    match live {
        Evaluated::Completed { seq: 3, intents } => {
            for intent in &intents {
                let published = SetHead::decode_from_bytes(intent.bytes()).expect("set-head");
                assert_eq!(published.to(), Digest::from_bytes([3; 32]));
            }
            assert_eq!(counts(), (1, 3));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn reactors_share_one_view_instance() {
    // Catches a per-reactor owner.
    reset_counts();
    let mut root = Pair::new().expect("names");
    let live = root.event(Event::new(journal_moved(1, digest_ref(1))));
    match live {
        Evaluated::Completed { intents, .. } => {
            assert_eq!(intents.len(), 2);
            assert_eq!(counts().0, 1);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn intents_carry_reactor_rule_and_mail_kind() {
    // Catches wrong attribution, or a kind name standing in for an id.
    let mut root = Pair::new().expect("names");
    let live = root.event(Event::new(journal_moved(1, digest_ref(1))));
    match live {
        Evaluated::Completed { intents, .. } => {
            assert_eq!(intents[0].reactor().as_str(), "test.bloomery.root.publisher");
            assert_eq!(intents[0].rule().as_str(), "publish");
            assert_eq!(intents[0].kind(), SetHead::ID);
            assert!(SetHead::decode_from_bytes(intents[0].bytes()).is_some());
            assert_eq!(intents[1].reactor().as_str(), "test.bloomery.root.witness");
            assert_eq!(intents[1].rule().as_str(), "note");
        }
        other => panic!("{other:?}"),
    }
}
