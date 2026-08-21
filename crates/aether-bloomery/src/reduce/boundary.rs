//! The reducer's operator-visible decision boundaries (ADR-0206).
//!
//! [`gate::Gate`](super::gate::Gate) is the general shape: named guards, and a
//! decision that cannot be produced without one. Inside the reducer a refusal
//! has a second audience the general shape does not model — the record. So a
//! boundary here emits [`Decision::RecordRefusal`] beside whatever it already
//! answered with, and `/why` reads that row back rather than re-deriving the
//! guard on demand.
//!
//! Two shapes, because two things can be refused:
//!
//! [`EventBoundary`] refuses a whole admitted event ([`super::land`],
//! [`super::integrate`]'s resolve). Its guards carry the typed
//! [`Outcome`] the admitter has always matched on, declared on
//! the guard rather than recovered from the guard's name at the call site — a
//! `match` over guard strings would be the second description of the guard set
//! ADR-0206 exists to prevent, and it would keep compiling after a rename.
//!
//! [`EffectBoundary`] refuses *inside* a larger decision set — one member's
//! entry while its siblings dispatch, one aggregate work order withheld by the
//! operator brake. There is no event to answer, so the refusal's only audience
//! is the record.
//!
//! Neither builder lets a caller reach the effects without going through the
//! guards, which is the property ADR-0206 asks for: a decision that skips its
//! justification does not compile.

use alloc::vec::Vec;

use super::gate::{Read, Refusal};
use super::{Decision, Decisions, Outcome};
use crate::ids::{BloomId, WorkpieceId};

/// One boundary whose refusal is the whole event's answer.
pub(super) struct EventBoundary {
    gate: &'static str,
    bloom: BloomId,
    stopped: Option<(Refusal, Outcome)>,
}

impl EventBoundary {
    /// Start a named boundary over `bloom`.
    pub(super) fn new(gate: &'static str, bloom: BloomId) -> Self {
        Self { gate, bloom, stopped: None }
    }

    /// Require `guard` to hold, naming both the values it consulted and the
    /// typed rejection the admitter gets when it does not.
    ///
    /// A later guard is not evaluated once an earlier one has failed, so a
    /// passing path never formats a value it will not print.
    #[must_use]
    pub(super) fn require(
        mut self,
        guard: &'static str,
        holds: impl FnOnce() -> bool,
        reads: impl FnOnce() -> Vec<Read>,
        rejection: impl FnOnce() -> Outcome,
    ) -> Self {
        if self.stopped.is_some() || holds() {
            return self;
        }
        self.stopped = Some((Refusal { gate: self.gate, guard, reads: reads() }, rejection()));
        self
    }

    /// Produce the boundary's decisions: `decided` when every guard held, and
    /// otherwise the failing guard's typed rejection carrying the row that
    /// records why.
    pub(super) fn decide(self, decided: impl FnOnce() -> Decisions) -> Decisions {
        match self.stopped {
            None => decided(),
            Some((refusal, outcome)) => Decisions {
                outcome,
                effects: alloc::vec![Decision::RecordRefusal {
                    bloom: self.bloom,
                    workpiece: None,
                    refusal: refusal.recorded(),
                }],
            },
        }
    }
}

/// One boundary that refuses inside a larger decision set.
pub(super) struct EffectBoundary {
    gate: &'static str,
    bloom: BloomId,
    workpiece: Option<WorkpieceId>,
    stopped: Option<Refusal>,
}

impl EffectBoundary {
    /// Start a named boundary over `bloom`, optionally about one member.
    pub(super) fn new(gate: &'static str, bloom: BloomId, workpiece: Option<WorkpieceId>) -> Self {
        Self { gate, bloom, workpiece, stopped: None }
    }

    /// Require `guard` to hold, naming the values it consulted when it does not.
    #[must_use]
    pub(super) fn require(
        mut self,
        guard: &'static str,
        holds: impl FnOnce() -> bool,
        reads: impl FnOnce() -> Vec<Read>,
    ) -> Self {
        if self.stopped.is_some() || holds() {
            return self;
        }
        self.stopped = Some(Refusal { gate: self.gate, guard, reads: reads() });
        self
    }

    /// Produce the boundary's effects: `decided` when every guard held, and
    /// otherwise the row that records why.
    pub(super) fn effects(self, decided: impl FnOnce() -> Vec<Decision>) -> Vec<Decision> {
        self.effects_or(Vec::new, decided)
    }

    /// [`effects`](Self::effects), plus the effects a refusal itself owes.
    ///
    /// The operator brake is the case: withholding an aggregate work order
    /// still records [`Decision::DeferAggregate`], because a release has to
    /// know which orders it owes. The recording row is appended after them, so
    /// the withheld intent and the reason it was withheld ride one decision
    /// set.
    pub(super) fn effects_or(
        self,
        refused: impl FnOnce() -> Vec<Decision>,
        decided: impl FnOnce() -> Vec<Decision>,
    ) -> Vec<Decision> {
        let Some(refusal) = self.stopped else {
            return decided();
        };
        let mut effects = refused();
        effects.push(Decision::RecordRefusal {
            bloom: self.bloom,
            workpiece: self.workpiece,
            refusal: refusal.recorded(),
        });
        effects
    }
}
