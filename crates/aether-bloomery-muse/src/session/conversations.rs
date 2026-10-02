//! The live sessions, folded from the journal: which entry belongs to which
//! session, what each waiting turn still owes, each session's current tree,
//! and each session's latest record with the tree it rested with.
//!
//! Every hop of a session is one cause lookup. An entry the loop acts on is
//! linked to its session's key; the `Requested` entry the loop's intent
//! records is caused by that entry and takes the link over, and the run it
//! requested is caused by the `Requested` entry and takes it over in turn.
//! Each link is removed when it is used, so only live hops are kept. A wait
//! before a retried turn is one more hop: the clock's run is caused by the
//! `Requested` entry of the wait and takes the link over like a tool's run.
//!
//! A session that fails, on a fault of a run the loop requested, a failure of
//! one of the loop's own rules, a turn the vendor ended, or a request the loop
//! cannot build, stays linked at the failing entry with its failed record to
//! make next. A failure of that record, or of any rest's head move, drops the
//! session: it cannot record itself, and one attempt keeps it from looping.

use std::collections::BTreeMap;

use aether_bloomery_kinds::{
    CallInput, CallProgram, Detail, EncodedArtifact, ErasedRef, Fault, Head, HeadChange, HeadMoved, OpaqueBytes,
    ProgramName, ReactionFailed, Ref, RequestSource, Requested, Seq, SetHeads, Transition, Tree, Until, Utf8Text,
};
use aether_bloomery_program::{At, Cited, CitedError, ClockUntil, Edited, Ran, Reactor, ViewCursor, tooled, view};

use crate::input::{Role, ToolCalls, ToolInput, ToolOutput, TurnInput, TurnItem};
use crate::program::MuseTurn;
use crate::result::{TurnOutcome, TurnResult};
use crate::session::MuseSession;
use crate::session::continue_::SessionContinue;
use crate::session::exhausted::{ExhaustedInput, Exhaustion, MAX_TOOL_RETRIES, SessionExhausted};
use crate::session::open::SessionOpen;
use crate::session::record::{Answered, CallAnswer, RecordInput, SessionRecord};
use crate::session::replay::replay;
use crate::session::retry::{MAX_RETRIES, retry_wait, wait_call};
use crate::session::state::{Failure, Session, SessionKey, TurnLimit};
use crate::session::tools::call;
use crate::tools::{NUDGE_TEXT, ends_run};

/// Every live session and the journal entries linked to them.
#[derive(Default)]
pub struct Conversations {
    cursor: ViewCursor,
    /// From an entry the loop acts on to its session.
    links: BTreeMap<Seq, SessionKey>,
    /// Each session with an activation in progress.
    sessions: BTreeMap<SessionKey, Conversation>,
    /// Each session's latest record, from its head's own moves.
    current: BTreeMap<SessionKey, Ref<Session>>,
    /// Each session's last record the head moved to and the tree it rested
    /// with, which a continue from that record works on.
    rested: BTreeMap<SessionKey, (Ref<Session>, Ref<Tree>)>,
    /// Each session's latest record and its tree, until the head moves to it.
    recorded: BTreeMap<SessionKey, (Ref<Session>, Ref<Tree>)>,
}

/// One session's activation in progress.
struct Conversation {
    /// The most turns this activation may make.
    limit: TurnLimit,
    /// The turns it has made. A retried turn counts once.
    turns: u32,
    /// The last turn it sent.
    turn: Ref<TurnInput>,
    /// The retries made of the turn now waiting to be retried, reset by any
    /// turn the vendor did not refuse as transient.
    retries: u32,
    /// The tree every call works on: the one the activation started from, or
    /// the one the last `Edited` result left.
    tree: Ref<Tree>,
    /// The turn whose calls are running, if one asked for calls.
    waiting: Option<Waiting>,
    /// The seq of the tool run answered last.
    answered: Option<Seq>,
    /// The runs of the call now running that ran out of time or memory, reset
    /// whenever a call is answered.
    exhaustions: u32,
    /// What the loop runs next.
    next: Option<Next>,
}

/// A turn that asked for calls, or an open's first turn with the seeded reads
/// that run before it, and the outputs of the calls answered so far.
struct Waiting {
    input: TurnInput,
    turn: Ref<TurnInput>,
    /// The result of the turn that asked for the calls; `None` for seeds,
    /// which no turn asked for.
    result: Option<Ref<TurnResult>>,
    text: Ref<Utf8Text>,
    calls: ToolCalls,
    outputs: Vec<CallAnswer>,
    /// Whether the turn that asked for the calls reached its input limit, so
    /// the session rests once every call has its output. `false` for seeds,
    /// which no turn asked for.
    full: bool,
}

/// What the loop runs after the entry linked to a session.
enum Next {
    /// A called program from the bundle its offer names, over the session's
    /// tree and the arguments its call decoded to.
    Call { head: Head<OpaqueBytes>, program: ProgramName, input: EncodedArtifact },
    /// The next turn, once every call has its output, or after a reply
    /// without calls with the reply text and the nudge appended.
    Turn(TurnInput),
    /// The record of a session resting once its last call has its output: on
    /// an end call, at its turn limit, or past its input limit.
    Settled(RecordInput),
    /// The record of a session whose turn rested it: a refused reply, a
    /// truncated reply, or a reply without calls at a limit.
    Rest(RecordInput),
    /// The record of a session that failed.
    Fail(RecordInput),
    /// The wait before `turn` is sent again, after the vendor refused it as
    /// transient.
    Wait { turn: Ref<TurnInput>, until: Until },
    /// `turn` sent again, once its wait fired.
    Retry(Ref<TurnInput>),
    /// The answer to the call now running, whose every attempt ran out of
    /// time or memory.
    Exhausted(ExhaustedInput),
}

impl Conversation {
    const fn new(limit: TurnLimit, turn: Ref<TurnInput>, tree: Ref<Tree>) -> Self {
        Self { limit, turns: 0, turn, retries: 0, tree, waiting: None, answered: None, exhaustions: 0, next: None }
    }

    /// The record of this session failing with `failure`: the last turn it
    /// sent, with that turn's calls answered so far when it asked for any.
    /// A session failing in its seeded reads cites the first turn alone.
    fn failure(&self, failure: Failure) -> RecordInput {
        let answered = self
            .waiting
            .as_ref()
            .filter(|waiting| waiting.turn == self.turn)
            .and_then(|waiting| waiting.result.map(|result| Answered::new(result, waiting.outputs.clone())));
        RecordInput::failed(self.turn, failure, answered, self.tree)
    }

    /// Answer every refused call up to the next decoded one, and set what runs
    /// next, or say why the loop cannot build it.
    fn advance(&mut self) -> Result<(), Detail> {
        let waiting = self.waiting.as_mut().ok_or_else(|| Detail::new("no turn waits on calls"))?;
        let answered = waiting.outputs.len();
        for call in &waiting.calls.as_slice()[answered..] {
            match call.input() {
                ToolInput::Refused { refusal, .. } => {
                    waiting.outputs.push(CallAnswer::new(call.call_id().clone(), ToolOutput::Refused(*refusal)));
                }
                ToolInput::Decoded { program, input } => {
                    let tool = waiting
                        .input
                        .tools()
                        .iter()
                        .find(|tool| tool.program() == program)
                        .ok_or_else(|| Detail::new(format!("{} is not an offered tool", program.as_str())))?;
                    let input = EncodedArtifact::new(&tooled(self.tree, *input, tool.bound()))
                        .map_err(|error| Detail::new(format!("a call's input did not encode: {error}")))?;
                    self.next = Some(Next::Call { head: tool.head().clone(), program: program.clone(), input });
                    return Ok(());
                }
            }
        }
        let ended = ends_run(waiting.calls.as_slice(), &waiting.outputs);
        let spent = self.turns >= self.limit.get();
        let settles = ended || waiting.full || spent;
        self.next = Some(match waiting.result {
            Some(result) if settles => {
                Next::Settled(RecordInput::rested(waiting.turn, result, waiting.outputs.clone(), self.tree))
            }
            _ => {
                let replayed = replay(waiting.text, waiting.calls.as_slice(), &waiting.outputs);
                Next::Turn(
                    waiting
                        .input
                        .append(replayed)
                        .map_err(|error| Detail::new(format!("the next turn's items: {error}")))?,
                )
            }
        });
        Ok(())
    }

    /// Whether `run` is the run of the call the loop runs next.
    fn awaits(&self, run: &Transition) -> bool {
        matches!(
            &self.next,
            Some(Next::Call { program, input, .. }) if program == run.program.name() && input.digest() == run.input
        )
    }

    /// Record `result`, the result of the tool run at `seq`, as the output of
    /// the call the loop ran, citing the result schema its turn offered.
    /// `None` when the call does not run or the turn offered no such tool.
    fn answer(&mut self, result: ErasedRef, seq: Seq) -> Option<()> {
        let waiting = self.waiting.as_mut()?;
        let call = waiting.calls.as_slice().get(waiting.outputs.len())?;
        let program = call.program()?;
        let schema = waiting.input.tools().iter().find(|tool| tool.program() == program)?.result();
        waiting.outputs.push(CallAnswer::new(call.call_id().clone(), ToolOutput::Result { schema, result }));
        self.answered = Some(seq);
        self.exhaustions = 0;
        Some(())
    }

    /// Answer the call now running with `refusal`, the staged answer to its
    /// exhausted attempts, at `seq`; whether a call waited for an answer.
    fn refuse(&mut self, refusal: Ref<Utf8Text>, seq: Seq) -> bool {
        let Some(waiting) = self.waiting.as_mut() else {
            return false;
        };
        let Some(call) = waiting.calls.as_slice().get(waiting.outputs.len()) else {
            return false;
        };
        waiting.outputs.push(CallAnswer::new(call.call_id().clone(), ToolOutput::Refused(refusal)));
        self.answered = Some(seq);
        self.exhaustions = 0;
        true
    }

    /// What runs after `fault` of the call now running, when it ran out of
    /// time or memory: the same run again below the retry cap, and the
    /// staged answer to its attempts at the cap. `None` for any other fault.
    fn exhausted(&mut self, fault: &Fault) -> Option<Next> {
        let resource = Exhaustion::of(&fault.reason)?;
        let Some(Next::Call { program, input }) = &self.next else {
            return None;
        };
        let same_program = program == fault.program.name();
        let same_input = input.digest() == fault.input;
        let answers_the_call = same_program && same_input;
        if !answers_the_call {
            return None;
        }
        self.exhaustions += 1;
        if self.exhaustions <= MAX_TOOL_RETRIES {
            return Some(Next::Call { program: program.clone(), input: input.clone() });
        }
        Some(Next::Exhausted(ExhaustedInput::new(program.clone(), resource, self.exhaustions)))
    }
}

impl Conversations {
    /// The call the loop makes after the entry at `at`, when it follows a turn
    /// or a call that did not rest the session.
    pub fn step(&self, at: At) -> Option<CallProgram> {
        match self.next(at)? {
            Next::Call { head, program, input } => Some(CallProgram {
                program: head.clone(),
                name: program.clone(),
                input: CallInput::Value(input.clone()),
            }),
            Next::Turn(turn) => Some(call::<MuseTurn>(CallInput::Value(EncodedArtifact::new(turn).ok()?))),
            Next::Settled(record) | Next::Fail(record) => {
                Some(call::<SessionRecord>(CallInput::Value(EncodedArtifact::new(record).ok()?)))
            }
            Next::Wait { until, .. } => wait_call(*until),
            Next::Retry(turn) => Some(call::<MuseTurn>(CallInput::Stored(turn.digest()))),
            Next::Exhausted(exhausted) => {
                Some(call::<SessionExhausted>(CallInput::Value(EncodedArtifact::new(exhausted).ok()?)))
            }
            Next::Rest(_) => None,
        }
    }

    /// The call the loop makes after the tool run at `at`.
    pub fn resume(&self, at: At) -> Option<CallProgram> {
        let session = self.sessions.get(self.links.get(&at.seq)?)?;
        session.answered.filter(|answered| *answered == at.seq).and_then(|_| self.step(at))
    }

    /// The record the loop makes after the turn at `at`, when that turn rested
    /// the session.
    pub fn rest(&self, at: At) -> Option<CallProgram> {
        match self.next(at)? {
            Next::Rest(record) => Some(call::<SessionRecord>(CallInput::Value(EncodedArtifact::new(record).ok()?))),
            Next::Call { .. }
            | Next::Turn(_)
            | Next::Settled(_)
            | Next::Fail(_)
            | Next::Wait { .. }
            | Next::Retry(_)
            | Next::Exhausted(_) => None,
        }
    }

    /// The first turn of the session the entry at `at` opened or continued,
    /// when that turn runs next: an open with seeded reads runs them first.
    pub fn starts(&self, at: At) -> Option<Ref<TurnInput>> {
        let session = self.sessions.get(self.links.get(&at.seq)?)?;
        session.next.is_none().then_some(session.turn)
    }

    /// The move of the session head the record at `at` belongs to, from the
    /// session's latest record to `to`.
    pub fn moved(&self, at: At, to: Ref<Session>) -> Option<SetHeads> {
        let key = *self.links.get(&at.seq)?;
        Some(SetHeads::new(vec![HeadChange::new(&key.head(), self.current.get(&key).copied(), to)]))
    }

    fn next(&self, at: At) -> Option<&Next> {
        self.sessions.get(self.links.get(&at.seq)?)?.next.as_ref()
    }

    /// Take the session the entry at `cause` is linked to, with its
    /// conversation.
    fn take(&mut self, cause: Option<Seq>) -> Option<(SessionKey, Conversation)> {
        let key = self.links.remove(&cause?)?;
        self.sessions.remove(&key).map(|conversation| (key, conversation))
    }

    /// Keep `conversation` for `key`, and link the entry at `seq` to it.
    fn keep(&mut self, key: SessionKey, conversation: Conversation, seq: Seq) {
        self.sessions.insert(key, conversation);
        self.links.insert(seq, key);
    }

    /// Set what runs next for `key` after the entry at `seq`, or fail the
    /// session when the loop cannot build it.
    fn advance(&mut self, key: SessionKey, mut conversation: Conversation, seq: Seq) {
        match conversation.advance() {
            Ok(()) => self.keep(key, conversation, seq),
            Err(reason) => self.fail(key, conversation, Failure::Unbuilt { reason }, seq),
        }
    }

    /// Record `key` failing with `failure` after the entry at `seq`, or drop
    /// the session when the failure is of its failed record: that session
    /// cannot record itself.
    fn fail(&mut self, key: SessionKey, mut conversation: Conversation, failure: Failure, seq: Seq) {
        if !matches!(conversation.next, Some(Next::Fail(_))) {
            conversation.next = Some(Next::Fail(conversation.failure(failure)));
            self.keep(key, conversation, seq);
        }
    }
}

#[view(cursor = cursor)]
impl View for Conversations {
    /// An open: the session starts on its first turn, or, with seeded
    /// reads, waits on them as calls with no turn result and empty text, so
    /// the first turn sends the user message, the calls, then their outputs.
    /// Seeds are not a turn.
    #[fold]
    fn opened(&mut self, run: Ran<SessionOpen>, cited: &Cited, at: At) -> Result<(), CitedError> {
        let (input, opened) = (cited.get(run.input())?, cited.get(run.result())?);
        let (key, turn) = (SessionKey::new(at.seq.0), opened.turn());
        let mut conversation = Conversation::new(input.max_turns(), turn, input.tree());
        match opened.seeds() {
            None => self.keep(key, conversation, at.seq),
            Some(seeds) => {
                let (calls, text, first) = (seeds.clone(), Ref::of_text(""), input.first_turn());
                let waiting =
                    Waiting { input: first, turn, result: None, text, calls, outputs: Vec::new(), full: false };
                conversation.waiting = Some(waiting);
                self.advance(key, conversation, at.seq);
            }
        }
        Ok(())
    }

    #[fold]
    fn continued(&mut self, run: Ran<SessionContinue>, cited: &Cited, at: At) -> Result<(), CitedError> {
        let input = cited.get(run.input())?;
        let key = input.session();
        let rested = self.rested.get(&key).filter(|(from, _)| *from == input.from());
        if let Some(&(_, tree)) = rested
            && self.current.get(&key) == Some(&input.from())
            && !self.sessions.contains_key(&key)
        {
            self.keep(key, Conversation::new(input.max_turns(), run.result(), tree), at.seq);
        }
        Ok(())
    }

    #[fold]
    fn turned(&mut self, run: Ran<MuseTurn>, cited: &Cited, at: At) -> Result<(), CitedError> {
        let Some((key, mut conversation)) = self.take(at.cause) else {
            return Ok(());
        };
        let (input, outcome) = (cited.get(run.input())?, cited.get(run.result())?.outcome().clone());
        conversation.turn = run.input();
        match outcome {
            TurnOutcome::Transient { retry_after_secs } if conversation.retries < MAX_RETRIES => {
                let until = retry_wait(at, retry_after_secs, conversation.retries);
                conversation.retries += 1;
                conversation.next = Some(Next::Wait { turn: run.input(), until });
                self.keep(key, conversation, at.seq);
            }
            TurnOutcome::Called { calls, text, usage } => {
                conversation.turns += 1;
                conversation.retries = 0;
                let (turn, result) = (run.input(), Some(run.result()));
                let full = input.input_limit().reached(usage.input_tokens());
                conversation.waiting = Some(Waiting { input, turn, result, text, calls, outputs: Vec::new(), full });
                self.advance(key, conversation, at.seq);
            }
            TurnOutcome::Completed { text, usage } => {
                conversation.turns += 1;
                conversation.retries = 0;
                let spent = conversation.turns >= conversation.limit.get();
                let full = input.input_limit().reached(usage.input_tokens());
                let nudged =
                    [TurnItem::message(Role::Assistant, text), TurnItem::message(Role::User, Ref::of_text(NUDGE_TEXT))];
                conversation.next = Some(if spent || full {
                    Next::Rest(RecordInput::rested(run.input(), run.result(), Vec::new(), conversation.tree))
                } else {
                    match input.append(nudged) {
                        Ok(turn) => Next::Turn(turn),
                        Err(error) => {
                            let reason = Detail::new(format!("the next turn's items: {error}"));
                            Next::Fail(conversation.failure(Failure::Unbuilt { reason }))
                        }
                    }
                });
                self.keep(key, conversation, at.seq);
            }
            TurnOutcome::Declined { .. } | TurnOutcome::Incomplete { .. } => {
                conversation.turns += 1;
                conversation.retries = 0;
                let record = RecordInput::rested(run.input(), run.result(), Vec::new(), conversation.tree);
                conversation.next = Some(Next::Rest(record));
                self.keep(key, conversation, at.seq);
            }
            TurnOutcome::Rejected | TurnOutcome::Transient { .. } | TurnOutcome::Unreadable => {
                self.fail(key, conversation, Failure::Turn { result: run.result() }, at.seq);
            }
        }
        Ok(())
    }

    /// Any program's run: the output of a tool call when the run answers the
    /// `Requested` the loop recorded for the call it runs next. A result that
    /// is an [`Edited`] moves the session to its tree before the next call.
    /// Every other run, `muse.turn` and the session programs included, is
    /// left to its own fold.
    #[fold]
    fn ran(&mut self, run: Transition, cited: &Cited, at: At) -> Result<(), CitedError> {
        let linked = at.cause.and_then(|cause| self.sessions.get(self.links.get(&cause)?));
        if !linked.is_some_and(|conversation| conversation.awaits(&run)) {
            return Ok(());
        }
        let result = ErasedRef::new(cited.kind(run.result)?, run.result);
        let edited = result.cast::<Edited>().map(|edited| cited.get(edited)).transpose()?;
        if let Some((key, mut conversation)) = self.take(at.cause) {
            if conversation.answer(result, at.seq).is_some() {
                conversation.tree = edited.map_or(conversation.tree, |edited| edited.tree());
                self.advance(key, conversation, at.seq);
            } else {
                conversation.answered = Some(at.seq);
                let failure = Failure::Unbuilt { reason: Detail::new("the run's result answers no offered call") };
                self.fail(key, conversation, failure, at.seq);
            }
        }
        Ok(())
    }

    /// The clock's run: the wait before a retried turn fired, when the run
    /// answers the `Requested` the loop recorded for that wait.
    #[fold]
    fn fired(&mut self, _run: Ran<ClockUntil>, at: At) {
        let linked = at.cause.and_then(|cause| self.sessions.get(self.links.get(&cause)?));
        if !linked.is_some_and(|conversation| matches!(conversation.next, Some(Next::Wait { .. }))) {
            return;
        }
        if let Some((key, mut conversation)) = self.take(at.cause)
            && let Some(Next::Wait { turn, .. }) = conversation.next.take()
        {
            conversation.next = Some(Next::Retry(turn));
            self.keep(key, conversation, at.seq);
        }
    }

    #[fold]
    fn recorded(&mut self, run: Ran<SessionRecord>, at: At) {
        if let Some((key, conversation)) = self.take(at.cause) {
            self.recorded.insert(key, (run.result(), conversation.tree));
            self.links.insert(at.seq, key);
        }
    }

    #[fold]
    fn requested(&mut self, request: Requested, at: At) {
        let RequestSource::Reaction { reactor, .. } = &request.source else {
            return;
        };
        if reactor.as_str() != MuseSession::NAMESPACE {
            return;
        }
        if let Some(key) = at.cause.and_then(|cause| self.links.remove(&cause)) {
            self.links.insert(at.seq, key);
        }
    }

    /// The staged answer to a call whose every attempt was exhausted: the
    /// call's output, when the run answers the `Requested` the loop recorded
    /// for it. The `resume` rule then runs what comes next, as after a tool's
    /// own run.
    #[fold]
    fn answered_exhausted(&mut self, run: Ran<SessionExhausted>, cited: &Cited, at: At) -> Result<(), CitedError> {
        let linked = at.cause.and_then(|cause| self.sessions.get(self.links.get(&cause)?));
        let awaits_answer = linked.is_some_and(|conversation| matches!(conversation.next, Some(Next::Exhausted(_))));
        if !awaits_answer {
            return Ok(());
        }
        let refusal = cited.get(run.result())?.refusal();
        if let Some((key, mut conversation)) = self.take(at.cause) {
            let refused = conversation.refuse(refusal, at.seq);
            if refused {
                self.advance(key, conversation, at.seq);
            } else {
                conversation.answered = Some(at.seq);
                let failure = Failure::Unbuilt { reason: Detail::new("the exhausted answer answers no waiting call") };
                self.fail(key, conversation, failure, at.seq);
            }
        }
        Ok(())
    }

    /// A run the loop requested faulted. A tool run that ran out of time or
    /// memory runs again, up to [`MAX_TOOL_RETRIES`] times, and is then
    /// answered with the staged text saying so; any other fault fails the
    /// session, or, when the run was its failed record, drops it.
    #[fold]
    fn faulted(&mut self, fault: Fault, at: At) {
        let Some((key, mut conversation)) = self.take(at.cause) else {
            return;
        };
        if let Some(next) = conversation.exhausted(&fault) {
            conversation.next = Some(next);
            self.keep(key, conversation, at.seq);
        } else {
            let failure = Failure::Faulted { program: fault.program.name().clone(), reason: fault.reason };
            self.fail(key, conversation, failure, at.seq);
        }
    }

    /// One of the loop's rules produced no records: the session fails, or,
    /// when the rule made its failed record, is dropped. A failed head move
    /// finds its session already gone and only drops the link.
    #[fold]
    fn failed(&mut self, failure: ReactionFailed, at: At) {
        if failure.reactor.is_some_and(|reactor| reactor.as_str() == MuseSession::NAMESPACE)
            && let Some((key, conversation)) = self.take(at.cause)
        {
            self.fail(key, conversation, Failure::Reaction { reason: failure.reason }, at.seq);
        }
    }

    #[fold]
    fn moved_head(&mut self, event: HeadMoved<Session>, at: At) {
        let Some(key) = SessionKey::of_head(event.head()) else {
            return;
        };
        self.current.insert(key, event.to());
        if self.recorded.get(&key).is_some_and(|(record, _)| *record == event.to())
            && let Some(recorded) = self.recorded.remove(&key)
        {
            self.rested.insert(key, recorded);
        }
        if let Some(cause) = at.cause.filter(|cause| self.links.get(cause) == Some(&key)) {
            self.links.remove(&cause);
        }
    }
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Head, ProgramName, Ref, Seq, Tree};
    use aether_bloomery_program::At;

    use super::{Conversation, Conversations, Waiting};
    use crate::input::{CallId, OfferedTool, OfferedTools, ToolCall, ToolCalls};
    use crate::session::fixture::settings;
    use crate::session::state::{SessionKey, TurnLimit};
    use crate::tools::offered;

    #[test]
    fn a_call_runs_in_the_bundle_its_offer_names() {
        // Catches a loop that calls every tool in the muse bundle, or reads the head of another offer than the one
        // the call names.
        let (muse, _) = offered();
        let echo = &muse.as_slice()[0];
        let proofs = Head::new("proofs");
        let program = ProgramName::new("proof.check").expect("program");
        let foreign = OfferedTool::new(
            program.clone(),
            proofs.clone(),
            echo.definition(),
            echo.input(),
            echo.bound(),
            echo.result(),
        );
        let input = settings(OfferedTools::new(vec![echo.clone(), foreign]).expect("tools"))
            .open(Ref::of_text("rules"), Ref::of_text("hi"));

        let call =
            ToolCall::decoded(CallId::new("call-1").expect("id"), program.clone(), Ref::of_text("{}"), echo.bound());
        let calls = ToolCalls::new(vec![call]).expect("calls");
        let (turn, tree) = (Ref::of_encoded(&input).expect("turn"), Ref::of_encoded(&Tree::empty()).expect("tree"));
        let mut conversation = Conversation::new(TurnLimit::new(4).expect("limit"), turn, tree);
        let text = Ref::of_text("");
        conversation.waiting =
            Some(Waiting { input, turn, result: None, text, calls, outputs: Vec::new(), full: false });

        let mut conversations = Conversations::default();
        conversations.advance(SessionKey::new(1), conversation, Seq(2));
        let step =
            conversations.step(At { seq: Seq(2), cause: None, recorded_at_millis: 0 }).expect("a call runs next");
        assert_eq!((step.program, step.name), (proofs, program));
    }
}
