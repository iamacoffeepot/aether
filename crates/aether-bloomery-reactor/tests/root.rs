//! Native `Root` state machine: contiguity, poison, attribution, shared views.

use std::cell::Cell;
use std::error::Error;
use std::fmt;
use std::future::{Future, Ready, poll_fn, ready};
use std::mem::forget;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;

use aether_bloomery_kinds::{
    ClosureArtifact, Digest, EncodedArtifact, Entry, Evaluated, Event, Head, HeadMoved, JournalEntry,
    ReadArtifactResult, Ref, RuleRecord, Seq, SetHeads, Tree, Warm, WarmEntries, Warmed, reactor_record_len,
    write_reactor_record,
};
use aether_bloomery_reactor::{Completion, Nil, Owner, PrepareError, Reactor, Root, RootPoll, reactor};
use aether_bloomery_view::{ArtifactResolver, Publish, PublishError, ResolveError, View, ViewCursor, view};
use aether_data::{Cites, Kind, KindId, Storage, StorageData};

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
    type Advance<'a> = Ready<Result<(), Self::Error>>;

    fn empty() -> Self {
        VIEWS_BUILT.with(|built| built.update(|count| count + 1));
        Self { cursor: Seq(0) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance<'a>(&'a mut self, entries: &'a [Entry], _artifacts: &'a mut ArtifactResolver) -> Self::Advance<'a> {
        ready((|| {
            if entries.iter().any(|entry| entry.kind == KindId(0xdead)) {
                return Err(Boom);
            }
            ENTRIES_FOLDED.with(|folded| folded.update(|count| count + entries.len() as u64));
            if let Some(last) = entries.last() {
                self.cursor = last.seq;
            }
            Ok(())
        })())
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
    fn publish(&self, change: HeadMoved<Tree>, _view: CountView) -> SetHeads {
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, change.to())])
    }
}

struct Witness;

#[reactor]
impl Reactor for Witness {
    const NAMESPACE: &'static str = "test.bloomery.root.witness";

    #[rule]
    fn note(&self, change: HeadMoved<Tree>, _view: CountView) -> SetHeads {
        SetHeads::new(vec![aether_bloomery_kinds::HeadChange::new(&PUBLISHED, None, change.to())])
    }
}

struct BoomReactor;

const BOOM_RULES: &[RuleRecord<'static>] = &[RuleRecord::new("note", HeadMoved::<Tree>::ID, SetHeads::ID)];
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
        visitor.visit::<HeadMoved<Tree>, aether_bloomery_reactor::ViewArg<CountView>, SetHeads>("note");
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

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.root.resolved-note")]
struct ResolvedNote {
    value: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.root.resolved-envelope")]
struct ResolvedEnvelope {
    note: Ref<ResolvedNote>,
}

#[derive(Clone, Debug, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.root.resolve-event")]
struct ResolveEvent {
    envelope: Ref<ResolvedEnvelope>,
    ignore_error: bool,
    retain_read: bool,
}

#[derive(Default)]
struct ResolvedView {
    cursor: ViewCursor,
    value: u64,
    starts: u64,
    firsts: u64,
}

#[view(cursor = cursor)]
impl View for ResolvedView {
    #[fold]
    async fn resolve(&mut self, event: ResolveEvent, artifacts: &mut ArtifactResolver) -> Result<(), ResolveError> {
        self.starts += 1;
        if event.retain_read {
            let mut read = Box::pin(artifacts.read(event.envelope));
            poll_fn(|cx| {
                assert!(Future::poll(read.as_mut(), cx).is_pending());
                Poll::Ready(())
            })
            .await;
            forget(read);
            return Ok(());
        }
        let envelope = match artifacts.read(event.envelope).await {
            Ok(envelope) => envelope,
            Err(_error) if event.ignore_error => return Ok(()),
            Err(error) => return Err(error),
        };
        self.firsts += 1;
        self.value = artifacts.read(envelope.note).await?.value;
        Ok(())
    }
}

struct ResolverReactor;

struct ResolvedValue;

impl aether_bloomery_reactor::Guard<ResolveEvent> for ResolvedValue {
    type Views = ResolvedView;

    fn resolve(_trigger: &ResolveEvent, view: &ResolvedView) -> Option<Self> {
        (view.value == 42 && view.starts == 1 && view.firsts == 1).then_some(Self)
    }
}

#[reactor]
impl Reactor for ResolverReactor {
    const NAMESPACE: &'static str = "test.bloomery.root.resolver";

    #[rule]
    fn observe(&self, _event: ResolveEvent, _resolved: ResolvedValue) -> SetHeads {
        SetHeads::new(Vec::new())
    }
}

type ResolverRoot = Root<(ResolverReactor, Nil)>;

fn closure_of<K: Storage + Clone + Cites>(value: &K) -> (Ref<K>, ClosureArtifact) {
    let encoded = EncodedArtifact::new(value).expect("artifact encode");
    let artifact = ClosureArtifact::new(encoded.kind(), encoded.bytes().to_vec());
    (Ref::from_digest(artifact.claimed().unverified()), artifact)
}

fn resolve_entry(seq: u64, event: ResolveEvent) -> JournalEntry {
    JournalEntry {
        seq,
        kind: ResolveEvent::ID,
        cause: None,
        recorded_at_millis: 0,
        bytes: ResolveEvent::encode_storage(&StorageData::from_value(event)).expect("storage encode"),
    }
}

#[test]
fn live_async_fold_retains_one_future_across_nested_reads() {
    let (note, note_artifact) = closure_of(&ResolvedNote { value: 42 });
    let (envelope, envelope_artifact) = closure_of(&ResolvedEnvelope { note });
    let event = Event::new(resolve_entry(1, ResolveEvent { envelope, ignore_error: false, retain_read: false }));
    let mut root = ResolverRoot::new().expect("names");

    let RootPoll::NeedArtifact(first) = root.start_event(event.clone()) else {
        panic!("first read should suspend");
    };
    assert_eq!(first.digest, envelope.digest());
    assert_eq!(root.status().cursor(), 0);

    let RootPoll::Complete(Completion::Evaluated(rejected)) = root.start_event(event) else {
        panic!("concurrent event should be rejected");
    };
    assert!(matches!(rejected, Evaluated::OutOfSequence { seq: 1, expected: 1 }));
    assert_eq!(root.status().cursor(), 0);

    let Some(RootPoll::NeedArtifact(second)) = root.fulfill(ReadArtifactResult::Found { artifact: envelope_artifact })
    else {
        panic!("second read should suspend");
    };
    assert_eq!(second.digest, note.digest());

    let Some(RootPoll::Complete(Completion::Evaluated(done))) =
        root.fulfill(ReadArtifactResult::Found { artifact: note_artifact })
    else {
        panic!("fold should complete");
    };
    let Evaluated::Completed { seq: 1, intents } = done else {
        panic!("fold should evaluate successfully");
    };
    assert_eq!(intents.len(), 1, "the guard observes each pre-await mutation exactly once");
    assert_eq!(root.status().cursor(), 1);
}

#[test]
fn ignored_resolver_failure_still_poisons_at_frozen_prefix() {
    let (envelope, _) = closure_of(&ResolvedEnvelope { note: digest_ref(8) });
    let event = Event::new(resolve_entry(1, ResolveEvent { envelope, ignore_error: true, retain_read: false }));
    let mut root = ResolverRoot::new().expect("names");

    assert!(matches!(root.start_event(event), RootPoll::NeedArtifact(_)));
    let Some(RootPoll::Complete(Completion::Evaluated(poisoned))) =
        root.fulfill(ReadArtifactResult::Missing { digest: envelope.digest() })
    else {
        panic!("missing result should terminate the fold");
    };
    assert!(matches!(poisoned, Evaluated::Poisoned { seq: 1, last_trusted: 0, .. }));
    assert!(root.status().poisoned());
    assert_eq!(root.status().cursor(), 0);
}

#[test]
fn warm_async_fold_replays_without_evaluating() {
    let (note, note_artifact) = closure_of(&ResolvedNote { value: 42 });
    let (envelope, envelope_artifact) = closure_of(&ResolvedEnvelope { note });
    let warm = warm_of(vec![resolve_entry(1, ResolveEvent { envelope, ignore_error: false, retain_read: false })]);
    let mut root = ResolverRoot::new().expect("names");

    assert!(matches!(root.start_warm(warm), RootPoll::NeedArtifact(_)));
    assert!(matches!(
        root.fulfill(ReadArtifactResult::Found { artifact: envelope_artifact }),
        Some(RootPoll::NeedArtifact(_))
    ));
    let Some(RootPoll::Complete(Completion::Warmed(done))) =
        root.fulfill(ReadArtifactResult::Found { artifact: note_artifact })
    else {
        panic!("warm replay should complete");
    };
    assert!(matches!(done, Warmed::Folded { through: 1 }));
    assert_eq!(root.status().cursor(), 1);
}

#[test]
fn retained_outstanding_read_cannot_publish_a_trusted_cursor() {
    let (envelope, _) = closure_of(&ResolvedEnvelope { note: digest_ref(13) });
    let event = Event::new(resolve_entry(1, ResolveEvent { envelope, ignore_error: false, retain_read: true }));
    let mut root = ResolverRoot::new().expect("names");

    let RootPoll::Complete(Completion::Evaluated(poisoned)) = root.start_event(event) else {
        panic!("retained read must terminate the fold before transport");
    };
    assert!(matches!(poisoned, Evaluated::Poisoned { seq: 1, last_trusted: 0, .. }));
    assert!(root.status().poisoned());
    assert_eq!(root.status().cursor(), 0);
}

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
                let published = SetHeads::decode_from_bytes(intent.bytes()).expect("set-head");
                assert_eq!(published.changes().len(), 1);
                assert_eq!(published.changes()[0].to(), Digest::from_bytes([3; 32]));
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
            assert_eq!(intents[0].kind(), SetHeads::ID);
            assert!(SetHeads::decode_from_bytes(intents[0].bytes()).is_some());
            assert_eq!(intents[1].reactor().as_str(), "test.bloomery.root.witness");
            assert_eq!(intents[1].rule().as_str(), "note");
        }
        other => panic!("{other:?}"),
    }
}
