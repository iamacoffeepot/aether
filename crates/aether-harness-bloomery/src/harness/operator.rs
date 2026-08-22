//! Lowering one [`OperatorMove`] onto the [`Fact`] the reducer expects.
//!
//! A scenario states operator actions as values so a pinned file reads as the
//! run it describes — "at tick 4 the operator withdrew `issue-b`" — rather than
//! as a hand-assembled fact with a content-addressed payload in the middle of
//! it. This is the one place that translation happens, so a scenario cannot
//! admit a fact shaped differently from the one the operator door would send,
//! and a new door has exactly one place to be taught.
//!
//! Every variant lands through [`ScenarioHarness::admit`] — the same control-core
//! wire ingress the scripted seal uses. The REST doors that front these facts in
//! production add authorization and validation, not a different fact; a scenario
//! that wants to exercise a door's *refusal* drives the door itself.

use aether_bloomery::{
    BloomId, BloomSpec, Digest, Fact, OperatorHold, OperatorRepair, Outcome, Withdrawal, WithdrawalCause, WorkpieceId,
};

use super::ScenarioHarness;
use super::drive::member;
use crate::scenario::OperatorMove;

impl OperatorMove {
    /// The tick index this move is admitted at.
    #[must_use]
    pub const fn at_tick(&self) -> u32 {
        match self {
            Self::Grant { at_tick, .. }
            | Self::Hold { at_tick, .. }
            | Self::Release { at_tick, .. }
            | Self::Repair { at_tick, .. }
            | Self::Withdraw { at_tick, .. }
            | Self::Amend { at_tick, .. } => *at_tick,
        }
    }

    /// A stable idempotency key for this move.
    ///
    /// The tick is in the key because a scenario may make the same move twice —
    /// hold, release, hold again — and two admissions sharing a key are one
    /// admission with the second silently deduplicated.
    fn key(&self) -> String {
        let (verb, subject) = match self {
            Self::Grant { workpiece, .. } => ("grant", workpiece.0.clone()),
            Self::Hold { .. } => ("hold", String::new()),
            Self::Release { .. } => ("release", String::new()),
            Self::Repair { workpiece, .. } => ("repair", workpiece.0.clone()),
            Self::Withdraw { workpiece, .. } => ("withdraw", workpiece.0.clone()),
            Self::Amend { workpiece, .. } => ("amend", workpiece.0.clone()),
        };
        format!("operator-{verb}-{subject}-{}", self.at_tick())
    }
}

impl ScenarioHarness {
    /// Admit one operator move against `bloom`, and hand back what the reducer
    /// answered.
    ///
    /// The outcome is returned rather than asserted: half the moves worth
    /// scripting are the ones a scenario expects to be *refused* — a withdrawal
    /// that would strand a dependent, a grant against a member that is not
    /// wedged — and a helper that panicked on a refusal could only ever test
    /// the happy half.
    ///
    /// # Panics
    /// [`OperatorMove::Amend`] against a harness that has sealed nothing: the
    /// amendment is a supersession of the sealed spec, so there has to be one.
    pub fn apply_operator(&mut self, bloom: BloomId, action: &OperatorMove) -> Outcome {
        let key = action.key();
        let fact = match action {
            OperatorMove::Grant { workpiece, stage, attempts, .. } => {
                Fact::GrantAttempts { bloom, workpiece: workpiece.clone(), stage: *stage, attempts: *attempts }
            }
            OperatorMove::Hold { bloom: named, reason, operator, .. } => Fact::OperatorHold {
                bloom: named.unwrap_or(bloom),
                hold: OperatorHold { reason: reason.clone(), operator: operator.clone() },
            },
            OperatorMove::Release { bloom: named, reason, operator, .. } => Fact::OperatorRelease {
                bloom: named.unwrap_or(bloom),
                release: OperatorHold { reason: reason.clone(), operator: operator.clone() },
            },
            OperatorMove::Repair { workpiece, candidate, reason, operator, .. } => Fact::OperatorRepair {
                bloom,
                repair: OperatorRepair {
                    workpiece: workpiece.clone(),
                    candidate: *candidate,
                    reason: reason.clone(),
                    operator: operator.clone(),
                },
            },
            OperatorMove::Withdraw { workpiece, reason, operator, cascade, .. } => Fact::Withdraw {
                bloom,
                // Only the operator-named member. The cascade set is derived by
                // the reducer from the flag (#5327), so naming a dependent here
                // would state a decision the reducer is the one that makes.
                withdrawals: vec![Withdrawal {
                    workpiece: workpiece.clone(),
                    cause: WithdrawalCause::Operator,
                    reason: reason.clone(),
                    operator: operator.clone(),
                }],
                cascade: *cascade,
            },
            OperatorMove::Amend { workpiece, scope_revision, .. } => {
                Fact::Supersede { predecessor: bloom, successor: self.amended_spec(workpiece, *scope_revision) }
            }
        };

        self.admit(&key, fact)
    }

    /// The successor spec an amendment seals: the sealed bloom's members, with
    /// `workpiece` re-admitted at `scope_revision`.
    ///
    /// An amended surface is a *new scope revision*, and a sealed bloom's
    /// membership is frozen — so granting one is a supersession that carries
    /// every other member across at the revision it already holds (which
    /// inherits its claim and its finished work) and re-admits the amended one
    /// at the widened revision. That is what re-arms a member parked awaiting a
    /// surface: it enters the successor with a scope the containment check
    /// admits.
    ///
    /// # Panics
    /// Nothing has been sealed, or `workpiece` is not a member of what was.
    fn amended_spec(&self, workpiece: &WorkpieceId, scope_revision: Digest) -> BloomSpec {
        let sealed = self.sealed.as_ref().expect("an amendment supersedes a sealed bloom; seal one first");
        assert!(
            sealed.members().iter().any(|existing| existing.workpiece == *workpiece),
            "{workpiece:?} is not a member of the sealed bloom"
        );
        let members = sealed
            .members()
            .iter()
            .map(|existing| {
                if existing.workpiece == *workpiece {
                    member(&existing.workpiece.0, scope_revision)
                } else {
                    existing.clone()
                }
            })
            .collect::<Vec<_>>();

        super::draft(sealed.base(), &members)
    }
}
