//! A Muse session driven through its reactor the way the bloomery driver drives it: each entry is routed live to
//! the reactor root, each intent the loop returns is recorded as a request and run for real, and each turn is
//! answered with a recorded reply.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error;

use aether_bloomery_kinds::{
    CLOCK, CLOCK_BUNDLE, CallInput, CallProgram, ClosureArtifact, Detail, Digest, EncodedArtifact, ErasedRef,
    Evaluated, Event, Fault, FaultReason, Fired, HeadChange, Invoke, Invoked, JournalEntry, Name, NativeOrigin, Node,
    ProgramName, ProgramRef, ReactionFailed, ReactorIntent, ReactorName, ReadArtifactResult, RecordedHead, Ref,
    RequestSource, Requested, SetHeads, Transition, Tree, Until, Utf8Text, Warm, WarmEntries, Warmed,
    decode_call_program, decode_set_heads,
};
use aether_bloomery_muse::{
    Answered, ContinueInput, Echo, EchoResult, Endpoint, Failure, InputLimit, ModelName, MuseSession, MuseTurn,
    OfferedTools, OpenInput, Opened, OutputBudget, ReadArgs, ReasoningEffort, RecordInput, RestReason, Role, Session,
    SessionContinue, SessionKey, SessionOpen, SessionRecord, ToolCall, ToolInput, ToolOutput, TreeEdit, TreeGrep,
    TreeList, TreeRead, TreeWrite, TurnInput, TurnItem, TurnItems, TurnLimit, TurnOutcome, TurnResult, TurnSettings,
    Viewed, offered,
};
use aether_bloomery_program::reactor::Root;
use aether_bloomery_program::{
    AsyncProgram, ClockUntil, Edited, Nil, Pending, PollResult, Program, Reactor, Started, invoke, start_async, tooled,
};
use aether_bloomery_workspace::TreePath;
use aether_data::{Cites, Kind, Storage, StorageData};
use aether_http::{Fetch, FetchResult, HttpError, HttpHeader};

const CALLED_ECHO: &str = include_str!("../fixtures/called_echo.json");
const CALLED_ECHO_MORE: &str = include_str!("../fixtures/called_echo_more.json");
const CALLED_EDIT_WRITE: &str = include_str!("../fixtures/called_edit_write.json");
const CALLED_LIST_READ_EDIT_GREP: &str = include_str!("../fixtures/called_list_read_edit_grep.json");
const CALLED_WRITE: &str = include_str!("../fixtures/called_write.json");
const CALLED_UNOFFERED: &str = include_str!("../fixtures/called_unoffered.json");
const COMPLETED: &str = include_str!("../fixtures/completed.json");
const CONTEXT_FULL: &str = include_str!("../fixtures/context_full.json");
const INCOMPLETE_BUDGET: &str = include_str!("../fixtures/incomplete_budget.json");
const OVERLOADED: &str = include_str!("../fixtures/overloaded.json");
const RATE_LIMITED: &str = include_str!("../fixtures/rate_limited.json");
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

/// One recorded answer to a turn's fetch.
struct Reply {
    fetched: FetchResult,
}

impl Reply {
    /// A 200 carrying `body`, with no headers.
    fn ok(body: &str) -> Self {
        Self::replied(200, Vec::new(), body)
    }

    /// A `status` refusal carrying `body`, with a `Retry-After` of `retry_after_secs` when given.
    fn refused(status: u16, body: &str, retry_after_secs: Option<u32>) -> Self {
        let headers = retry_after_secs
            .map(|secs| HttpHeader { name: "Retry-After".to_owned(), value: secs.to_string() })
            .into_iter()
            .collect();
        Self::replied(status, headers, body)
    }

    /// A fetch that got no reply, failing with `error`.
    fn failed(error: HttpError) -> Self {
        Self { fetched: FetchResult::Err { request_id: 1, url: URL.into(), error } }
    }

    fn replied(status: u16, headers: Vec<HttpHeader>, body: &str) -> Self {
        let body = body.as_bytes().to_vec();
        Self { fetched: FetchResult::Ok { request_id: 1, url: URL.into(), status, headers, body } }
    }
}

/// The journal a driver appends, every artifact it holds, its heads, and the live reactor root it routes each
/// entry to.
struct Driver {
    root: Root<(MuseSession, Nil)>,
    entries: Vec<(JournalEntry, Vec<ClosureArtifact>)>,
    evaluated: Vec<Evaluated>,
    store: BTreeMap<Digest, EncodedArtifact>,
    heads: BTreeMap<RecordedHead, Digest>,
    replies: VecDeque<Reply>,
    /// The body of every request a turn sent, in order.
    sent: Vec<serde_json::Value>,
    native_keys: u64,
    /// The journal time the next entry is recorded at.
    now_millis: u64,
}

impl Driver {
    /// A driver whose turns are answered by 200s carrying `replies`, in order.
    fn new(replies: &[&str]) -> Self {
        Self::replying(replies.iter().map(|reply| Reply::ok(reply)).collect())
    }

    /// A driver whose turns are answered by `replies`, in order.
    fn replying(replies: VecDeque<Reply>) -> Self {
        Self {
            root: Root::new().expect("reactor names"),
            entries: Vec::new(),
            evaluated: Vec::new(),
            store: BTreeMap::new(),
            heads: BTreeMap::new(),
            replies,
            sent: Vec::new(),
            native_keys: 0,
            now_millis: 0,
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
        let entry = JournalEntry { seq, kind: K::ID, cause, recorded_at_millis: self.now_millis, bytes, cites };
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

    /// The digest of the first turn the open run at `seq` staged.
    fn first_turn(&self, seq: u64) -> Digest {
        self.result::<Opened>(seq).turn().digest()
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
        let recorded_under = if call.program.as_str() == CLOCK.as_str() {
            CLOCK_BUNDLE
        } else {
            bundle()
        };
        let program = ProgramRef::new(recorded_under, call.name.clone());
        let requested = self.append(&Requested { program, input, source }, Some(seq), Vec::new());
        Asked { requested, call, input }
    }

    /// Fire the wait `asked` requested, as the driver's clock does once journal time reaches its due time; the
    /// run's seq.
    fn fire(&mut self, asked: &Asked) -> u64 {
        let Until { due_millis } = self.value(asked.input);
        let fired = encoded(&Fired { due_millis });
        let result = fired.digest();
        self.stage([fired]);
        self.now_millis = self.now_millis.max(due_millis);
        let program = ProgramRef::new(CLOCK_BUNDLE, ProgramName::new(ClockUntil::NAME).expect("program name"));
        let transition = Transition { program, input: asked.input, result };
        self.append(&transition, Some(asked.requested), vec![asked.input, result])
    }

    /// Run the program the request asked for, and record its transition or its fault; the outcome's seq.
    fn run(&mut self, program: &ProgramRef, input: Digest, requested: u64) -> u64 {
        let invocation = Invoke::new(requested, program.name().clone(), input, self.closure(input));
        let invoked = match program.name().as_str() {
            name if name == MuseTurn::NAME => self.turn(invocation),
            name if name == Echo::NAME => invoke::<Echo>(invocation),
            name if name == TreeEdit::NAME => self.run_async::<TreeEdit>(invocation),
            name if name == TreeWrite::NAME => self.run_async::<TreeWrite>(invocation),
            name if name == TreeList::NAME => self.run_async::<TreeList>(invocation),
            name if name == TreeRead::NAME => self.run_async::<TreeRead>(invocation),
            name if name == TreeGrep::NAME => self.run_async::<TreeGrep>(invocation),
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

    /// One run of a pure async program, answering every read it fetches from the store.
    fn run_async<P: AsyncProgram>(&self, invocation: Invoke) -> Invoked {
        let (mut session, mut waiting) = match start_async::<P>(invocation) {
            Started::Finished(invoked) => return invoked,
            Started::Live { session, waiting } => (session, waiting),
        };
        loop {
            let Some(Pending::Artifact(pending)) = waiting else {
                panic!("expected a pure program to wait only on reads, got {waiting:?}");
            };
            let reply = if self.store.contains_key(&pending.digest) {
                ReadArtifactResult::Found { artifact: self.artifact(pending.digest) }
            } else {
                ReadArtifactResult::Missing { digest: pending.digest }
            };
            session.fulfill(pending, reply);
            waiting = match session.poll() {
                PollResult::Finished(invoked) => return invoked,
                PollResult::NeedArtifact(pending) => Some(Pending::Artifact(pending)),
                other => panic!("expected a pure program to finish or read, got {other:?}"),
            };
        }
    }

    /// One turn, answered by the next recorded reply.
    fn turn(&mut self, invoke: Invoke) -> Invoked {
        let Reply { fetched } = self.replies.pop_front().expect("a reply for every turn");
        let Started::Live { mut session, waiting: Some(Pending::Send(pending)) } = start_async::<MuseTurn>(invoke)
        else {
            panic!("expected the turn to send its one fetch");
        };
        let fetch = Fetch::decode_from_bytes(&pending.api_call(1).payload).expect("the pending call is a fetch");
        self.sent.push(serde_json::from_slice(&fetch.body).expect("the request body is JSON"));
        session.fulfill_send(&pending, FetchResult::ID, fetched.encode_into_bytes());
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
    let limit = InputLimit::new(u64::MAX).expect("limit");
    Ok(TurnSettings::new(endpoint, model, tools, budget, ReasoningEffort::Low, limit))
}

/// The text of `src/lib.rs` in the tree a session opens on.
const LIB: &str = "pub fn smelt() {}\n";

/// Stage the tree a session opens on, `README` and `src/lib.rs`, and cite it.
fn small_tree(driver: &mut Driver) -> Ref<Tree> {
    let name = |name: &str| Name::new(name).expect("name");
    let src = Tree::new([(name("lib.rs"), Node::File(Ref::of_bytes(LIB.as_bytes())))].into());
    let root = Tree::new(
        [
            (name("README"), Node::File(Ref::of_bytes(b"# Bloomery\n"))),
            (name("src"), Node::Directory(Ref::of_encoded(&src).expect("src"))),
        ]
        .into(),
    );
    let blobs = [EncodedArtifact::opaque_bytes(LIB.as_bytes()), EncodedArtifact::opaque_bytes(b"# Bloomery\n")];
    driver.stage(blobs.into_iter().chain([encoded(&src), encoded(&root)]));
    Ref::of_encoded(&root).expect("root")
}

/// The bytes of the file at `path` in `tree`.
fn file(driver: &Driver, tree: Ref<Tree>, path: &str) -> Vec<u8> {
    let mut dir: Tree = driver.value(tree.digest());
    let (parents, leaf) = path.rsplit_once('/').unwrap_or(("", path));
    for segment in parents.split('/').filter(|segment| !segment.is_empty()) {
        let Some(Node::Directory(next)) = dir.entries().get(&Name::new(segment).expect("name")) else {
            panic!("expected {segment} to be a directory in {path}");
        };
        dir = driver.value(next.digest());
    }
    let Some(Node::File(blob)) = dir.entries().get(&Name::new(leaf).expect("name")) else {
        panic!("expected a file at {path}");
    };
    payload(&driver.store[&blob.digest()])
}

/// Open a session on the small tree that makes at most `max_turns` turns; the open run's seq and the tree.
fn open(driver: &mut Driver, max_turns: u32) -> Result<(u64, Ref<Tree>), Box<dyn Error>> {
    open_seeded(driver, max_turns, Vec::new())
}

/// Open a session on the small tree like [`open`], but resting `ContextFull`
/// once a turn is billed `tokens` input tokens; the open run's seq and the tree.
fn open_limited(driver: &mut Driver, max_turns: u32, tokens: u64) -> Result<(u64, Ref<Tree>), Box<dyn Error>> {
    let (tools, artifacts) = offered();
    driver.stage(artifacts);
    let settings = TurnSettings::new(
        Endpoint::new(URL)?,
        ModelName::new("muse-spark-1.3")?,
        tools,
        OutputBudget::new(512)?,
        ReasoningEffort::Low,
        InputLimit::new(tokens)?,
    );
    let tree = small_tree(driver);
    driver.stage([EncodedArtifact::text(QUESTION)]);
    let input = OpenInput::new(settings, Ref::of_text(QUESTION), TurnLimit::new(max_turns)?, tree, Vec::new());
    Ok((driver.call_native::<SessionOpen>(&input), tree))
}

/// Open a session on the small tree that reads `seeds` before its first turn and makes at most `max_turns` turns;
/// the open run's seq and the tree.
fn open_seeded(driver: &mut Driver, max_turns: u32, seeds: Vec<TreePath>) -> Result<(u64, Ref<Tree>), Box<dyn Error>> {
    let settings = settings(driver)?;
    let tree = small_tree(driver);
    driver.stage([EncodedArtifact::text(QUESTION)]);
    let input = OpenInput::new(settings, Ref::of_text(QUESTION), TurnLimit::new(max_turns)?, tree, seeds);
    Ok((driver.call_native::<SessionOpen>(&input), tree))
}

/// Continue `session` from the record `from` with the user message `text`; the continue run's seq.
fn continue_from(driver: &mut Driver, session: SessionKey, from: Digest, text: &str, max_turns: u32) -> u64 {
    driver.stage([EncodedArtifact::text(text)]);
    let limit = TurnLimit::new(max_turns).expect("limit");
    driver.call_native::<SessionContinue>(&ContinueInput::new(
        session,
        Ref::from_digest(from),
        Some(Ref::of_text(text)),
        None,
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

/// The arguments `call` decoded to.
fn decoded(call: &ToolCall) -> ErasedRef {
    let ToolInput::Decoded { input, .. } = call.input() else {
        panic!("expected {:?} to decode", call.call_id());
    };
    *input
}

/// The input the loop runs `call` over: `tree`, the arguments the call decoded to, and the offered bound.
fn bound(tree: Ref<Tree>, call: &ToolCall) -> CallInput {
    let program = call.program().expect("a decoded call names its program");
    let tool = offered().0.as_slice().iter().find(|tool| tool.program() == program).expect("an offered tool").clone();
    CallInput::Value(encoded(&tooled(tree, decoded(call), tool.bound())))
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
    let (opened, tree) = open(&mut driver, 2)?;
    let first_input = driver.first_turn(opened);
    let opening = asked(&driver, opened);
    assert_eq!(opening.name.as_str(), MuseTurn::NAME);
    assert_eq!(opening.input, CallInput::Stored(first_input), "the opened turn runs over the open's result");
    let first_turn = driver.follow(opened);
    assert_eq!(driver.intent(first_turn).rule().as_str(), "call", "a turn's run is not also a tool's output");

    let (text, calls) = called(&driver.result(first_turn));
    let [call_a, call_b] = calls.as_slice() else {
        panic!("expected two calls, got {calls:?}");
    };
    let mut trigger = first_turn;
    let mut outputs = Vec::new();
    for call in [call_a, call_b] {
        let asked = asked(&driver, trigger);
        assert_eq!(asked.name.as_str(), Echo::NAME);
        assert_eq!(asked.input, bound(tree, call), "{:?} runs over the tree and its arguments", call.call_id());
        trigger = driver.follow(trigger);
        assert_eq!(driver.intent(trigger).rule().as_str(), "resume", "a tool's run resumes the loop");
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
        first.input_limit(),
    );
    let next_call = asked(&driver, trigger);
    assert_eq!(next_call.name.as_str(), MuseTurn::NAME);
    assert_eq!(next_call.input, CallInput::Value(encoded(&next)), "the next turn extends the first exactly");
    let second_turn = driver.follow(trigger);
    let second_result = driver.transition(second_turn).result;

    let record = RecordInput::rested(Ref::of_encoded(&next)?, Ref::from_digest(second_result), Vec::new(), tree);
    let record_call = asked(&driver, second_turn);
    assert_eq!(record_call.name.as_str(), SessionRecord::NAME);
    assert_eq!(record_call.input, CallInput::Value(encoded(&record)), "the completed turn is recorded");
    let recorded = driver.follow(second_turn);
    let session: Session = driver.result(recorded);
    assert_eq!(*session.rested(), RestReason::Completed);
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
    let (opened, _) = open(&mut driver, 2)?;
    let first_turn = driver.follow(opened);

    let (_, calls) = called(&driver.result(first_turn));
    let refusals: Vec<_> = calls
        .iter()
        .map(|call| match call.input() {
            ToolInput::Refused { refusal, .. } => (call.call_id().clone(), *refusal),
            ToolInput::Decoded { .. } => panic!("expected {:?} to refuse", call.call_id()),
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

#[test]
fn a_call_to_an_unoffered_tool_is_answered_with_its_refusal_and_the_next_turn_completes() -> TestResult {
    // Catches a session dropped over a call to a tool the turn did not offer, that call run as a tool or answered
    // with anything but its stored refusal, its output out of order with the offered call's result, and a replay
    // under any name but the one the model wrote.
    let mut driver = Driver::new(&[CALLED_UNOFFERED, COMPLETED]);
    let (opened, tree) = open(&mut driver, 2)?;
    let first_turn = driver.follow(opened);

    let (text, calls) = called(&driver.result(first_turn));
    let [call_a, call_b] = calls.as_slice() else {
        panic!("expected two calls, got {calls:?}");
    };
    let ToolInput::Refused { name, refusal } = call_b.input() else {
        panic!("expected the unoffered call to refuse, got {:?}", call_b.input());
    };
    assert_eq!(name.as_str(), "muse-shout", "the call keeps the name the model wrote");
    let echo = asked(&driver, first_turn);
    assert_eq!(echo.name.as_str(), Echo::NAME);
    assert_eq!(echo.input, bound(tree, call_a), "only the offered call runs");
    let echoed = driver.follow(first_turn);
    let result = ErasedRef::new(EchoResult::ID, driver.transition(echoed).result);

    let next_call = asked(&driver, echoed);
    assert_eq!(next_call.name.as_str(), MuseTurn::NAME, "the refused call runs no tool");
    let CallInput::Value(next) = next_call.input else {
        panic!("expected the next turn as a value");
    };
    let next: TurnInput = TurnInput::decode_storage(&payload(&next))?.value;
    let first: TurnInput = driver.value(driver.first_turn(opened));
    let answered = [
        TurnItem::message(Role::Assistant, text),
        TurnItem::Call(call_a.clone()),
        TurnItem::Call(call_b.clone()),
        TurnItem::CallOutput {
            call_id: call_a.call_id().clone(),
            output: ToolOutput::Result { schema: offered().0.as_slice()[0].result(), result },
        },
        TurnItem::CallOutput { call_id: call_b.call_id().clone(), output: ToolOutput::Refused(*refusal) },
    ];
    assert_eq!(next.items().split_at(first.items().len()), (first.items(), answered.as_slice()));

    driver.settle(echoed);
    let session: Session = driver.value(driver.head(SessionKey::new(opened)));
    assert_eq!(*session.rested(), RestReason::Completed);
    assert_warm_and_live_agree(&driver);
    Ok(())
}

/// The payload `artifact` stores.
fn payload(artifact: &EncodedArtifact) -> Vec<u8> {
    let (kind, payload, _) = artifact.clone().into_parts();
    ClosureArtifact::new(kind, payload).load(artifact.digest()).expect("bytes hash to their digest")
}

/// The driver's record of one of the loop's rules failing on the entry at `cause`.
fn reaction_failed(driver: &mut Driver, cause: u64, reason: &str) -> u64 {
    let reactor = Some(ReactorName::new(MuseSession::NAMESPACE).expect("reactor name"));
    driver.append(&ReactionFailed { bundle: bundle(), reactor, reason: Detail::new(reason) }, Some(cause), Vec::new())
}

#[test]
fn a_faulted_call_rests_the_session_failed_with_the_calls_answered_before_it() -> TestResult {
    // Catches a fault that leaves the session unrecorded or its head unmoved, a record that replays the faulted call
    // (leaving it unanswered) or drops the call answered before it, and a failed rest that loses the session's tree.
    let mut driver = Driver::new(&[CALLED_ECHO]);
    let (opened, tree) = open(&mut driver, 2)?;
    let first_turn = driver.follow(opened);
    let echoed = driver.follow(first_turn);

    let asked = driver.request(echoed);
    let reason = FaultReason::Panicked { message: Detail::new("echo panicked") };
    let fault = Fault { program: program::<Echo>(), input: asked.input, reason: reason.clone() };
    let faulted = driver.append(&fault, Some(asked.requested), Vec::new());
    assert_eq!(driver.intent(faulted).rule().as_str(), "rest_faulted");
    assert_eq!(asked_at(&driver, faulted), SessionRecord::NAME, "the fault is recorded as the session's rest");
    let recorded = driver.follow(faulted);

    let session: Session = driver.result(recorded);
    let program = ProgramName::new(Echo::NAME)?;
    assert_eq!(*session.rested(), RestReason::Failed(Failure::Faulted { program, reason }));
    let (text, calls) = called(&driver.result(first_turn));
    let result = ErasedRef::new(EchoResult::ID, driver.transition(echoed).result);
    let output = ToolOutput::Result { schema: offered().0.as_slice()[0].result(), result };
    let first: TurnInput = driver.value(driver.first_turn(opened));
    let answered = [
        TurnItem::message(Role::Assistant, text),
        TurnItem::Call(calls[0].clone()),
        TurnItem::CallOutput { call_id: calls[0].call_id().clone(), output },
    ];
    assert_eq!(session.items().split_at(first.items().len()), (first.items(), answered.as_slice()));
    assert_eq!(session.tree(), tree);

    let (moved, heads) = driver.move_heads(recorded);
    let to = Ref::from_digest(driver.transition(recorded).result);
    assert_eq!(heads.changes(), [HeadChange::new(&SessionKey::new(opened).head(), None, to)]);
    assert!(driver.intents(moved).is_empty(), "the loop rests");

    assert_warm_and_live_agree(&driver);
    Ok(())
}

/// The name of the program the one call the entry at `seq` asked for runs.
fn asked_at(driver: &Driver, seq: u64) -> String {
    asked(driver, seq).name.as_str().to_owned()
}

#[test]
fn a_failed_rule_rests_the_session_and_a_fault_of_that_rest_ends_it_without_looping() -> TestResult {
    // Catches a rule failure that leaves the session unrecorded, a failed record that forgets the turn's text, and
    // a failed rest whose own record faults and is tried again, which would loop.
    let mut driver = Driver::new(&[CALLED_ECHO]);
    let (opened, tree) = open(&mut driver, 2)?;
    let first_turn = driver.follow(opened);

    let failed = reaction_failed(&mut driver, first_turn, "program head unbound");
    assert_eq!(driver.intent(failed).rule().as_str(), "rest_failed");
    let failure = Failure::Reaction { reason: Detail::new("program head unbound") };
    let answered = Answered::new(Ref::from_digest(driver.transition(first_turn).result), Vec::new());
    let turn = Ref::from_digest(driver.first_turn(opened));
    let record = RecordInput::failed(turn, failure, Some(answered), tree);
    assert_eq!(asked(&driver, failed).input, CallInput::Value(encoded(&record)), "the turn's text, no calls");

    let asked = driver.request(failed);
    let reason = FaultReason::Refused { reason: Detail::new("record refused") };
    let fault = Fault { program: program::<SessionRecord>(), input: asked.input, reason };
    let faulted = driver.append(&fault, Some(asked.requested), Vec::new());
    assert!(driver.intents(faulted).is_empty(), "a failed rest that cannot record itself is dropped");
    assert!(driver.heads.is_empty(), "the session never rested");

    assert_warm_and_live_agree(&driver);
    Ok(())
}

#[test]
fn a_refused_head_move_ends_the_activation_and_the_previous_head_still_continues() -> TestResult {
    // Catches a refused head move that is recorded again (which could overwrite another mover's head), and a
    // continue refused from the record the head still names because the unmoved record replaced it.
    let mut driver = Driver::new(&[CALLED_ECHO, COMPLETED, COMPLETED, COMPLETED]);
    let (opened, _) = open(&mut driver, 2)?;
    driver.settle(opened);
    let key = SessionKey::new(opened);
    let first = driver.head(key);

    let resumed = continue_from(&mut driver, key, first, "And iron?", 2);
    let resumed_turn = driver.follow(resumed);
    let recorded = driver.follow(resumed_turn);
    let refused = reaction_failed(&mut driver, recorded, "set_heads compare-and-swap mismatch");
    assert!(driver.intents(refused).is_empty(), "a refused head move records nothing more");
    assert_eq!(driver.head(key), first);

    let again = continue_from(&mut driver, key, first, "And steel?", 2);
    assert_eq!(asked_at(&driver, again), MuseTurn::NAME, "the head's record still continues");
    let again_turn = driver.follow(again);
    let rested = driver.follow(again_turn);
    let (moved, heads) = driver.move_heads(rested);
    assert_eq!(heads.changes()[0].from(), Some(first), "the rest compares against the head");
    assert!(driver.intents(moved).is_empty());

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
        settings.input_limit(),
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
    let (opened, _) = open(&mut driver, 2)?;
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
    let (opened, _) = open(&mut driver, 2)?;
    driver.settle(opened);
    let key = SessionKey::new(opened);
    let first = driver.head(key);

    let limited = continue_from(&mut driver, key, first, "Echo gamma.", 1);
    let limited_turn = driver.follow(limited);
    let echoed = driver.follow(limited_turn);
    assert_eq!(asked(&driver, echoed).name.as_str(), SessionRecord::NAME, "the limit records instead of turning");
    let recorded = driver.follow(echoed);
    let session: Session = driver.result(recorded);
    assert_eq!(*session.rested(), RestReason::TurnLimit);
    let Some(TurnItem::CallOutput { call_id, .. }) = session.items().last() else {
        panic!("expected the session to end on the echo's output, got {:?}", session.items().last());
    };
    assert_eq!(call_id.as_str(), "call_c");
    driver.move_heads(recorded);

    let rested = driver.head(key);
    let resumed = continue_from(&mut driver, key, rested, "Now answer.", 2);
    driver.settle(resumed);
    let completed: Session = driver.value(driver.head(key));
    assert_eq!(*completed.rested(), RestReason::Completed);
    let answer = TurnItem::message(Role::Assistant, Ref::of_text(ANSWER));
    assert_eq!(completed.items().last(), Some(&answer));
    assert_eq!(completed.items()[..session.items().len()], *session.items(), "the resumed session extends its rest");

    assert_warm_and_live_agree(&driver);
    Ok(())
}

#[test]
fn a_session_rests_full_when_a_turn_reaches_its_input_limit_and_a_continue_resumes_it() -> TestResult {
    // Catches a loop that sends one more vendor turn past fullness, a rest that drops the calls' outputs, a
    // `ContextFull` session no continue can pick up, and, in the second run, a diversion firing early and stalling
    // a healthy session.
    let mut driver = Driver::new(&[CONTEXT_FULL, COMPLETED]);
    let (opened, tree) = open_limited(&mut driver, 4, 900)?;
    let first_turn = driver.follow(opened);

    let (_, calls) = called(&driver.result(first_turn));
    let [call_a, call_b] = calls.as_slice() else {
        panic!("expected two calls, got {calls:?}");
    };
    let mut trigger = first_turn;
    for call in [call_a, call_b] {
        let asked = asked(&driver, trigger);
        assert_eq!(asked.name.as_str(), Echo::NAME);
        assert_eq!(asked.input, bound(tree, call), "the echo runs over the opened tree");
        trigger = driver.follow(trigger);
    }
    assert_eq!(asked(&driver, trigger).name.as_str(), SessionRecord::NAME, "the full turn records instead of turning");
    let recorded = driver.follow(trigger);
    let session: Session = driver.result(recorded);
    assert_eq!(*session.rested(), RestReason::ContextFull);
    let Some(TurnItem::CallOutput { call_id, .. }) = session.items().last() else {
        panic!("expected the session to end on the echo's output, got {:?}", session.items().last());
    };
    assert_eq!(call_id.as_str(), "call_b");
    let key = SessionKey::new(opened);
    let (moved, heads) = driver.move_heads(recorded);
    let to = Ref::from_digest(driver.transition(recorded).result);
    assert_eq!(heads.changes(), [HeadChange::new(&key.head(), None, to)], "moved from unbound");
    assert!(driver.intents(moved).is_empty(), "the loop rests");

    let full = driver.head(key);
    let resumed = continue_from(&mut driver, key, full, "Now answer.", 2);
    driver.settle(resumed);
    let completed: Session = driver.value(driver.head(key));
    assert_eq!(*completed.rested(), RestReason::Completed);
    let answer = TurnItem::message(Role::Assistant, Ref::of_text(ANSWER));
    assert_eq!(completed.items().last(), Some(&answer));
    assert_eq!(completed.items()[..session.items().len()], *session.items(), "the resumed session extends its rest");

    assert_warm_and_live_agree(&driver);

    let mut driver = Driver::new(&[CONTEXT_FULL, COMPLETED]);
    let (opened, _) = open(&mut driver, 2)?;
    driver.settle(opened);
    let completed: Session = driver.value(driver.head(SessionKey::new(opened)));
    assert_eq!(*completed.rested(), RestReason::Completed, "under a large limit the same replies complete");

    assert_warm_and_live_agree(&driver);
    Ok(())
}

#[test]
fn an_edit_binds_its_tree_into_the_next_call_and_the_session_rests_with_it() -> TestResult {
    // Catches a call run over a tree other than the latest, an `Edited` result whose tree the loop drops, a rest
    // that records the opened tree instead of the edited one, a continue that forgets the tree its record rested
    // with, and folds that diverge between warm-up and live delivery.
    let mut driver = Driver::new(&[CALLED_EDIT_WRITE, COMPLETED, CALLED_WRITE, COMPLETED]);
    let (opened, opened_tree) = open(&mut driver, 2)?;
    let first_turn = driver.follow(opened);

    let (_, calls) = called(&driver.result(first_turn));
    let [edit, write] = calls.as_slice() else {
        panic!("expected two calls, got {calls:?}");
    };
    let editing = asked(&driver, first_turn);
    assert_eq!(editing.name.as_str(), TreeEdit::NAME);
    assert_eq!(editing.input, bound(opened_tree, edit), "the edit runs over the opened tree");
    let edited_run = driver.follow(first_turn);
    let edited: Edited = driver.result(edited_run);
    assert_eq!(edited.summary(), "Edited src/lib.rs.");

    let writing = asked(&driver, edited_run);
    assert_eq!(writing.name.as_str(), TreeWrite::NAME);
    assert_eq!(writing.input, bound(edited.tree(), write), "the write runs over the edited tree");
    let written_run = driver.follow(edited_run);
    let written: Edited = driver.result(written_run);
    assert_eq!(written.summary(), "Wrote docs/notes.md.");

    driver.settle(written_run);
    let key = SessionKey::new(opened);
    let rested = driver.head(key);
    let session: Session = driver.value(rested);
    assert_eq!((session.rested(), session.tree()), (&RestReason::Completed, written.tree()));
    assert_eq!(file(&driver, session.tree(), "src/lib.rs"), b"pub fn smelt_iron() {}\n");
    assert_eq!(file(&driver, session.tree(), "docs/notes.md"), b"Iron blooms.\n");
    assert_eq!(file(&driver, session.tree(), "README"), b"# Bloomery\n");

    let resumed = continue_from(&mut driver, key, rested, "Note the slag too.", 2);
    let resumed_turn = driver.follow(resumed);
    let (_, calls) = called(&driver.result(resumed_turn));
    let [rewrite] = calls.as_slice() else {
        panic!("expected one call, got {calls:?}");
    };
    assert_eq!(
        asked(&driver, resumed_turn).input,
        bound(session.tree(), rewrite),
        "a continue works on the rested tree"
    );
    driver.settle(resumed_turn);
    let continued: Session = driver.value(driver.head(key));
    assert_eq!(file(&driver, continued.tree(), "docs/notes.md"), b"Iron blooms.\nSlag floats.\n");

    assert_warm_and_live_agree(&driver);
    Ok(())
}

/// How long after the entry at `seq` was recorded the wait that entry asked for is due.
fn waits_for(driver: &Driver, seq: u64) -> u64 {
    let wait = asked(driver, seq);
    assert_eq!((wait.program.as_str(), wait.name.as_str()), (CLOCK.as_str(), ClockUntil::NAME), "a wait");
    let CallInput::Value(until) = wait.input else {
        panic!("expected the wait's due time as a value");
    };
    let Until { due_millis } = Until::decode_storage(&payload(&until)).expect("an until decodes").value;
    due_millis - driver.entries[index(seq)].0.recorded_at_millis
}

#[test]
fn a_transient_turn_waits_out_retry_after_then_resends_the_same_turn() -> TestResult {
    // Catches seconds read as millis, a due time taken from an entry other than the refused turn or from a live
    // clock, a resent turn that is re-encoded or rebuilt instead of naming the stored input, a retry counted
    // against the turn limit (the next called turn would then rest at the limit), and warm/live divergence in the
    // wait and retry folds.
    let replies = [Reply::refused(429, RATE_LIMITED, Some(7)), Reply::ok(CALLED_ECHO), Reply::ok(COMPLETED)];
    let mut driver = Driver::replying(replies.into());
    driver.now_millis = 1_000;
    let (opened, _) = open(&mut driver, 2)?;
    let first_input = driver.first_turn(opened);

    let asked_turn = driver.request(opened);
    driver.now_millis = 5_000;
    let refused = driver.run(&program::<MuseTurn>(), asked_turn.input, asked_turn.requested);
    let outcome = driver.result::<TurnResult>(refused).outcome().clone();
    assert_eq!(outcome, TurnOutcome::Transient { retry_after_secs: Some(7) });
    assert_eq!(driver.intent(refused).rule().as_str(), "call");
    assert_eq!(waits_for(&driver, refused), 7_000, "the wait is due Retry-After after the refused turn");

    let wait = driver.request(refused);
    let fired = driver.fire(&wait);
    let retry = asked(&driver, fired);
    assert_eq!(driver.intent(fired).rule().as_str(), "retry");
    assert_eq!(retry.name.as_str(), MuseTurn::NAME);
    assert_eq!(retry.input, CallInput::Stored(first_input), "the retry resends the stored input");

    driver.settle(fired);
    let session: Session = driver.value(driver.head(SessionKey::new(opened)));
    assert_eq!(*session.rested(), RestReason::Completed, "the retry spent none of the two turns");

    assert_warm_and_live_agree(&driver);
    Ok(())
}

#[test]
fn a_timed_out_turn_backs_off_then_resends_the_same_turn() -> TestResult {
    // Catches a timed-out fetch that ends the session instead of retrying, a retry that waits no backoff, and a
    // resent turn rebuilt instead of naming the stored input.
    let mut driver = Driver::replying([Reply::failed(HttpError::Timeout), Reply::ok(COMPLETED)].into());
    let (opened, _) = open(&mut driver, 2)?;
    let first_input = driver.first_turn(opened);

    let timed_out = driver.follow(opened);
    let outcome = driver.result::<TurnResult>(timed_out).outcome().clone();
    assert_eq!(outcome, TurnOutcome::Transient { retry_after_secs: None });
    let waited = waits_for(&driver, timed_out);
    assert!((1_000..2_000).contains(&waited), "the first backoff, waited {waited}");

    let wait = driver.request(timed_out);
    let fired = driver.fire(&wait);
    let retry = asked(&driver, fired);
    assert_eq!(driver.intent(fired).rule().as_str(), "retry");
    assert_eq!(retry.input, CallInput::Stored(first_input), "the retry resends the stored input");

    driver.settle(fired);
    let session: Session = driver.value(driver.head(SessionKey::new(opened)));
    assert_eq!(*session.rested(), RestReason::Completed);

    assert_warm_and_live_agree(&driver);
    Ok(())
}

#[test]
fn a_policy_refused_turn_faults_and_rests_the_session_failed() -> TestResult {
    // Catches a refusal no resend can clear retried instead of faulting, and a turn fault that leaves the session
    // unrecorded.
    let mut driver = Driver::replying([Reply::failed(HttpError::AllowlistDenied)].into());
    let (opened, _) = open(&mut driver, 2)?;

    let faulted = driver.follow(opened);
    assert_eq!(driver.entries[index(faulted)].0.kind, Fault::ID, "the turn faults");
    assert_eq!(asked_at(&driver, faulted), SessionRecord::NAME, "the fault is recorded as the session's rest");
    let recorded = driver.follow(faulted);

    let session: Session = driver.result(recorded);
    let RestReason::Failed(Failure::Faulted { program, reason: FaultReason::Refused { reason } }) = session.rested()
    else {
        panic!("expected the session to rest failed on the turn's refusal, got {:?}", session.rested());
    };
    assert_eq!(program.as_str(), MuseTurn::NAME);
    assert!(reason.as_str().contains("AllowlistDenied"), "{}", reason.as_str());
    Ok(())
}

#[test]
fn a_turn_refused_past_the_retry_cap_rests_the_session_failed_and_a_continue_retries_it() -> TestResult {
    // Catches an unbounded retry loop, a cap off by one, a backoff that does not grow, a retry count lost across
    // waits, a turn failure left unrecorded or citing another turn's result, and a failed rest a continue cannot
    // pick up with the conversation the refused turn sent.
    let replies = (0..4).map(|_| Reply::refused(503, OVERLOADED, None)).chain([Reply::ok(COMPLETED)]).collect();
    let mut driver = Driver::replying(replies);
    let (opened, _) = open(&mut driver, 2)?;
    let mut turn = driver.follow(opened);

    for (retry, backoff) in [1_000, 2_000, 4_000].into_iter().enumerate() {
        let waited = waits_for(&driver, turn);
        assert!((backoff..backoff + 1_000).contains(&waited), "retry {retry} waits {waited}");
        let wait = driver.request(turn);
        let fired = driver.fire(&wait);
        assert_eq!(driver.intent(fired).rule().as_str(), "retry", "retry {retry}");
        turn = driver.follow(fired);
    }
    assert_eq!(driver.replies.len(), 1, "the turn was sent four times");
    assert_eq!(asked_at(&driver, turn), SessionRecord::NAME, "the fourth refusal rests the session");
    let recorded = driver.follow(turn);
    let session: Session = driver.result(recorded);
    let result = Ref::from_digest(driver.transition(turn).result);
    assert_eq!(*session.rested(), RestReason::Failed(Failure::Turn { result }));
    let first: TurnInput = driver.value(driver.first_turn(opened));
    assert_eq!(session.items(), first.items(), "the refused turn's conversation");
    driver.move_heads(recorded);

    let key = SessionKey::new(opened);
    let failed = driver.head(key);
    let resumed = continue_from(&mut driver, key, failed, "Try again.", 2);
    let retried: TurnInput = driver.value(driver.transition(resumed).result);
    let again = TurnItem::message(Role::User, Ref::of_text("Try again."));
    assert_eq!(retried.items().split_last(), Some((&again, first.items())));
    driver.settle(resumed);
    let completed: Session = driver.value(driver.head(key));
    assert_eq!(*completed.rested(), RestReason::Completed);

    assert_warm_and_live_agree(&driver);
    Ok(())
}

#[test]
fn a_turn_out_of_budget_rests_on_what_it_sent_and_a_continue_resends_it_with_a_larger_budget() -> TestResult {
    // Catches an empty assistant message recorded after a turn that spent its budget on reasoning, a resend that
    // appends anything to the conversation the failed turn sent or changes its cache key, and a budget the
    // continue does not send.
    let mut driver = Driver::new(&[INCOMPLETE_BUDGET, COMPLETED]);
    let (opened, _) = open(&mut driver, 2)?;
    driver.settle(opened);
    let key = SessionKey::new(opened);
    let first: TurnInput = driver.value(driver.first_turn(opened));
    let incomplete: Session = driver.value(driver.head(key));
    assert_eq!(*incomplete.rested(), RestReason::Incomplete);
    assert_eq!(incomplete.items(), first.items(), "the session ends on the opening user message");

    let budget = OutputBudget::new(4096)?;
    let input = ContinueInput::new(key, Ref::from_digest(driver.head(key)), None, Some(budget), TurnLimit::new(2)?);
    let resumed = driver.call_native::<SessionContinue>(&input);
    driver.settle(resumed);
    let [first_sent, resent] = driver.sent.as_slice() else {
        panic!("expected two requests, got {}", driver.sent.len());
    };
    assert_eq!(resent["input"], first_sent["input"], "the resend adds nothing to the conversation");
    assert_eq!(resent["prompt_cache_key"], first_sent["prompt_cache_key"]);
    assert_eq!(first_sent["max_output_tokens"], 512);
    assert_eq!(resent["max_output_tokens"], 4096);
    let completed: Session = driver.value(driver.head(key));
    assert_eq!(*completed.rested(), RestReason::Completed);
    let tools = OfferedTools::new(first.tools().to_vec())?;
    let kept = TurnSettings::new(
        first.endpoint().clone(),
        first.model().clone(),
        tools,
        budget,
        first.reasoning(),
        first.input_limit(),
    );
    assert_eq!(*completed.settings(), kept, "later continues keep the new budget");

    assert_warm_and_live_agree(&driver);
    Ok(())
}

#[test]
fn a_session_lists_reads_edits_and_greps_its_tree_and_rests_with_the_edit() -> TestResult {
    // Catches a read-only result the loop mistakes for an edit, a read-only tool the loop cannot run, and folds
    // that diverge between warm-up and live delivery.
    let mut driver = Driver::new(&[CALLED_LIST_READ_EDIT_GREP, COMPLETED]);
    let (opened, opened_tree) = open(&mut driver, 2)?;
    let first_turn = driver.follow(opened);

    let (_, calls) = called(&driver.result(first_turn));
    let [list, read, edit, grep] = calls.as_slice() else {
        panic!("expected four calls, got {calls:?}");
    };
    let mut trigger = first_turn;
    for (call, name, text) in [(list, TreeList::NAME, "file\tlib.rs"), (read, TreeRead::NAME, "1\tpub fn smelt() {}")] {
        let asked = asked(&driver, trigger);
        assert_eq!(asked.name.as_str(), name);
        assert_eq!(asked.input, bound(opened_tree, call), "{name} runs over the opened tree");
        trigger = driver.follow(trigger);
        let viewed: Viewed = driver.result(trigger);
        assert_eq!(viewed.text(), text, "{name}");
    }

    let editing = asked(&driver, trigger);
    assert_eq!(editing.name.as_str(), TreeEdit::NAME);
    assert_eq!(editing.input, bound(opened_tree, edit), "a viewed result leaves the tree where it was");
    let edited_run = driver.follow(trigger);
    let edited: Edited = driver.result(edited_run);

    let grepping = asked(&driver, edited_run);
    assert_eq!(grepping.name.as_str(), TreeGrep::NAME);
    assert_eq!(grepping.input, bound(edited.tree(), grep), "the grep runs over the edited tree");
    let grepped_run = driver.follow(edited_run);
    let grepped: Viewed = driver.result(grepped_run);
    assert_eq!(grepped.text(), "src/lib.rs:1:pub fn smelt_iron() {}");

    driver.settle(grepped_run);
    let session: Session = driver.value(driver.head(SessionKey::new(opened)));
    assert_eq!((session.rested(), session.tree()), (&RestReason::Completed, edited.tree()));

    assert_warm_and_live_agree(&driver);
    Ok(())
}

/// The fixture's read call with its arguments replaced by `arguments`, a JSON object spelled as it sits in the
/// fixture's escaped string.
fn reading(arguments: &str) -> String {
    CALLED_LIST_READ_EDIT_GREP.replace(r#"{\"path\": \"src/lib.rs\"}"#, arguments)
}

#[test]
fn a_read_whose_integers_arrive_as_strings_runs_the_requested_window() -> TestResult {
    // Catches string integers refused at the decode, and a coercion that runs the read over any other window.
    let windowed = reading(r#"{\"path\": \"src/lib.rs\", \"from_line\": \"1\", \"lines\": \"1\"}"#);
    let mut driver = Driver::new(&[&windowed, COMPLETED]);
    let (opened, opened_tree) = open(&mut driver, 2)?;
    let first_turn = driver.follow(opened);

    let (_, calls) = called(&driver.result(first_turn));
    let read = &calls[1];
    let args = ReadArgs::new(TreePath::new("src/lib.rs")?, Some(1), Some(1));
    assert_eq!(decoded(read).cast::<ReadArgs>(), Some(Ref::of_encoded(&args)?), "decodes as the integers would");

    let listed = driver.follow(first_turn);
    let asked = asked(&driver, listed);
    assert_eq!(asked.name.as_str(), TreeRead::NAME);
    assert_eq!(asked.input, bound(opened_tree, read));
    let read_run = driver.follow(listed);
    let viewed: Viewed = driver.result(read_run);
    assert_eq!(viewed.text(), "1\tpub fn smelt() {}");
    Ok(())
}

#[test]
fn a_read_with_a_non_numeric_string_is_refused_with_how_to_fix_it_and_the_turn_continues() -> TestResult {
    // Catches a mistyped integer that runs, or is answered with the codec's terse refusal instead of the correction.
    let mistyped = reading(r#"{\"path\": \"src/lib.rs\", \"from_line\": \"one\"}"#);
    let mut driver = Driver::new(&[&mistyped, COMPLETED]);
    let (opened, _) = open(&mut driver, 2)?;
    let first_turn = driver.follow(opened);

    let (_, calls) = called(&driver.result(first_turn));
    let ToolInput::Refused { refusal, .. } = calls[1].input() else {
        panic!("expected the read to refuse, got {:?}", calls[1].input());
    };
    let text = "The arguments do not decode as muse.tree.read.args: `from_line` must be a JSON number, a whole number \
                from 0 to 4294967295, e.g. `3`, not `\"one\"`";
    assert_eq!(*refusal, Ref::of_text(text));

    let mut trigger = first_turn;
    while asked(&driver, trigger).name.as_str() != MuseTurn::NAME {
        trigger = driver.follow(trigger);
    }
    let CallInput::Value(next) = asked(&driver, trigger).input else {
        panic!("expected the next turn as a value");
    };
    let next: TurnInput = TurnInput::decode_storage(&payload(&next))?.value;
    let answered = next.items().iter().any(|item| {
        matches!(item, TurnItem::CallOutput { call_id, output: ToolOutput::Refused(at) }
            if call_id == calls[1].call_id() && *at == Ref::of_text(text))
    });
    assert!(answered, "the next turn carries the correction as the read's output");
    Ok(())
}

#[test]
fn a_seeded_session_reads_its_seeds_as_calls_before_the_first_turn_and_replays_them() -> TestResult {
    // Catches seeds rendered without running `tree.read`, run over another tree or path, sent before the user
    // message or with outputs other than their runs' results, counted as a turn, or dropped from later turns and the
    // record, and seed folds that diverge between warm-up and live delivery.
    let mut driver = Driver::new(&[CALLED_ECHO, COMPLETED]);
    let seeds = vec![TreePath::new("src/lib.rs")?, TreePath::new("README")?];
    let (opened, tree) = open_seeded(&mut driver, 2, seeds)?;
    let calls = driver.result::<Opened>(opened).seeds().expect("seeded reads").as_slice().to_vec();
    assert_eq!(driver.intent(opened).rule().as_str(), "seed", "the seeds run before the first turn");

    let mut trigger = opened;
    let mut outputs = Vec::new();
    for (call, text) in calls.iter().zip(["1\tpub fn smelt() {}", "1\t# Bloomery"]) {
        let asked = asked(&driver, trigger);
        assert_eq!(asked.name.as_str(), TreeRead::NAME);
        assert_eq!(asked.input, bound(tree, call), "{:?} reads over the opened tree", call.call_id());
        trigger = driver.follow(trigger);
        let viewed: Viewed = driver.result(trigger);
        assert_eq!(viewed.text(), text);
        outputs.push(serde_json::json!({ "text": viewed.text() }));
    }
    let first_turn = asked(&driver, trigger);
    assert_eq!(first_turn.name.as_str(), MuseTurn::NAME, "the first turn follows the last seed");
    let rested = driver.settle(trigger);
    assert!(driver.intents(rested).is_empty(), "the loop rests");

    let [first_sent, second_sent] = driver.sent.as_slice() else {
        panic!("expected two requests, got {}", driver.sent.len());
    };
    let sent = first_sent["input"].as_array().expect("items are sent");
    let [user, call_0, call_1, output_0, output_1] = sent.as_slice() else {
        panic!("expected the user message, two calls, and two outputs, got {sent:?}");
    };
    assert_eq!(user["role"], "user");
    assert_eq!(user["content"][0]["text"], QUESTION);
    for (call, (id, path)) in [call_0, call_1].into_iter().zip([("seed-0", "src/lib.rs"), ("seed-1", "README")]) {
        assert_eq!(call["type"], "function_call");
        assert_eq!(call["call_id"], id);
        assert_eq!(call["arguments"], format!(r#"{{"path":"{path}","lines":2000}}"#));
    }
    for (output, (id, viewed)) in [output_0, output_1].into_iter().zip(["seed-0", "seed-1"].into_iter().zip(&outputs)) {
        assert_eq!(output["type"], "function_call_output");
        assert_eq!(output["call_id"], id);
        let rendered: serde_json::Value = serde_json::from_str(output["output"].as_str().expect("a rendered output"))?;
        assert_eq!(&rendered, viewed, "{id} sends what its read returned");
    }
    let replayed = second_sent["input"].as_array().expect("items are sent");
    assert_eq!(replayed.get(..sent.len()), Some(sent.as_slice()), "a later turn replays the seeds unchanged");

    let session: Session = driver.value(driver.head(SessionKey::new(opened)));
    assert_eq!(*session.rested(), RestReason::Completed, "the seeds spent none of the two turns");
    let first: TurnInput = driver.value(driver.first_turn(opened));
    let seeded: Vec<_> = calls.iter().cloned().map(TurnItem::Call).collect();
    assert_eq!(session.items().first(), first.items().first());
    assert_eq!(session.items().get(1..3), Some(seeded.as_slice()), "the record holds the seeded calls");

    assert_warm_and_live_agree(&driver);
    Ok(())
}
