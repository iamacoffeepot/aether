//! A Muse session: a reactor loop over its turns and tool calls.
//!
//! A session starts only with a `muse.session.open` run and resumes only with
//! a `muse.session.continue` run whose `from` is the session's latest record;
//! a bare `muse.turn` run is never a session. After that, recorded entries
//! decide everything, and the loop's rules only pass references: every new
//! artifact comes out of a program.
//!
//! - `open_turn` and `continue_turn` run `muse.turn` over the open's or the
//!   continue's result.
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
//! - `record` records a session a turn rested (completed, declined, or
//!   incomplete) as a [`Session`] through `muse.session.record`. `call` and
//!   `resume` record one the same way when the activation has made as many
//!   turns as its limit allows, resting it with [`RestReason::TurnLimit`].
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
//! Calls run one at a time. A tool run that faults ends the session, and the
//! fault is its record. A turn refused past the retry cap, or a wait the
//! clock refuses, ends the session the same way.

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

use aether_bloomery_kinds::{CallInput, CallProgram, SetHeads, Transition};
use aether_bloomery_program::{At, ClockUntil, Guard, Ran, reactor};

pub use continue_::{ContinueInput, SessionContinue};
use conversations::Conversations;
pub use open::{OpenInput, SessionOpen};
pub use record::{CallAnswer, RecordInput, SessionRecord};
pub use state::{
    RestReason, Session, SessionItems, SessionItemsError, SessionKey, TurnLimit, TurnLimitError, TurnSettings,
};
pub use tools::MUSE;
pub use tools::program_name;

use crate::program::MuseTurn;
use tools::call;

/// The loop's next call after a turn or a call that keeps the session going.
struct Step(CallProgram);

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

/// The record of a session the triggering turn rested.
struct Rest(CallProgram);

impl Guard<Ran<MuseTurn>> for Rest {
    type Views = Conversations;

    fn resolve(_run: &Ran<MuseTurn>, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.rest(at).map(Self)
    }
}

/// The triggering run opened a session, or continued one from its latest
/// record while no activation was in progress.
#[derive(Clone, Copy)]
struct Started;

impl Guard<Ran<SessionOpen>> for Started {
    type Views = Conversations;

    fn resolve(_run: &Ran<SessionOpen>, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.starts(at).then_some(Self)
    }
}

impl Guard<Ran<SessionContinue>> for Started {
    type Views = Conversations;

    fn resolve(_run: &Ran<SessionContinue>, at: At, conversations: &Conversations) -> Option<Self> {
        conversations.starts(at).then_some(Self)
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
    fn open_turn(&self, run: Ran<SessionOpen>, _started: Started) -> CallProgram {
        call::<MuseTurn>(CallInput::Stored(run.result().digest()))
    }

    #[rule]
    fn continue_turn(&self, run: Ran<SessionContinue>, _started: Started) -> CallProgram {
        call::<MuseTurn>(CallInput::Stored(run.result().digest()))
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
    fn rest(&self, _run: Ran<SessionRecord>, moved: Moved) -> SetHeads {
        moved.0
    }
}
