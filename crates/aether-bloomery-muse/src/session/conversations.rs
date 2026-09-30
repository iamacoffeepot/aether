//! The live sessions, folded from the journal: which entry belongs to which
//! session, what each waiting turn still owes, and each session's latest
//! record.
//!
//! Every hop of a session is one cause lookup. An entry the loop acts on is
//! linked to its session's key; the `Requested` entry the loop's intent
//! records is caused by that entry and takes the link over, and the run it
//! requested is caused by the `Requested` entry and takes it over in turn.
//! Each link is removed when it is used, so only live hops are kept.

use std::collections::BTreeMap;

use aether_bloomery_kinds::{
    CallInput, CallProgram, Digest, EncodedArtifact, ErasedRef, Fault, HeadChange, HeadMoved, ProgramName,
    ReactionFailed, Ref, RequestSource, Requested, Seq, SetHeads, Utf8Text,
};
use aether_bloomery_program::{At, Cited, CitedError, Ran, Reactor, ViewCursor, view};

use crate::input::{ToolCalls, ToolInput, ToolOutput, TurnInput};
use crate::program::MuseTurn;
use crate::result::{TurnOutcome, TurnResult};
use crate::session::MuseSession;
use crate::session::continue_::SessionContinue;
use crate::session::open::SessionOpen;
use crate::session::record::{CallAnswer, RecordInput, SessionRecord};
use crate::session::replay::replay;
use crate::session::state::{Session, SessionKey, TurnLimit};
use crate::session::tools::{Echo, MUSE, call};

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
}

/// One session's activation in progress.
struct Conversation {
    /// The most turns this activation may make.
    limit: TurnLimit,
    /// The turns it has made.
    turns: u32,
    /// The turn whose calls are running, if one asked for calls.
    waiting: Option<Waiting>,
    /// What the loop runs next.
    next: Option<Next>,
}

/// A turn that asked for calls, and the outputs of the calls answered so far.
struct Waiting {
    input: TurnInput,
    turn: Ref<TurnInput>,
    result: Ref<TurnResult>,
    text: Ref<Utf8Text>,
    calls: ToolCalls,
    outputs: Vec<CallAnswer>,
}

/// What the loop runs after the entry linked to a session.
enum Next {
    /// A called program, over the input its call decoded to.
    Call { program: ProgramName, input: Digest },
    /// The next turn, once every call has its output.
    Turn(TurnInput),
    /// The record of a session resting at its turn limit.
    Limit(RecordInput),
    /// The record of a session whose turn rested it.
    Rest(RecordInput),
}

impl Conversation {
    const fn new(limit: TurnLimit) -> Self {
        Self { limit, turns: 0, waiting: None, next: None }
    }

    /// Answer every refused call up to the next decoded one, and set what runs
    /// next. `None` when the next turn would pass the item cap.
    fn advance(&mut self) -> Option<()> {
        let waiting = self.waiting.as_mut()?;
        let answered = waiting.outputs.len();
        for call in &waiting.calls.as_slice()[answered..] {
            match call.input() {
                ToolInput::Refused(text) => {
                    waiting.outputs.push(CallAnswer::new(call.call_id().clone(), ToolOutput::Refused(*text)));
                }
                ToolInput::Decoded(input) => {
                    self.next = Some(Next::Call { program: call.program().clone(), input: input.digest() });
                    return Some(());
                }
            }
        }
        self.next = Some(if self.turns < self.limit.get() {
            Next::Turn(waiting.input.append(replay(waiting.text, waiting.calls.as_slice(), &waiting.outputs)).ok()?)
        } else {
            Next::Limit(RecordInput::new(waiting.turn, waiting.result, waiting.outputs.clone(), None))
        });
        Some(())
    }

    /// Record the result `result` of the program `program` that ran over
    /// `input` as the output of the next call. `None` when that call did not
    /// ask for this run.
    fn answer(&mut self, program: &ProgramName, input: Digest, result: ErasedRef) -> Option<()> {
        let waiting = self.waiting.as_mut()?;
        let call = waiting.calls.as_slice().get(waiting.outputs.len())?;
        let decoded = matches!(call.input(), ToolInput::Decoded(decoded) if decoded.digest() == input);
        if !decoded || call.program() != program {
            return None;
        }
        let schema = waiting.input.tools().iter().find(|tool| tool.program() == program)?.result();
        waiting.outputs.push(CallAnswer::new(call.call_id().clone(), ToolOutput::Result { schema, result }));
        Some(())
    }
}

impl Conversations {
    /// The call the loop makes after the entry at `at`, when it follows a turn
    /// or a call that did not rest the session.
    pub fn step(&self, at: At) -> Option<CallProgram> {
        match self.next(at)? {
            Next::Call { program, input } => {
                Some(CallProgram { program: MUSE, name: program.clone(), input: CallInput::Stored(*input) })
            }
            Next::Turn(turn) => Some(call::<MuseTurn>(CallInput::Value(EncodedArtifact::new(turn).ok()?))),
            Next::Limit(record) => Some(call::<SessionRecord>(CallInput::Value(EncodedArtifact::new(record).ok()?))),
            Next::Rest(_) => None,
        }
    }

    /// The record the loop makes after the turn at `at`, when that turn rested
    /// the session.
    pub fn rest(&self, at: At) -> Option<CallProgram> {
        match self.next(at)? {
            Next::Rest(record) => Some(call::<SessionRecord>(CallInput::Value(EncodedArtifact::new(record).ok()?))),
            Next::Call { .. } | Next::Turn(_) | Next::Limit(_) => None,
        }
    }

    /// Whether the entry at `at` opened or continued a session.
    pub fn starts(&self, at: At) -> bool {
        self.links.get(&at.seq).is_some_and(|key| self.sessions.get(key).is_some_and(|session| session.next.is_none()))
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

    /// Set what runs next for `key` after the entry at `seq`, or drop the
    /// session when nothing can.
    fn advance(&mut self, key: SessionKey, mut conversation: Conversation, seq: Seq) {
        if conversation.advance().is_some() {
            self.keep(key, conversation, seq);
        }
    }

    /// Drop the link from the entry at `cause`, and the session it names.
    fn drop_linked(&mut self, cause: Option<Seq>) {
        self.take(cause);
    }
}

#[view(cursor = cursor)]
impl View for Conversations {
    #[fold]
    fn opened(&mut self, run: Ran<SessionOpen>, cited: &Cited, at: At) -> Result<(), CitedError> {
        let input = cited.get(run.input())?;
        self.keep(SessionKey::new(at.seq.0), Conversation::new(input.max_turns()), at.seq);
        Ok(())
    }

    #[fold]
    fn continued(&mut self, run: Ran<SessionContinue>, cited: &Cited, at: At) -> Result<(), CitedError> {
        let input = cited.get(run.input())?;
        let key = input.session();
        if self.current.get(&key) == Some(&input.from()) && !self.sessions.contains_key(&key) {
            self.keep(key, Conversation::new(input.max_turns()), at.seq);
        }
        Ok(())
    }

    #[fold]
    fn turned(&mut self, run: Ran<MuseTurn>, cited: &Cited, at: At) -> Result<(), CitedError> {
        let Some((key, mut conversation)) = self.take(at.cause) else {
            return Ok(());
        };
        let (input, outcome) = (cited.get(run.input())?, cited.get(run.result())?.outcome().clone());
        conversation.turns += 1;
        match outcome {
            TurnOutcome::Called { calls, text, .. } => {
                let (turn, result) = (run.input(), run.result());
                conversation.waiting = Some(Waiting { input, turn, result, text, calls, outputs: Vec::new() });
                self.advance(key, conversation, at.seq);
            }
            TurnOutcome::Completed { .. } | TurnOutcome::Declined { .. } | TurnOutcome::Incomplete { .. } => {
                conversation.next = Some(Next::Rest(RecordInput::new(run.input(), run.result(), Vec::new(), None)));
                self.keep(key, conversation, at.seq);
            }
            TurnOutcome::Rejected | TurnOutcome::Transient { .. } | TurnOutcome::Unreadable => {}
        }
        Ok(())
    }

    #[fold]
    fn echoed(&mut self, run: Ran<Echo>, at: At) {
        let Some((key, mut conversation)) = self.take(at.cause) else {
            return;
        };
        if conversation.answer(run.program().name(), run.input().digest(), run.result().erase()).is_some() {
            self.advance(key, conversation, at.seq);
        }
    }

    #[fold]
    fn recorded(&mut self, _run: Ran<SessionRecord>, at: At) {
        if let Some((key, _)) = self.take(at.cause) {
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

    #[fold]
    fn faulted(&mut self, _fault: Fault, at: At) {
        self.drop_linked(at.cause);
    }

    #[fold]
    fn failed(&mut self, failure: ReactionFailed, at: At) {
        if failure.reactor.is_some_and(|reactor| reactor.as_str() == MuseSession::NAMESPACE) {
            self.drop_linked(at.cause);
        }
    }

    #[fold]
    fn moved_head(&mut self, event: HeadMoved<Session>, at: At) {
        let Some(key) = SessionKey::of_head(event.head()) else {
            return;
        };
        self.current.insert(key, event.to());
        if let Some(cause) = at.cause.filter(|cause| self.links.get(cause) == Some(&key)) {
            self.links.remove(&cause);
        }
    }
}
