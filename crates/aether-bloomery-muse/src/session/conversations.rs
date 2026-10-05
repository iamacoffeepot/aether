//! The live sessions, folded from the journal: which entry belongs to which
//! session, what each waiting turn still owes, each session's current tree,
//! and each session's latest record with the tree it rested with.
//!
//! A `Done` end call is gated (ADR-0234 decision 11): once every call of its
//! turn has its output, the loop runs each required proof whose latest
//! passing run did not leave the current tree, adopting each proof's tree,
//! and settles only when every one is proven on it. A failed proof, or
//! proofs whose trees do not settle, answer the end call with a staged
//! refusal instead, and the turn goes on as one that did not end.
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
    CallInput, CallProgram, Detail, EncodedArtifact, Fault, Head, HeadChange, HeadMoved, ProgramName, ReactionFailed,
    RequestSource, Requested, Seq, SetHeads, Transition, Tree, Until,
};
use aether_bloomery_program::{
    At, Cited, CitedError, ClockUntil, ErasedEdited, Ran, Reactor, ViewCursor, tooled, view,
};
use aether_data::{ErasedRef, OpaqueBytes, Ref, Utf8Text};

use crate::input::{Reasoning, Role, ToolCalls, ToolInput, ToolOutput, TurnInput, TurnItem};
use crate::program::MuseTurn;
use crate::result::{TurnOutcome, TurnResult};
use crate::session::MuseSession;
use crate::session::continue_::SessionContinue;
use crate::session::exhausted::{ExhaustedInput, Exhaustion, MAX_TOOL_RETRIES, SessionExhausted};
use crate::session::gate::{GateInput, MAX_GATE_RUNS, RequiredProof, RequiredProofs, SessionGate};
use crate::session::open::SessionOpen;
use crate::session::record::{Answered, CallAnswer, RecordInput, SessionRecord};
use crate::session::replay::replay;
use crate::session::retry::{MAX_RETRIES, retry_wait, wait_call};
use crate::session::state::{Failure, Session, SessionKey, TurnLimit};
use crate::session::tools::call;
use crate::tools::{Ending, NUDGE_TEXT, end_position, ends_run, proof_passed};

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
    /// Each session's last record the head moved to, with what a continue
    /// from that record picks up.
    rested: BTreeMap<SessionKey, Rested>,
    /// Each session's latest record, with what a continue from it picks up,
    /// until the head moves to it.
    recorded: BTreeMap<SessionKey, Rested>,
}

/// A session's record and what a continue from it picks up: the tree it
/// rested with, the proofs its `Done` end must pass, and the tree each
/// proof's latest passing run left.
struct Rested {
    record: Ref<Session>,
    tree: Ref<Tree>,
    required: RequiredProofs,
    proven: BTreeMap<ProgramName, Ref<Tree>>,
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
    /// the one the last `Edited` result left, whatever its detail.
    tree: Ref<Tree>,
    /// The proofs a `Done` end must pass on [`Self::tree`].
    required: RequiredProofs,
    /// The tree each proof's latest passing run left, the model's own runs
    /// and the gate's alike: a proof proves the tree it returns.
    proven: BTreeMap<ProgramName, Ref<Tree>>,
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
    /// The reasoning items of the turn that asked for the calls, resent ahead
    /// of its text; empty for seeds.
    reasoning: Vec<Reasoning>,
    text: Ref<Utf8Text>,
    calls: ToolCalls,
    outputs: Vec<CallAnswer>,
    /// Whether the turn that asked for the calls reached its input limit, so
    /// the session rests once every call has its output. `false` for seeds,
    /// which no turn asked for.
    full: bool,
    /// Whether the turn's end call was answered with `Ending::Done`, so the
    /// turn ends only once its gate passes.
    done: bool,
    /// The runs the gate made of each required proof for the end call.
    gate_runs: BTreeMap<ProgramName, u32>,
}

/// What the loop runs after the entry linked to a session.
#[derive(Clone)]
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
    /// A required proof the gate runs from the bundle its offer names, over
    /// the session's tree, the required arguments, and the offer's bound.
    Prove { head: Head<OpaqueBytes>, program: ProgramName, input: EncodedArtifact },
    /// The answer to a `Done` end call whose gate did not pass.
    Ungated(GateInput),
}

impl Next {
    /// The program and input of the tool run this names: a call, or a
    /// required proof the gate runs.
    fn tool_run(&self) -> Option<(&ProgramName, &EncodedArtifact)> {
        match self {
            Self::Call { program, input, .. } | Self::Prove { program, input, .. } => Some((program, input)),
            _ => None,
        }
    }
}

impl Conversation {
    const fn new(limit: TurnLimit, turn: Ref<TurnInput>, tree: Ref<Tree>, required: RequiredProofs) -> Self {
        Self {
            limit,
            turns: 0,
            turn,
            retries: 0,
            tree,
            required,
            proven: BTreeMap::new(),
            waiting: None,
            answered: None,
            exhaustions: 0,
            next: None,
        }
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
    /// next: that call, the gate's next run when the turn ended `Done`, or
    /// what follows the turn. Or say why the loop cannot build it.
    fn advance(&mut self) -> Result<(), Detail> {
        let next = match self.next_call()? {
            Some(call) => call,
            None => match self.gate()? {
                Some(gate) => gate,
                None => self.after_calls()?,
            },
        };
        self.next = Some(next);
        Ok(())
    }

    /// Answer every refused call up to the next decoded one, and the call of
    /// that one; `None` once every call has its output.
    fn next_call(&mut self) -> Result<Option<Next>, Detail> {
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
                    return Ok(Some(Next::Call { head: tool.head().clone(), program: program.clone(), input }));
                }
            }
        }
        Ok(None)
    }

    /// The gate's next run, once every call has its output and the turn
    /// ended `Done`: the first required proof whose latest passing run did not
    /// leave the current tree, or the answer that the proofs did not settle
    /// once it has run [`MAX_GATE_RUNS`] times. `None` when the turn did not
    /// end `Done`, or every required proof is proven on the current tree.
    fn gate(&mut self) -> Result<Option<Next>, Detail> {
        let waiting = self.waiting.as_mut().ok_or_else(|| Detail::new("no turn waits on calls"))?;
        let ended = ends_run(waiting.calls.as_slice(), &waiting.outputs);
        let gated = ended && waiting.done;
        if !gated {
            return Ok(None);
        }

        let tree = self.tree;
        let unproven = self.required.as_slice().iter().find(|proof| self.proven.get(proof.program()) != Some(&tree));
        let Some(proof) = unproven else {
            return Ok(None);
        };
        let program = proof.program();
        let runs = waiting.gate_runs.get(program).copied().unwrap_or_default();
        let capped = runs >= MAX_GATE_RUNS;
        if capped {
            let ran = self.required.as_slice().iter().map(RequiredProof::program);
            let proofs = ran.filter(|program| waiting.gate_runs.contains_key(*program)).cloned().collect();
            return Ok(Some(Next::Ungated(GateInput::unsettled(proofs))));
        }
        waiting.gate_runs.insert(program.clone(), runs + 1);

        let tool = waiting
            .input
            .tools()
            .iter()
            .find(|tool| tool.program() == program)
            .ok_or_else(|| Detail::new(format!("{} is required but not an offered tool", program.as_str())))?;
        let input = EncodedArtifact::new(&tooled(tree, proof.args(), tool.bound()))
            .map_err(|error| Detail::new(format!("a required proof's input did not encode: {error}")))?;
        Ok(Some(Next::Prove { head: tool.head().clone(), program: program.clone(), input }))
    }

    /// What follows a turn whose every call has its output and whose gate,
    /// if any, passed: its record when it ended or reached a limit, and the
    /// next turn otherwise.
    fn after_calls(&self) -> Result<Next, Detail> {
        let waiting = self.waiting.as_ref().ok_or_else(|| Detail::new("no turn waits on calls"))?;
        let ended = ends_run(waiting.calls.as_slice(), &waiting.outputs);
        let spent = self.turns >= self.limit.get();
        let settles = ended || waiting.full || spent;
        Ok(match waiting.result {
            Some(result) if settles => {
                Next::Settled(RecordInput::rested(waiting.turn, result, waiting.outputs.clone(), self.tree))
            }
            _ => {
                let replayed = replay(&waiting.reasoning, waiting.text, waiting.calls.as_slice(), &waiting.outputs);
                Next::Turn(
                    waiting
                        .input
                        .append(replayed)
                        .map_err(|error| Detail::new(format!("the next turn's items: {error}")))?,
                )
            }
        })
    }

    /// Whether `run` is the run of the call or the required proof the loop
    /// runs next.
    fn awaits(&self, run: &Transition) -> bool {
        let Some((program, input)) = self.next.as_ref().and_then(Next::tool_run) else {
            return false;
        };
        let same_program = program == run.program.name();
        let same_input = input.digest() == run.input;
        same_program && same_input
    }

    /// Record `result`, the result of the tool run at `seq`, as the output of
    /// the call the loop ran, citing the result schema its turn offered, and
    /// mark the turn `done` when that call is its end call and `ends_done`.
    /// `None` when the call does not run or the turn offered no such tool.
    fn answer(&mut self, result: ErasedRef, ends_done: bool, seq: Seq) -> Option<()> {
        let waiting = self.waiting.as_mut()?;
        let position = waiting.outputs.len();
        let call = waiting.calls.as_slice().get(position)?;
        let program = call.program()?;
        let schema = waiting.input.tools().iter().find(|tool| tool.program() == program)?.result();
        let is_end = end_position(waiting.calls.as_slice()) == Some(position);
        waiting.outputs.push(CallAnswer::new(call.call_id().clone(), ToolOutput::Result { schema, result }));
        waiting.done |= is_end && ends_done;
        self.answered = Some(seq);
        self.exhaustions = 0;
        Some(())
    }

    /// Take `edited`, the result of `program`'s run, as the session's tree,
    /// and as the tree `program` proves when the result says it passed.
    fn adopt(&mut self, program: &ProgramName, edited: &ErasedEdited) {
        self.tree = edited.tree();
        let passed = proof_passed(edited.detail());
        if passed {
            self.proven.insert(program.clone(), edited.tree());
        }
    }

    /// Answer the call now running with `refusal`, the staged answer to its
    /// exhausted attempts, at `seq`; whether a call waited for an answer.
    /// When the run was the gate's required proof, every call already has
    /// its output, and the end call is answered with `refusal` instead.
    fn refuse(&mut self, refusal: Ref<Utf8Text>, seq: Seq) -> bool {
        let Some(waiting) = self.waiting.as_mut() else {
            return false;
        };
        match waiting.calls.as_slice().get(waiting.outputs.len()) {
            Some(call) => waiting.outputs.push(CallAnswer::new(call.call_id().clone(), ToolOutput::Refused(refusal))),
            None if waiting.done => return self.refuse_end(refusal, seq),
            None => return false,
        }
        self.answered = Some(seq);
        self.exhaustions = 0;
        true
    }

    /// Answer the turn's end call with `refusal` in place of its `Done`, at
    /// `seq`, so the turn goes on as one that did not end; whether the turn
    /// has an answered end call.
    fn refuse_end(&mut self, refusal: Ref<Utf8Text>, seq: Seq) -> bool {
        let Some(waiting) = self.waiting.as_mut() else {
            return false;
        };
        let Some(answer) =
            end_position(waiting.calls.as_slice()).and_then(|position| waiting.outputs.get_mut(position))
        else {
            return false;
        };
        *answer = CallAnswer::new(answer.call_id().clone(), ToolOutput::Refused(refusal));
        waiting.done = false;
        self.answered = Some(seq);
        self.exhaustions = 0;
        true
    }

    /// What runs after `fault` of the call now running, when it ran out of
    /// time or memory: the same run again below the retry cap, and the
    /// staged answer to its attempts at the cap. `None` for any other fault.
    fn exhausted(&mut self, fault: &Fault) -> Option<Next> {
        let resource = Exhaustion::of(&fault.reason)?;
        let (program, input) = self.next.as_ref().and_then(Next::tool_run)?;
        let same_program = program == fault.program.name();
        let same_input = input.digest() == fault.input;
        let answers_the_call = same_program && same_input;
        if !answers_the_call {
            return None;
        }
        let program = program.clone();
        self.exhaustions += 1;
        if self.exhaustions <= MAX_TOOL_RETRIES {
            return self.next.clone();
        }
        Some(Next::Exhausted(ExhaustedInput::new(program, resource, self.exhaustions)))
    }
}

impl Conversations {
    /// The call the loop makes after the entry at `at`, when it follows a turn
    /// or a call that did not rest the session.
    pub fn step(&self, at: At) -> Option<CallProgram> {
        match self.next(at)? {
            Next::Call { head, program, input } | Next::Prove { head, program, input } => Some(CallProgram {
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
            Next::Ungated(gate) => Some(call::<SessionGate>(CallInput::Value(EncodedArtifact::new(gate).ok()?))),
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
            | Next::Exhausted(_)
            | Next::Prove { .. }
            | Next::Ungated(_) => None,
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

    /// The gate's run of the required proof `program` at `seq` answered
    /// `result`: its tree becomes the session's, pass or fail. A pass gates
    /// on, and a failure answers the end call through `muse.session.gate`. A
    /// result that is no `Edited` fails the session.
    fn proved(
        &mut self,
        key: SessionKey,
        mut conversation: Conversation,
        program: &ProgramName,
        result: ErasedRef,
        edited: Option<&ErasedEdited>,
        seq: Seq,
    ) {
        conversation.answered = Some(seq);
        conversation.exhaustions = 0;
        let (Some(edited), Some(cited)) = (edited, result.cast::<ErasedEdited>()) else {
            let failure = Failure::Unbuilt { reason: Detail::new("a required proof's result is not an edited tree") };
            self.fail(key, conversation, failure, seq);
            return;
        };
        conversation.adopt(program, edited);
        let passed = proof_passed(edited.detail());
        if passed {
            self.advance(key, conversation, seq);
        } else {
            let gate = GateInput::failed(program.clone(), cited);
            conversation.next = Some(Next::Ungated(gate));
            self.keep(key, conversation, seq);
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
        let mut conversation = Conversation::new(input.max_turns(), turn, input.tree(), input.required().clone());
        match opened.seeds() {
            None => self.keep(key, conversation, at.seq),
            Some(seeds) => {
                let (calls, text, first) = (seeds.clone(), Ref::of_text(""), input.first_turn());
                let waiting = Waiting {
                    input: first,
                    turn,
                    result: None,
                    reasoning: Vec::new(),
                    text,
                    calls,
                    outputs: Vec::new(),
                    full: false,
                    done: false,
                    gate_runs: BTreeMap::new(),
                };
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
        let rested = self.rested.get(&key).filter(|rested| rested.record == input.from());
        if let Some(rested) = rested
            && self.current.get(&key) == Some(&input.from())
            && !self.sessions.contains_key(&key)
        {
            let required = rested.required.clone();
            let mut conversation = Conversation::new(input.max_turns(), run.result(), rested.tree, required);
            conversation.proven.clone_from(&rested.proven);
            self.keep(key, conversation, at.seq);
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
            TurnOutcome::Called { reasoning, calls, text, usage } => {
                conversation.turns += 1;
                conversation.retries = 0;
                let (turn, result) = (run.input(), Some(run.result()));
                let full = input.input_limit().reached(usage.input_tokens());
                conversation.waiting = Some(Waiting {
                    input,
                    turn,
                    result,
                    reasoning,
                    text,
                    calls,
                    outputs: Vec::new(),
                    full,
                    done: false,
                    gate_runs: BTreeMap::new(),
                });
                self.advance(key, conversation, at.seq);
            }
            TurnOutcome::Completed { reasoning, text, usage } => {
                conversation.turns += 1;
                conversation.retries = 0;
                let spent = conversation.turns >= conversation.limit.get();
                let full = input.input_limit().reached(usage.input_tokens());
                let nudged = reasoning.into_iter().map(TurnItem::Reasoning).chain([
                    TurnItem::message(Role::Assistant, text),
                    TurnItem::message(Role::User, Ref::of_text(NUDGE_TEXT)),
                ]);
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

    /// Any program's run: the output of a tool call, or the gate's run of a
    /// required proof, when the run answers the `Requested` the loop recorded
    /// for what it runs next. A result that is an `Edited`, of any detail,
    /// moves the session to its tree before the next call: the loop reads it
    /// as [`ErasedEdited`], so it links no tool's detail kind, and a passing
    /// proof's result proves that tree. An end call answered `Done` gates its
    /// turn.
    /// Every other run, `muse.turn` and the session programs included, is
    /// left to its own fold.
    #[fold]
    fn ran(&mut self, run: Transition, cited: &Cited, at: At) -> Result<(), CitedError> {
        let linked = at.cause.and_then(|cause| self.sessions.get(self.links.get(&cause)?));
        if !linked.is_some_and(|conversation| conversation.awaits(&run)) {
            return Ok(());
        }
        let result = ErasedRef::new(cited.kind(run.result)?, run.result);
        let edited = result.cast::<ErasedEdited>().map(|edited| cited.get(edited)).transpose()?;
        let ending = result.cast::<Ending>().map(|ending| cited.get(ending)).transpose()?;
        let ends_done = matches!(ending, Some(Ending::Done { .. }));
        let Some((key, mut conversation)) = self.take(at.cause) else {
            return Ok(());
        };

        let program = run.program.name().clone();
        let proving = matches!(conversation.next, Some(Next::Prove { .. }));
        if proving {
            self.proved(key, conversation, &program, result, edited.as_ref(), at.seq);
            return Ok(());
        }
        let answered = conversation.answer(result, ends_done, at.seq).is_some();
        if answered {
            if let Some(edited) = &edited {
                conversation.adopt(&program, edited);
            }
            self.advance(key, conversation, at.seq);
        } else {
            conversation.answered = Some(at.seq);
            let failure = Failure::Unbuilt { reason: Detail::new("the run's result answers no offered call") };
            self.fail(key, conversation, failure, at.seq);
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
            let Conversation { tree, required, proven, .. } = conversation;
            self.recorded.insert(key, Rested { record: run.result(), tree, required, proven });
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

    /// The staged answer to a `Done` end call whose gate did not pass: it
    /// replaces the end call's output, when the run answers the `Requested`
    /// the loop recorded for it, so the turn goes on as one that did not end.
    /// The `resume` rule then sends the next turn, or records the session at
    /// a limit: the failed gate spent its turn.
    #[fold]
    fn ungated(&mut self, run: Ran<SessionGate>, cited: &Cited, at: At) -> Result<(), CitedError> {
        let linked = at.cause.and_then(|cause| self.sessions.get(self.links.get(&cause)?));
        let awaits_answer = linked.is_some_and(|conversation| matches!(conversation.next, Some(Next::Ungated(_))));
        if !awaits_answer {
            return Ok(());
        }
        let refusal = cited.get(run.result())?.refusal();
        if let Some((key, mut conversation)) = self.take(at.cause) {
            let refused = conversation.refuse_end(refusal, at.seq);
            if refused {
                self.advance(key, conversation, at.seq);
            } else {
                conversation.answered = Some(at.seq);
                let failure = Failure::Unbuilt { reason: Detail::new("the gate's answer answers no end call") };
                self.fail(key, conversation, failure, at.seq);
            }
        }
        Ok(())
    }

    /// A run the loop requested faulted. A tool run, or a required proof the
    /// gate runs, that ran out of time or memory runs again, up to
    /// [`MAX_TOOL_RETRIES`] times, and is then answered with the staged text
    /// saying so (a gate's proof answers the end call); any other fault fails
    /// the session, or, when the run was its failed record, drops it.
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
        if self.recorded.get(&key).is_some_and(|rested| rested.record == event.to())
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
    use aether_bloomery_kinds::{Head, ProgramName, Seq, Tree};
    use aether_bloomery_program::At;
    use aether_data::Ref;

    use std::collections::BTreeMap;

    use super::{Conversation, Conversations, Waiting};
    use crate::input::{CallId, OfferedTool, OfferedTools, ToolCall, ToolCalls};
    use crate::session::fixture::settings;
    use crate::session::gate::RequiredProofs;
    use crate::session::state::{SessionKey, TurnLimit};
    use crate::tools::offered;

    #[test]
    fn a_call_runs_in_the_bundle_its_offer_names() {
        // Catches a loop that calls every tool in the muse bundle, or reads the head of another offer than the one
        // the call names.
        let tree = Ref::of_encoded(&Tree::empty()).expect("tree");
        let (muse, _) = offered(tree);
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
        let mut conversation =
            Conversation::new(TurnLimit::new(4).expect("limit"), turn, tree, RequiredProofs::default());
        let text = Ref::of_text("");
        conversation.waiting = Some(Waiting {
            input,
            turn,
            result: None,
            reasoning: Vec::new(),
            text,
            calls,
            outputs: Vec::new(),
            full: false,
            done: false,
            gate_runs: BTreeMap::new(),
        });

        let mut conversations = Conversations::default();
        conversations.advance(SessionKey::new(1), conversation, Seq(2));
        let step =
            conversations.step(At { seq: Seq(2), cause: None, recorded_at_millis: 0 }).expect("a call runs next");
        assert_eq!((step.program, step.name), (proofs, program));
    }
}
