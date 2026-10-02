//! A Muse session: a reactor loop over its turns and tool calls.
//!
//! A session starts only with a `muse.session.open` run and resumes only with
//! a `muse.session.continue` run whose `from` is the session's latest record;
//! a bare `muse.turn` run is never a session. After that, recorded entries
//! decide everything, and the loop's rules only pass references: every new
//! artifact comes out of a program.
//!
//! - `open_turn` and `continue_turn` run `muse.turn` over the open's
//!   first turn, which sends the instructions as the leading developer
//!   message, or the continue's result.
//! - `seed` runs the first of an open's seeded reads instead, when it has
//!   any: each seed is a `tree.read` call `muse.session.open` built from a
//!   path, and the loop runs the seeds through the same path as a turn's
//!   calls (`resume` runs the next, then sends the first turn with the
//!   instructions, the user message, every seed's call, and every seed's
//!   output). Seeds are not a turn and count against no limit; a seed that
//!   faults fails the session, whose record holds the user message alone.
//! - `call` runs the first call a turn asked for over the session's current
//!   tree and the arguments `muse.turn` decoded for it. A call whose
//!   arguments did not decode is answered with the stored refusal and
//!   skipped.
//! - `resume`, on any program's run that answers the call the loop ran,
//!   records its result as the call's output, takes the tree of a result
//!   that is an `Edited` as the session's current tree, and runs the next
//!   call, or, when every call has its output, sends `muse.turn` again with
//!   the previous input's items plus the turn's text, its calls, and their
//!   outputs, so each turn's conversation begins with exactly what the
//!   previous one sent.
//! - `record` records a session a turn rested as a [`Session`] through
//!   `muse.session.record`. A run ends only through a `muse.end` call, which
//!   rests it with [`RestReason::Completed`], [`RestReason::Blocked`], or
//!   [`RestReason::Asked`] by the end call's variant, appending the summary,
//!   reason, or question as the final assistant message. A reply without
//!   calls never rests the session below a limit: the loop sends another turn
//!   with the reply text and a nudge, which counts against the turn limit. A
//!   reply without calls at the turn limit, or past the input limit, is
//!   recorded like a called turn there and rests with [`RestReason::TurnLimit`]
//!   or [`RestReason::ContextFull`]. A turn that stopped early with no output
//!   adds nothing, so the session ends on what that turn sent. `call` and
//!   `resume` record one the same way when the activation has made as many
//!   turns as its limit allows, resting it with [`RestReason::TurnLimit`], or
//!   when a called turn reached the session's input limit, resting it with
//!   [`RestReason::ContextFull`]. `ContextFull` takes precedence over
//!   `TurnLimit` when both hold; a called turn still runs its calls to
//!   completion before resting. A refused or truncated reply rests with
//!   [`RestReason::Declined`] or [`RestReason::Incomplete`].
//! - `rest` moves the session's head to that record, compare-and-swap from the
//!   record before it.
//! - `call` also waits on the driver's clock after a turn the vendor refused
//!   as transient: `Retry-After` seconds after the refused turn was recorded,
//!   or a doubling backoff when it sent none. `retry`, when the wait fires,
//!   sends `muse.turn` again over the same stored turn input, so the resent
//!   request is byte-identical. A turn is retried at most three times, and a
//!   retry does not count against the turn limit.
//!
//! A session opens on a tree, and a continue picks up the tree its record
//! rested with. Every record holds the session's latest tree.
//!
//! Calls run one at a time.
//!
//! Every activation ends with one head move. A session that fails rests with
//! [`RestReason::Failed`], recorded through `muse.session.record` like any
//! other rest and moved by `rest`: `rest_faulted` records it when a run the
//! loop requested faults (a tool, `muse.turn`, the clock's wait, or a record),
//! and `rest_failed` when one of the loop's own rules fails. `call` and
//! `resume` record it when the vendor ends a turn (rejected, unreadable, or
//! transient past the retry cap) or the loop cannot build its next request.
//! The record keeps the conversation the last turn sent, followed by that
//! turn's text, the calls answered before the failure, and their outputs, and
//! the session's latest tree. A continue from it works like one from any other
//! rest.
//!
//! The loop never retries a turn by itself. A continue with no user message
//! resends the conversation as it stands, optionally with a new output budget
//! that the session keeps from that turn on. Only a session that ends on what
//! its last turn sent can be resent: one that rested failed, or incomplete
//! with no output. An asked session resumes with its context once answered: a
//! continue carries the answer as its user message. Retrying a failed turn, or
//! one that ran out of budget, is the caller's continue.
//!
//! One case cannot record itself: a failure of the failed rest's own record
//! (its `muse.session.record` run faults, or the rule emitting it fails), or a
//! refused head move of any rest. The session is dropped after that one
//! attempt, so nothing loops; the driver's `Fault` or `ReactionFailed` entry
//! is the only mark, and the session keeps its previous head.

mod continue_;
mod conversations;
#[cfg(test)]
pub mod fixture;
mod open;
mod record;
mod replay;
mod retry;
mod state;
mod tools;

use aether_bloomery_kinds::{CallInput, CallProgram, Fault, ReactionFailed, Ref, SetHeads, Transition};
use aether_bloomery_program::{At, ClockUntil, Guard, Ran, reactor};

pub use continue_::{ContinueInput, SessionContinue};
use conversations::Conversations;
pub use open::{OpenInput, Opened, SessionOpen};
pub use record::{Answered, CallAnswer, RecordInput, SessionRecord, TurnEnd};
pub use state::{
    Failure, RestReason, Session, SessionItems, SessionItemsError, SessionKey, TurnLimit, TurnLimitError, TurnSettings,
};
pub use tools::MUSE;
pub use tools::program_name;

use crate::input::TurnInput;
use crate::program::MuseTurn;
use tools::call;

/// The loop's next call after a turn or a call that keeps the session going,
/// or the record of a session that failed.
struct Step(CallProgram);

impl Guard<Ran<SessionOpen>> for Step {
    type Views = Conversations;

    fn resolve(_run: &Ran<SessionOpen>, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.step(at).map(Self)
    }
}

impl Guard<Ran<MuseTurn>> for Step {
    type Views = Conversations;

    fn resolve(_run: &Ran<MuseTurn>, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.step(at).map(Self)
    }
}

impl Guard<Transition> for Step {
    type Views = Conversations;

    fn resolve(_run: &Transition, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.resume(at).map(Self)
    }
}

impl Guard<Ran<ClockUntil>> for Step {
    type Views = Conversations;

    fn resolve(_run: &Ran<ClockUntil>, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.step(at).map(Self)
    }
}

impl Guard<Fault> for Step {
    type Views = Conversations;

    fn resolve(_fault: &Fault, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.step(at).map(Self)
    }
}

impl Guard<ReactionFailed> for Step {
    type Views = Conversations;

    fn resolve(_failure: &ReactionFailed, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.step(at).map(Self)
    }
}

/// The record of a session the triggering turn rested.
struct Rest(CallProgram);

impl Guard<Ran<MuseTurn>> for Rest {
    type Views = Conversations;

    fn resolve(_run: &Ran<MuseTurn>, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.rest(at).map(Self)
    }
}

/// The triggering run opened a session with no seeded reads, or continued
/// one from its latest record while no activation was in progress: the turn
/// it starts on.
#[derive(Clone, Copy)]
struct Started(Ref<TurnInput>);

impl Guard<Ran<SessionOpen>> for Started {
    type Views = Conversations;

    fn resolve(_run: &Ran<SessionOpen>, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.starts(at).map(Self)
    }
}

impl Guard<Ran<SessionContinue>> for Started {
    type Views = Conversations;

    fn resolve(_run: &Ran<SessionContinue>, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.starts(at).map(Self)
    }
}

/// The move of the recorded session's head to the triggering record.
struct Moved(SetHeads);

impl Guard<Ran<SessionRecord>> for Moved {
    type Views = Conversations;

    fn resolve(run: &Ran<SessionRecord>, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.moved(at, run.result()).map(Self)
    }
}

/// The Muse session loop.
pub struct MuseSession;

#[reactor]
impl Reactor for MuseSession {
    const NAMESPACE: &'static str = "muse.session";

    #[rule]
    fn open_turn(&self, _run: Ran<SessionOpen>, started: Started) -> CallProgram {
        call::<MuseTurn>(CallInput::Stored(started.0.digest()))
    }

    #[rule]
    fn seed(&self, _run: Ran<SessionOpen>, step: Step) -> CallProgram {
        step.0
    }

    #[rule]
    fn continue_turn(&self, _run: Ran<SessionContinue>, started: Started) -> CallProgram {
        call::<MuseTurn>(CallInput::Stored(started.0.digest()))
    }

    #[rule]
    fn call(&self, _run: Ran<MuseTurn>, step: Step) -> CallProgram {
        step.0
    }

    #[rule]
    fn record(&self, _run: Ran<MuseTurn>, rest: Rest) -> CallProgram {
        rest.0
    }

    #[rule]
    fn resume(&self, _run: Transition, step: Step) -> CallProgram {
        step.0
    }

    #[rule]
    fn retry(&self, _run: Ran<ClockUntil>, step: Step) -> CallProgram {
        step.0
    }

    #[rule]
    fn rest_faulted(&self, _fault: Fault, step: Step) -> CallProgram {
        step.0
    }

    #[rule]
    fn rest_failed(&self, _failure: ReactionFailed, step: Step) -> CallProgram {
        step.0
    }

    #[rule]
    fn rest(&self, _run: Ran<SessionRecord>, moved: Moved) -> SetHeads {
        moved.0
    }
}
