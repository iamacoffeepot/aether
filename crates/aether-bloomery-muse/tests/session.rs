//! A Muse session driven through its reactor the way the bloomery driver drives it: each entry is routed live to
//! the reactor root, each intent the loop returns is recorded as a request and run for real, and each turn is
//! answered with a recorded reply.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;

use aether_bloomery_kinds::{
    CallInput, CallProgram, ClosureArtifact, Detail, Digest, EncodedArtifact, ErasedRef, Evaluated, Event, Fault,
    FaultReason, HeadChange, Invoke, Invoked, JournalEntry, NativeOrigin, ProgramName, ProgramRef, ReactorIntent,
    RecordedHead, Ref, RequestSource, Requested, SetHeads, Transition, Utf8Text, Warm, WarmEntries, Warmed,
    decode_call_program, decode_set_heads,
};
use aether_bloomery_muse::{
    ContinueInput, Echo, EchoResult, Endpoint, ModelName, MuseSession, MuseTurn, OfferedTools, OpenInput, OutputBudget,
    ReasoningEffort, RecordInput, RestReason, Role, Session, SessionContinue, SessionKey, SessionOpen, SessionRecord,
    ToolCall, ToolInput, ToolOutput, TurnInput, TurnItem, TurnItems, TurnLimit, TurnOutcome, TurnResult, TurnSettings,
    offered,
};
use aether_bloomery_program::reactor::Root;
use aether_bloomery_program::{Nil, Pending, PollResult, Program, Started, invoke, start_async};
use aether_data::{Cites, Kind, Storage, StorageData};
use aether_http::FetchResult;

const CALLED_ECHO: &str = include_str!("../fixtures/called_echo.json");
const CALLED_ECHO_MORE: &str = include_str!("../fixtures/called_echo_more.json");
const COMPLETED: &str = include_str!("../fixtures/completed.json");
const ANSWER: &str = "A bloomery is a furnace that smelts iron into a bloom.";
const URL: &str = "https://example.test/v1/responses";
const QUESTION: &str = "Echo alpha and beta, then say what a bloomery is.";

type TestResult = Result<(), Box<dyn Error>>;

/// The bundle every recorded run names.
fn bundle() -> Digest {
    Digest::from_bytes([9; 32])
}

/// One call the loop asked for, recorded as a request.
struct Asked {
    /// The `Requested` entry's seq.
    requested: u64,
    call: CallProgram,
    /// The digest of the input the call runs over, staged.
    input: Digest,
}

/// The journal a driver appends, every artifact it holds, its heads, and the live reactor root it routes each
/// entry to.
struct Driver {
    root: Root<(MuseSession, Nil)>,
    entries: Vec<(JournalEntry, Vec<ClosureArtifact>)>,
    evaluated: Vec<Evaluated>,
    store: BTreeMap<Digest, EncodedArtifact>,
    heads: BTreeMap<RecordedHead, Digest>,
    replies: VecDeque<String>,
    native_keys: u64,
}

impl Driver {
    /// A driver whose turns are answered by `replies`, in order.
    fn new(replies: &[&str]) -> Self {
        Self {
            root: Root::new().expect("reactor names"),
            entries: Vec::new(),
            evaluated: Vec::new(),
            store: BTreeMap::new(),
            heads: BTreeMap::new(),
            replies: replies.iter().map(|reply| (*reply).to_owned()).collect(),
            native_keys: 0,
        }
    }

    fn stage(&mut self, artifacts: impl IntoIterator<Item = EncodedArtifact>) {
        self.store.extend(artifacts.into_iter().map(|artifact| (artifact.digest(), artifact)));
    }

    fn artifact(&self, digest: Digest) -> ClosureArtifact {
        let (kind, payload, _) = self.store.get(&digest).expect("the artifact is stored").clone().into_parts();
        ClosureArtifact::new(kind, payload)
    }

    /// The stored value at `digest`.
    fn value<K: Storage>(&self, digest: Digest) -> K {
        let payload = self.artifact(digest).load(digest).expect("stored bytes hash to their digest");
        K::decode_storage(&payload).expect("the stored value decodes").value
    }

    /// Every stored artifact `root` reaches through citations, itself included.
    fn closure(&self, root: Digest) -> Vec<ClosureArtifact> {
        let (mut seen, mut stack, mut closure) = (BTreeSet::new(), vec![root], Vec::new());
        while let Some(digest) = stack.pop() {
            let Some(artifact) = self.store.get(&digest).filter(|_| seen.insert(digest)) else {
                continue;
            };
            let cited = artifact.citations().iter().filter_map(|citation| <[u8; 32]>::try_from(citation.bytes()).ok());
            stack.extend(cited.map(Digest::from_bytes));
            closure.push(self.artifact(digest));
        }
        closure
    }

    /// Append one entry, delivering the artifacts it cites, and route it live to the reactor root.
    fn append<K: Storage + Clone>(&mut self, value: &K, cause: Option<u64>, cites: Vec<Digest>) -> u64 {
        let seq = u64::try_from(self.entries.len()).expect("a short journal") + 1;
        let bytes = K::encode_storage(&StorageData::from_value(value.clone())).expect("the entry encodes");
        let distinct: BTreeSet<Digest> = cites.iter().copied().collect();
        let artifacts: Vec<_> = distinct.into_iter().map(|digest| self.artifact(digest)).collect();
        let entry = JournalEntry { seq, kind: K::ID, cause, recorded_at_millis: 0, bytes, cites };
        self.evaluated.push(self.root.event(Event::new(entry.clone(), artifacts.clone())));
        self.entries.push((entry, artifacts));
        seq
    }

    /// The intents the reactor returned for the entry at `seq`.
    fn intents(&self, seq: u64) -> Vec<ReactorIntent> {
        match &self.evaluated[index(seq)] {
            Evaluated::Completed { intents, .. } => intents.clone(),
            other => panic!("expected entry {seq} to evaluate, got {other:?}"),
        }
    }

    /// The one intent the reactor returned for the entry at `seq`.
    fn intent(&self, seq: u64) -> ReactorIntent {
        let [intent] = self.intents(seq).try_into().expect("one intent");
        intent
    }

    /// The transition recorded at `seq`.
    fn transition(&self, seq: u64) -> Transition {
        let (entry, _) = &self.entries[index(seq)];
        assert_eq!(entry.kind, Transition::ID, "entry {seq} is a run");
        Transition::decode_storage(&entry.bytes).expect("a transition decodes").value
    }

    /// The result of the run recorded at `seq`.
    fn result<K: Storage>(&self, seq: u64) -> K {
        self.value(self.transition(seq).result)
    }

    /// Ask for a native run of `P` over `input`, and run it; the run's seq.
    fn call_native<P: Program>(&mut self, input: &P::Input) -> u64 {
        let artifact = encoded(input);
        let digest = artifact.digest();
        self.stage([artifact]);
        self.native_keys += 1;
        let origin = NativeOrigin::new("test.muse").expect("origin");
        let source = RequestSource::Native { origin, key: self.native_keys };
        let requested = self.append(&Requested { program: program::<P>(), input: digest, source }, None, Vec::new());
        self.run(&program::<P>(), digest, requested)
    }

    /// Record the one call the entry at `seq` asked for as its request, staging a value input.
    fn request(&mut self, seq: u64) -> Asked {
        let intent = self.intent(seq);
        let call = decode_call_program(intent.kind(), intent.bytes()).expect("a call");
        let input = match &call.input {
            CallInput::Stored(digest) => *digest,
            CallInput::Value(artifact) => {
                self.stage([artifact.clone()]);
                artifact.digest()
            }
        };
        let source = RequestSource::Reaction {
            bundle: bundle(),
            reactor: intent.reactor().clone(),
            rule: intent.rule().clone(),
            ordinal: 0,
        };
        let program = ProgramRef::new(bundle(), call.name.clone());
        let requested = self.append(&Requested { program, input, source }, Some(seq), Vec::new());
        Asked { requested, call, input }
    }

    /// Run the program the request asked for, and record its transition or its fault; the outcome's seq.
    fn run(&mut self, program: &ProgramRef, input: Digest, requested: u64) -> u64 {
        let invocation = Invoke::new(requested, program.name().clone(), input, self.closure(input));
        let invoked = match program.name().as_str() {
            name if name == MuseTurn::NAME => self.turn(invocation),
            name if name == Echo::NAME => invoke::<Echo>(invocation),
            name if name == SessionOpen::NAME => invoke::<SessionOpen>(invocation),
            name if name == SessionContinue::NAME => invoke::<SessionContinue>(invocation),
            name if name == SessionRecord::NAME => invoke::<SessionRecord>(invocation),
            name => panic!("no program {name}"),
        };
        match invoked {
            Invoked::Completed { result, staged, .. } => {
                self.stage(staged);
                let transition = Transition { program: program.clone(), input, result };
                self.append(&transition, Some(requested), vec![input, result])
            }
            Invoked::Refused { refusal, .. } => {
                let fault = Fault { program: program.clone(), input, reason: refusal.into() };
                self.append(&fault, Some(requested), Vec::new())
            }
            other => panic!("expected a run to complete or refuse, got {other:?}"),
        }
    }

    /// One turn, answered by the next recorded reply.
    fn turn(&mut self, invoke: Invoke) -> Invoked {
        let body = self.replies.pop_front().expect("a reply for every turn");
        let Started::Live { mut session, waiting: Some(Pending::Send(pending)) } = start_async::<MuseTurn>(invoke)
        else {
            panic!("expected the turn to send its one fetch");
        };
        let reply = FetchResult::Ok {
            request_id: 1,
            url: URL.into(),
            status: 200,
            headers: Vec::new(),
            body: body.into_bytes(),
        };
        session.fulfill_send(&pending, FetchResult::ID, reply.encode_into_bytes());
        match session.poll() {
            PollResult::Finished(invoked) => invoked,
            other => panic!("expected the turn to finish after its fetch, got {other:?}"),
        }
    }

    /// Request and run the one call the entry at `seq` asked for; the run's seq.
    fn follow(&mut self, seq: u64) -> u64 {
        let asked = self.request(seq);
        self.run(&ProgramRef::new(bundle(), asked.call.name.clone()), asked.input, asked.requested)
    }

    /// Apply the head moves the entry at `seq` asked for, each compare-and-swap checked; the last move's seq.
    fn move_heads(&mut self, seq: u64) -> (u64, SetHeads) {
        let intent = self.intent(seq);
        let heads = decode_set_heads(intent.kind(), intent.bytes()).expect("a head move");
        let mut moved = seq;
        for change in heads.changes() {
            assert_eq!(self.heads.get(change.head()).copied(), change.from(), "the compare-and-swap holds");
            self.heads.insert(change.head().clone(), change.to());
            moved = self.append(&change.to_move(), Some(seq), Vec::new());
        }
        (moved, heads)
    }

    /// Carry out every intent from the entry at `seq` on until the loop is quiet; the last entry's seq.
    fn settle(&mut self, mut seq: u64) -> u64 {
        loop {
            match self.intents(seq).as_slice() {
                [] => return seq,
                [intent] if decode_call_program(intent.kind(), intent.bytes()).is_some() => seq = self.follow(seq),
                [_] => seq = self.move_heads(seq).0,
                intents => panic!("expected at most one intent, got {intents:?}"),
            }
        }
    }

    /// The record the session `key`'s head names.
    fn head(&self, key: SessionKey) -> Digest {
        *self.heads.get(&RecordedHead::from(&key.head())).expect("the session's head is bound")
    }
}

fn index(seq: u64) -> usize {
    usize::try_from(seq - 1).expect("a short journal")
}

fn program<P: Program>() -> ProgramRef {
    ProgramRef::new(bundle(), ProgramName::new(P::NAME).expect("program name"))
}

/// `value` as the artifact that stores it.
fn encoded<K: Storage + Clone + Cites>(value: &K) -> EncodedArtifact {
    EncodedArtifact::new(value).expect("the value encodes")
}

/// Settings posting to the test endpoint and offering every bound tool, staging what they cite.
fn settings(driver: &mut Driver) -> Result<TurnSettings, Box<dyn Error>> {
    let (tools, artifacts) = offered();
    driver.stage(artifacts);
    let (endpoint, model, budget) = (Endpoint::new(URL)?, ModelName::new("muse-spark-1.3")?, OutputBudget::new(512)?);
    Ok(TurnSettings::new(endpoint, model, tools, budget, ReasoningEffort::Low))
}

/// Open a session that makes at most `max_turns` turns; the open run's seq.
fn open(driver: &mut Driver, max_turns: u32) -> Result<u64, Box<dyn Error>> {
    let settings = settings(driver)?;
    driver.stage([EncodedArtifact::text(QUESTION)]);
    let input = OpenInput::new(settings, Ref::of_text(QUESTION), TurnLimit::new(max_turns)?);
    Ok(driver.call_native::<SessionOpen>(&input))
}

/// Continue `session` from the record `from` with the user message `text`; the continue run's seq.
fn continue_from(driver: &mut Driver, session: SessionKey, from: Digest, text: &str, max_turns: u32) -> u64 {
    driver.stage([EncodedArtifact::text(text)]);
    let limit = TurnLimit::new(max_turns).expect("limit");
    driver.call_native::<SessionContinue>(&ContinueInput::new(
        session,
        Ref::from_digest(from),
        Ref::of_text(text),
        limit,
    ))
}

/// The text and the calls of a turn that asked for calls.
fn called(result: &TurnResult) -> (Ref<Utf8Text>, Vec<ToolCall>) {
    let TurnOutcome::Called { calls, text, .. } = result.outcome() else {
        panic!("expected a called turn, got {:?}", result.outcome());
    };
    (*text, calls.as_slice().to_vec())
}

/// The input `call`'s arguments decoded to.
fn decoded(call: &ToolCall) -> Digest {
    let ToolInput::Decoded(input) = call.input() else {
        panic!("expected {:?} to decode", call.call_id());
    };
    input.digest()
}

/// The program and input of the one call the entry at `seq` asked for.
fn asked(driver: &Driver, seq: u64) -> CallProgram {
    let intent = driver.intent(seq);
    decode_call_program(intent.kind(), intent.bytes()).expect("a call")
}

/// Deliver the driver's journal again at every split point, warming the prefix and routing the rest live, and
/// require every live entry to evaluate exactly as it did when the journal was built.
fn assert_warm_and_live_agree(driver: &Driver) {
    for split in 0..=driver.entries.len() {
        let mut root = Root::<(MuseSession, Nil)>::new().expect("reactor names");
        if split > 0 {
            let prefix = &driver.entries[..split];
            let entries = prefix.iter().map(|(entry, _)| entry.clone()).collect();
            let mut artifacts: Vec<ClosureArtifact> = Vec::new();
            for artifact in prefix.iter().flat_map(|(_, artifacts)| artifacts) {
                if !artifacts.iter().any(|kept| kept.claimed() == artifact.claimed()) {
                    artifacts.push(artifact.clone());
                }
            }
            let warm = Warm::new(WarmEntries::new(entries).expect("dense"), artifacts).expect("scoped");
            let warmed = root.warm(warm);
            assert!(matches!(warmed, Warmed::Folded { .. }), "split {split}: {warmed:?}");
        }
        for (index, (entry, artifacts)) in driver.entries.iter().enumerate().skip(split) {
            let live = root.event(Event::new(entry.clone(), artifacts.clone()));
            assert_eq!(live, driver.evaluated[index], "split {split}, entry {}", entry.seq);
        }
    }
}

#[test]
fn an_opened_session_runs_both_calls_in_order_then_records_and_moves_its_head() -> TestResult {
    // Catches calls run out of order or over the wrong input, a next turn that is not an exact extension of the
    // previous one or that `muse.turn` refuses, a limit counting tool calls instead of turns, a rest recorded from
    // the wrong turn, a first head move comparing against anything but an unbound head, and folds that diverge
    // between warm-up and live delivery.
    let mut driver = Driver::new(&[CALLED_ECHO, COMPLETED]);
    let opened = open(&mut driver, 2)?;
    let first_input = driver.transition(opened).result;
    let opening = asked(&driver, opened);
    assert_eq!(opening.name.as_str(), MuseTurn::NAME);
    assert_eq!(opening.input, CallInput::Stored(first_input), "the opened turn runs over the open's result");
    let first_turn = driver.follow(opened);

    let (text, calls) = called(&driver.result(first_turn));
    let [call_a, call_b] = calls.as_slice() else {
        panic!("expected two calls, got {calls:?}");
    };
    let mut trigger = first_turn;
    let mut outputs = Vec::new();
    for call in [call_a, call_b] {
        let asked = asked(&driver, trigger);
        assert_eq!(asked.name.as_str(), Echo::NAME);
        assert_eq!(asked.input, CallInput::Stored(decoded(call)), "{:?} runs over its input", call.call_id());
        trigger = driver.follow(trigger);
        let result = ErasedRef::new(EchoResult::ID, driver.transition(trigger).result);
        let output = ToolOutput::Result { schema: offered().0.as_slice()[0].result(), result };
        outputs.push(TurnItem::CallOutput { call_id: call.call_id().clone(), output });
    }

    let first: TurnInput = driver.value(first_input);
    let replayed =
        [TurnItem::message(Role::Assistant, text), TurnItem::Call(call_a.clone()), TurnItem::Call(call_b.clone())];
    let items = first.items().iter().cloned().chain(replayed).chain(outputs).collect();
    let next = TurnInput::new(
        first.endpoint().clone(),
        first.model().clone(),
        OfferedTools::new(first.tools().to_vec())?,
        TurnItems::new(items)?,
        first.max_output_tokens(),
        first.reasoning(),
    );
    let next_call = asked(&driver, trigger);
    assert_eq!(next_call.name.as_str(), MuseTurn::NAME);
    assert_eq!(next_call.input, CallInput::Value(encoded(&next)), "the next turn extends the first exactly");
    let second_turn = driver.follow(trigger);
    let second_result = driver.transition(second_turn).result;

    let record = RecordInput::new(Ref::of_encoded(&next)?, Ref::from_digest(second_result), Vec::new(), None);
    let record_call = asked(&driver, second_turn);
    assert_eq!(record_call.name.as_str(), SessionRecord::NAME);
    assert_eq!(record_call.input, CallInput::Value(encoded(&record)), "the completed turn is recorded");
    let recorded = driver.follow(second_turn);
    let session: Session = driver.result(recorded);
    assert_eq!(session.rested(), RestReason::Completed);
    let answer = TurnItem::message(Role::Assistant, Ref::of_text(ANSWER));
    assert_eq!(session.items().split_last(), Some((&answer, next.items())));

    let (moved, heads) = driver.move_heads(recorded);
    let to = Ref::from_digest(driver.transition(recorded).result);
    assert_eq!(heads.changes(), [HeadChange::new(&SessionKey::new(opened).head(), None, to)], "moved from unbound");
    assert!(driver.intents(moved).is_empty(), "the loop rests");

    assert_warm_and_live_agree(&driver);
    Ok(())
}

#[test]
fn a_call_whose_arguments_do_not_decode_goes_straight_to_the_next_turn() -> TestResult {
    // Catches a refused decode run as a tool, or answered with anything but the refusal `muse.turn` stored.
    let refusing = CALLED_ECHO.replace(r#"{\"text\": \"alpha\"}"#, "not json").replace(r#"{\"text\": \"beta\"}"#, "[]");
    let mut driver = Driver::new(&[&refusing, COMPLETED]);
    let opened = open(&mut driver, 2)?;
    let first_turn = driver.follow(opened);

    let (_, calls) = called(&driver.result(first_turn));
    let refusals: Vec<_> = calls
        .iter()
        .map(|call| match call.input() {
            ToolInput::Refused(refusal) => (call.call_id().clone(), *refusal),
            ToolInput::Decoded(_) => panic!("expected {:?} to refuse", call.call_id()),
        })
        .collect();
    let next_call = asked(&driver, first_turn);
    assert_eq!(next_call.name.as_str(), MuseTurn::NAME, "no tool runs");
    let CallInput::Value(next) = next_call.input else {
        panic!("expected the next turn as a value");
    };
    let next: TurnInput = TurnInput::decode_storage(&payload(&next))?.value;
    let answered: Vec<_> = next
        .items()
        .iter()
        .filter_map(|item| match item {
            TurnItem::CallOutput { call_id, output: ToolOutput::Refused(text) } => Some((call_id.clone(), *text)),
            _ => None,
        })
        .collect();
    assert_eq!(answered, refusals, "each call is answered with its stored refusal, in order");

    let rested = driver.settle(first_turn);
    assert!(driver.intents(rested).is_empty());
    assert_warm_and_live_agree(&driver);
    Ok(())
}

/// The payload `artifact` stores.
fn payload(artifact: &EncodedArtifact) -> Vec<u8> {
    let (kind, payload, _) = artifact.clone().into_parts();
    ClosureArtifact::new(kind, payload).load(artifact.digest()).expect("bytes hash to their digest")
}

#[test]
fn a_faulted_call_ends_the_session() -> TestResult {
    // Catches a fault replayed to the model or left waiting, so the loop would go on after a tool it could not run.
    let mut driver = Driver::new(&[CALLED_ECHO]);
    let opened = open(&mut driver, 2)?;
    let first_turn = driver.follow(opened);

    let asked = driver.request(first_turn);
    let reason = FaultReason::Panicked { message: Detail::new("echo panicked") };
    let fault = Fault { program: program::<Echo>(), input: asked.input, reason };
    let faulted = driver.append(&fault, Some(asked.requested), Vec::new());
    assert!(driver.intents(faulted).is_empty(), "the session ends at the fault");

    assert_warm_and_live_agree(&driver);
    Ok(())
}

#[test]
fn a_bare_turn_is_never_a_session() -> TestResult {
    // Catches a loop that guesses a session from a turn's content, so any turn offering bound tools would run them.
    let mut driver = Driver::new(&[CALLED_ECHO]);
    let settings = settings(&mut driver)?;
    driver.stage([EncodedArtifact::text(QUESTION)]);
    let input = TurnInput::new(
        Endpoint::new(URL)?,
        ModelName::new("muse-spark-1.3")?,
        OfferedTools::new(settings.tools().to_vec())?,
        TurnItems::new(vec![TurnItem::message(Role::User, Ref::of_text(QUESTION))])?,
        OutputBudget::new(512)?,
        ReasoningEffort::Low,
    );

    let turn = driver.call_native::<MuseTurn>(&input);
    assert_eq!(called(&driver.result(turn)).1.len(), 2, "the turn asked for bound calls");
    assert!(driver.intents(turn).is_empty(), "no session, so no call runs");
    Ok(())
}

#[test]
fn a_continue_resumes_only_from_the_latest_record_of_a_resting_session() -> TestResult {
    // Catches a continue run over a stale record or into an activation already in progress, and a later rest
    // that does not compare against the session's previous record.
    let mut driver = Driver::new(&[CALLED_ECHO, COMPLETED, COMPLETED]);
    let opened = open(&mut driver, 2)?;
    driver.settle(opened);
    let key = SessionKey::new(opened);
    let first = driver.head(key);

    let resumed = continue_from(&mut driver, key, first, "And iron?", 2);
    let resuming = asked(&driver, resumed);
    assert_eq!(resuming.name.as_str(), MuseTurn::NAME);
    assert_eq!(resuming.input, CallInput::Stored(driver.transition(resumed).result));
    let doubled = continue_from(&mut driver, key, first, "And steel?", 2);
    assert!(driver.intents(doubled).is_empty(), "a continue into an activation in progress runs nothing");

    let resumed_turn = driver.follow(resumed);
    let recorded = driver.follow(resumed_turn);
    let (_, heads) = driver.move_heads(recorded);
    let to = Ref::from_digest(driver.transition(recorded).result);
    assert_eq!(heads.changes(), [HeadChange::new(&key.head(), Some(Ref::from_digest(first)), to)]);

    let stale = continue_from(&mut driver, key, first, "And bronze?", 2);
    assert!(driver.intents(stale).is_empty(), "a continue from a stale record runs nothing");

    assert_warm_and_live_agree(&driver);
    Ok(())
}

#[test]
fn a_session_rests_at_its_turn_limit_and_a_continue_resumes_it() -> TestResult {
    // Catches a limit that counts tool calls instead of turns, a count that does not reset on continue, a limit
    // rest that sends another turn or records other items than the next turn would have sent, and a rested
    // session that cannot be continued.
    let mut driver = Driver::new(&[CALLED_ECHO, COMPLETED, CALLED_ECHO_MORE, COMPLETED]);
    let opened = open(&mut driver, 2)?;
    driver.settle(opened);
    let key = SessionKey::new(opened);
    let first = driver.head(key);

    let limited = continue_from(&mut driver, key, first, "Echo gamma.", 1);
    let limited_turn = driver.follow(limited);
    let echoed = driver.follow(limited_turn);
    assert_eq!(asked(&driver, echoed).name.as_str(), SessionRecord::NAME, "the limit records instead of turning");
    let recorded = driver.follow(echoed);
    let session: Session = driver.result(recorded);
    assert_eq!(session.rested(), RestReason::TurnLimit);
    let Some(TurnItem::CallOutput { call_id, .. }) = session.items().last() else {
        panic!("expected the session to end on the echo's output, got {:?}", session.items().last());
    };
    assert_eq!(call_id.as_str(), "call_c");
    driver.move_heads(recorded);

    let rested = driver.head(key);
    let resumed = continue_from(&mut driver, key, rested, "Now answer.", 2);
    driver.settle(resumed);
    let completed: Session = driver.value(driver.head(key));
    assert_eq!(completed.rested(), RestReason::Completed);
    let answer = TurnItem::message(Role::Assistant, Ref::of_text(ANSWER));
    assert_eq!(completed.items().last(), Some(&answer));
    assert_eq!(completed.items()[..session.items().len()], *session.items(), "the resumed session extends its rest");

    assert_warm_and_live_agree(&driver);
    Ok(())
}
