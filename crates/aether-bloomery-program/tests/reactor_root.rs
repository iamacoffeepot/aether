//! Native `Root` state machine: contiguity, poison, attribution, shared views.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use aether_bloomery_kinds::{
    ClosureArtifact, Digest, Entry, Evaluated, Event, Head, HeadChange, HeadMoved, JournalEntry, Mode, ProgramName,
    ProgramRef, ReactorName, Ref, RequestSource, Requested, RuleName, RuleRecord, Seq, SetHeads, Transition, Tree,
    Warm, WarmEntries, Warmed, reactor_record_len, write_reactor_record,
};
use aether_bloomery_program::reactor::Root;
use aether_bloomery_program::{
    At, Cited, CitedError, Guard, Nil, Owner, PrepareError, Program, Publish, PublishError, Ran, Reactor, View,
    ViewCursor, reactor, view,
};
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
    fn publish(&self, change: HeadMoved<Tree>, _view: CountView) -> SetHeads {
        SetHeads::new(vec![HeadChange::new(&PUBLISHED, None, change.to())])
    }
}

struct Witness;

#[reactor]
impl Reactor for Witness {
    const NAMESPACE: &'static str = "test.bloomery.root.witness";

    #[rule]
    fn note(&self, change: HeadMoved<Tree>, _view: CountView) -> SetHeads {
        SetHeads::new(vec![HeadChange::new(&PUBLISHED, None, change.to())])
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

    fn visit_arms(visitor: &mut impl aether_bloomery_program::ArmVisitor) {
        visitor.visit::<HeadMoved<Tree>, aether_bloomery_program::ViewArg<CountView>, SetHeads>("note");
    }

    fn evaluate(&self, owner: &mut Owner) -> Result<Vec<aether_bloomery_program::Intent>, PrepareError> {
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
        cites: Vec::new(),
    }
}

fn fold_fail(seq: u64) -> JournalEntry {
    JournalEntry { seq, kind: KindId(0xdead), cause: None, recorded_at_millis: 0, bytes: Vec::new(), cites: Vec::new() }
}

fn warm_of(entries: Vec<JournalEntry>) -> Warm {
    Warm::new(WarmEntries::new(entries).expect("dense"), Vec::new()).expect("no artifacts")
}

/// A live `Event` of `entry`, which cites nothing.
fn live(entry: JournalEntry) -> Event {
    Event::new(entry, Vec::new())
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

    let live_gap = root.event(live(journal_moved(2, digest_ref(1))));
    assert!(matches!(live_gap, Evaluated::OutOfSequence { seq: 2, expected: 1 }), "{live_gap:?}");
    assert_eq!(root.status().cursor(), 0);

    let folded = root.warm(warm_of(vec![journal_moved(1, digest_ref(1))]));
    assert!(matches!(folded, Warmed::Folded { through: 1 }), "{folded:?}");
    let dup = root.event(live(journal_moved(1, digest_ref(1))));
    assert!(matches!(dup, Evaluated::OutOfSequence { seq: 1, expected: 2 }), "{dup:?}");
    assert_eq!(root.status().cursor(), 1);
}

#[test]
fn failed_fold_poisons_for_good() {
    // Catches recovery after a failed fold.
    let mut root = Pair::new().expect("names");
    let poisoned = root.event(live(fold_fail(1)));
    assert!(matches!(poisoned, Evaluated::Poisoned { seq: 1, last_trusted: 0, .. }), "{poisoned:?}");
    assert!(root.status().poisoned());
    assert_eq!(root.status().cursor(), 0);

    let later_warm = root.warm(warm_of(vec![journal_moved(1, digest_ref(1))]));
    assert!(matches!(later_warm, Warmed::Poisoned { last_trusted: 0, .. }), "{later_warm:?}");
    let later_event = root.event(live(journal_moved(1, digest_ref(1))));
    assert!(matches!(later_event, Evaluated::Poisoned { last_trusted: 0, .. }), "{later_event:?}");
}

#[test]
fn failing_reactor_yields_no_intents_and_views_advance() {
    // Catches partial intents and a stalled cursor.
    FAIL_B.store(true, Ordering::Relaxed);
    let mut root = BoomPair::new().expect("names");
    let failed = root.event(live(journal_moved(1, digest_ref(1))));
    match &failed {
        Evaluated::Failed { seq: 1, reactor, .. } => {
            assert_eq!(reactor.as_str(), "test.bloomery.root.boom");
        }
        other => panic!("{other:?}"),
    }
    assert!(!root.status().poisoned());
    assert_eq!(root.status().cursor(), 1);

    let next = root.event(live(journal_moved(2, digest_ref(2))));
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
    let live = root.event(live(journal_moved(3, digest_ref(3))));
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
    let live = root.event(live(journal_moved(1, digest_ref(1))));
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
    let live = root.event(live(journal_moved(1, digest_ref(1))));
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

#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "test.bloomery.root.summary")]
struct Summary {
    tree: Ref<Tree>,
}

/// A program marker whose runs the summary reactor reads.
struct Summarize;

impl Program for Summarize {
    const NAME: &'static str = "test.root.summarize";
    const MODE: Mode = Mode::Pure;
    const INTENT: &'static str = "Summarize a tree.";
    const DOC: &'static str = "Summarize a tree.";
    type Input = Summary;
    type Result = Summary;
}

const SUMMARY: Head<Tree> = Head::new("summary");

/// Every run's result tree, read from the result each run cites.
#[derive(Default)]
struct Summaries {
    cursor: ViewCursor,
    trees: Vec<Ref<Tree>>,
}

#[view(cursor = cursor)]
impl View for Summaries {
    #[fold]
    fn ran(&mut self, run: Ran<Summarize>, cited: &Cited) -> Result<(), CitedError> {
        self.trees.push(cited.get(run.result())?.tree);
        Ok(())
    }
}

/// The result tree of the run before the trigger, as the fold read it.
struct Previous(Option<Ref<Tree>>);

impl Guard<Ran<Summarize>> for Previous {
    type Views = Summaries;

    fn resolve(_run: &Ran<Summarize>, _at: At, summaries: &Summaries) -> Option<Self> {
        Some(Self(summaries.trees.iter().rev().nth(1).copied()))
    }
}

struct SummaryPublisher;

#[reactor]
impl Reactor for SummaryPublisher {
    const NAMESPACE: &'static str = "test.bloomery.root.summaries";

    #[rule]
    fn publish_summary(&self, run: Ran<Summarize>, cited: Cited, previous: Previous) -> SetHeads {
        let changes = cited.get(run.result()).map(|summary| vec![HeadChange::new(&SUMMARY, previous.0, summary.tree)]);
        SetHeads::new(changes.unwrap_or_default())
    }
}

/// One stored `Summary` naming `tree`: its digest and its artifact.
fn summary_artifact(tree: u8) -> (Digest, ClosureArtifact) {
    let summary = Summary { tree: digest_ref(tree) };
    let payload = Summary::encode_storage(&StorageData::from_value(summary.clone())).expect("storage encode");
    (Ref::of_encoded(&summary).expect("digest").digest(), ClosureArtifact::new(Summary::ID, payload))
}

fn summarize_ref() -> ProgramRef {
    ProgramRef::new(Digest::from_bytes([9; 32]), ProgramName::new(Summarize::NAME).expect("name"))
}

/// A recorded run of `Summarize` at `seq`, reacting to `cause`, whose result
/// names `tree`, and the two artifacts it cites.
fn ran(seq: u64, cause: Option<u64>, tree: u8) -> (JournalEntry, Vec<ClosureArtifact>) {
    let (input, input_artifact) = summary_artifact(tree + 100);
    let (result, result_artifact) = summary_artifact(tree);
    let transition = Transition { program: summarize_ref(), input, result };
    let entry = JournalEntry {
        seq,
        kind: Transition::ID,
        cause,
        recorded_at_millis: 0,
        bytes: Transition::encode_storage(&StorageData::from_value(transition)).expect("storage encode"),
        cites: vec![input, result],
    };
    (entry, vec![input_artifact, result_artifact])
}

type Summarized = Root<(SummaryPublisher, Nil)>;

#[test]
fn warm_and_live_delivery_hand_folds_and_rules_the_same_citations() {
    // Catches a warm that drops or mis-scopes an entry's citations, so a
    // fold that reads them diverges from live delivery, and a rule that
    // cannot read the result its triggering run cites.
    let (first, first_artifacts) = ran(1, None, 1);
    let (second, second_artifacts) = ran(2, None, 2);

    let mut all_live = Summarized::new().expect("names");
    let opening = all_live.event(Event::new(first.clone(), first_artifacts.clone()));
    assert!(matches!(opening, Evaluated::Completed { seq: 1, .. }), "{opening:?}");
    let live_second = all_live.event(Event::new(second.clone(), second_artifacts.clone()));

    let mut warmed = Summarized::new().expect("names");
    let warm = Warm::new(WarmEntries::new(vec![first]).expect("dense"), first_artifacts).expect("scoped");
    let folded = warmed.warm(warm);
    assert!(matches!(folded, Warmed::Folded { through: 1 }), "{folded:?}");
    let warmed_second = warmed.event(Event::new(second, second_artifacts));

    assert_eq!(live_second, warmed_second);
    let Evaluated::Completed { seq: 2, intents } = &live_second else {
        panic!("{live_second:?}");
    };
    let [intent] = intents.as_slice() else {
        panic!("one intent: {intents:?}");
    };
    let published = SetHeads::decode_from_bytes(intent.bytes()).expect("set heads");
    assert_eq!(published.changes(), [HeadChange::new(&SUMMARY, Some(digest_ref(1)), digest_ref(2))]);
}

#[test]
fn a_run_delivered_without_its_citations_poisons_the_fold() {
    // Catches a fold that folds past an entry whose cited result it could
    // not read, leaving its state silently short.
    let (first, _) = ran(1, None, 1);
    let mut root = Summarized::new().expect("names");
    let poisoned = root.event(live(first));
    assert!(matches!(poisoned, Evaluated::Poisoned { seq: 1, last_trusted: 0, .. }), "{poisoned:?}");
}

/// Each `Requested` entry's seq and the seq it reacts to, read from the
/// entry's own position.
#[derive(Default)]
struct RequestCauses {
    cursor: ViewCursor,
    causes: BTreeMap<Seq, Option<Seq>>,
}

#[view(cursor = cursor)]
impl View for RequestCauses {
    #[fold]
    fn requested(&mut self, _request: Requested, at: At) {
        self.causes.insert(at.seq, at.cause);
    }
}

/// The entry that asked for the triggering run: the run's cause is its
/// `Requested` entry, and that request's cause is the asker.
struct AskedBy(Seq);

impl Guard<Ran<Summarize>> for AskedBy {
    type Views = RequestCauses;

    fn resolve(_run: &Ran<Summarize>, at: At, causes: &RequestCauses) -> Option<Self> {
        causes.causes.get(&at.cause?).copied().flatten().map(Self)
    }
}

const ASKER: Head<Tree> = Head::new("asker");
const ANSWER: Head<Tree> = Head::new("answer");

struct AskerPublisher;

#[reactor]
impl Reactor for AskerPublisher {
    const NAMESPACE: &'static str = "test.bloomery.root.askers";

    #[rule]
    fn publish_asker(&self, _run: Ran<Summarize>, at: At, asked: AskedBy) -> SetHeads {
        SetHeads::new(vec![
            HeadChange::new(&ASKER, None, seq_ref(asked.0)),
            HeadChange::new(&ANSWER, None, seq_ref(at.seq)),
        ])
    }
}

/// A tree ref standing for `seq`, so an intent can name a journal position.
fn seq_ref(seq: Seq) -> Ref<Tree> {
    digest_ref(u8::try_from(seq.0).expect("small seq"))
}

/// A `Requested` run of `Summarize` at `seq`, reacting to `cause`.
fn requested(seq: u64, cause: u64) -> JournalEntry {
    let request = Requested {
        program: summarize_ref(),
        input: Digest::from_bytes([7; 32]),
        source: RequestSource::Reaction {
            bundle: Digest::from_bytes([8; 32]),
            reactor: ReactorName::new("test.bloomery.root.asker").expect("reactor name"),
            rule: RuleName::new("ask").expect("rule name"),
            ordinal: 0,
        },
    };
    JournalEntry {
        seq,
        kind: Requested::ID,
        cause: Some(cause),
        recorded_at_millis: 0,
        bytes: Requested::encode_storage(&StorageData::from_value(request)).expect("storage encode"),
        cites: Vec::new(),
    }
}

type Askers = Root<(AskerPublisher, Nil)>;

#[test]
fn warm_and_live_delivery_hand_folds_and_guards_the_same_position() {
    // Catches a warm path or derive walk that hands a fold or guard a wrong
    // or missing position (the trigger's instead of the folded entry's, or
    // an off-by-one in the retained prefix), so following a run's cause back
    // to the entry that asked for it diverges between warmup and live.
    let asker = journal_moved(1, digest_ref(40));
    let request = requested(2, 1);
    let (run, run_artifacts) = ran(3, Some(2), 3);

    let mut all_live = Askers::new().expect("names");
    for entry in [asker.clone(), request.clone()] {
        let quiet = all_live.event(live(entry));
        assert!(matches!(&quiet, Evaluated::Completed { intents, .. } if intents.is_empty()), "{quiet:?}");
    }
    let live_run = all_live.event(Event::new(run.clone(), run_artifacts.clone()));

    let mut warmed = Askers::new().expect("names");
    let folded = warmed.warm(warm_of(vec![asker, request]));
    assert!(matches!(folded, Warmed::Folded { through: 2 }), "{folded:?}");
    let warmed_run = warmed.event(Event::new(run, run_artifacts));

    assert_eq!(live_run, warmed_run);
    let Evaluated::Completed { seq: 3, intents } = &live_run else {
        panic!("{live_run:?}");
    };
    let [intent] = intents.as_slice() else {
        panic!("one intent: {intents:?}");
    };
    let published = SetHeads::decode_from_bytes(intent.bytes()).expect("set heads");
    assert_eq!(
        published.changes(),
        [HeadChange::new(&ASKER, None, seq_ref(Seq(1))), HeadChange::new(&ANSWER, None, seq_ref(Seq(3)))]
    );
}
