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
    BloomId, BloomSpec, Digest, Fact, KeyId, OperatorHold, OperatorProposal, OperatorRepair, Outcome, Withdrawal,
    WithdrawalCause, WorkpieceId, digest_of, signed_proposal,
};
use aether_chassis_bloomery::bloomery::verified_statement_approval;

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
            | Self::Amend { at_tick, .. }
            | Self::Propose { at_tick, .. } => *at_tick,
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
            Self::Propose { .. } => ("propose", String::new()),
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
                return self.admit_amendment(bloom, workpiece, *scope_revision).1;
            }
            OperatorMove::Propose { candidate, reason, operator, .. } => {
                let proposal =
                    OperatorProposal { candidate: *candidate, reason: reason.clone(), operator: operator.clone() };
                let digest = digest_of(&proposal);
                let statement =
                    signed_proposal(KeyId(String::from("operator")), &super::scenario::OPERATOR_SEED, digest);
                Fact::ProposeChange { proposal, authorization: verified_statement_approval(digest, &statement) }
            }
        };

        self.admit(&key, fact)
    }

    /// Answer a parked surface request the way an operator does (ADR-0207):
    /// widen `workpiece`'s current revision by `added`, store the widened
    /// successor and the operator's approval of it, and supersede `bloom` with
    /// the member re-pinned there. Returns the successor bloom, which is where
    /// the re-armed member walks.
    ///
    /// The three writes `cargo xtask bloom amend` performs, in its order and
    /// through the same store rows and the same [`Fact::Supersede`]: the
    /// coordinator never widens a surface on its own, so every scenario about a
    /// widening drives it from here. The tier ladder the command applies to the
    /// delta is the operator's own preflight and has no counterpart in the
    /// store, so nothing here stands in for it.
    ///
    /// The work orders are rendered before the fact is admitted, exactly as the
    /// supersede door renders them: the successor is a new bloom id, so its
    /// members' dispatch-description rows are its own, and the amended member's
    /// row is what carries the widened surface to the lane.
    ///
    /// # Panics
    /// Nothing has been sealed, `workpiece` is not a member of what was, or the
    /// commission store holds no revision at the member's pin.
    pub fn amend_surface(&mut self, bloom: BloomId, workpiece: &str, added: &[&str]) -> BloomId {
        let workpiece = WorkpieceId(workpiece.to_owned());
        let sealed = self.sealed.as_ref().expect("an amendment supersedes a sealed bloom; seal one first");
        let pinned = sealed
            .members()
            .iter()
            .find(|member| member.workpiece == workpiece)
            .unwrap_or_else(|| panic!("{workpiece:?} is not a member of the sealed bloom"))
            .scope_revision;
        let current = self
            .scope_revision(pinned)
            .unwrap_or_else(|| panic!("{workpiece:?} is pinned at a revision the commission store does not hold"));

        let widened = current.with_widened_surface(&added.iter().map(|glob| (*glob).to_owned()).collect::<Vec<_>>());
        let revision = self.approve_widened_revision(&widened);
        assert_eq!(revision, digest_of(&widened), "the store addresses the successor by its own content");

        let (successor, outcome) = self.admit_amendment(bloom, &workpiece, revision);
        assert!(matches!(outcome, Outcome::Superseded { .. }), "the amendment supersedes: {outcome:?}");
        successor
    }

    /// Render the successor's work orders, admit the supersession, and adopt it
    /// as the sealed spec.
    ///
    /// The work orders are rendered before the fact is admitted, exactly as the
    /// supersede door renders them: the successor is a new bloom id, so its
    /// members' dispatch-description rows are its own, and the amended member's
    /// row is what carries the widened surface to the lane. Adopting the
    /// successor as `sealed` is what a second amendment in the same scenario
    /// derives membership from.
    fn admit_amendment(
        &mut self,
        predecessor: BloomId,
        workpiece: &WorkpieceId,
        scope_revision: Digest,
    ) -> (BloomId, Outcome) {
        let spec = self.amended_spec(workpiece, scope_revision);
        let successor = spec.id();

        self.persist_work_orders(&spec);
        let outcome = self.admit(
            &format!("operator-amend-{}-{}", workpiece.0, scope_revision.to_hex()),
            Fact::Supersede { predecessor, successor: spec.clone() },
        );
        self.sealed = Some(spec);

        (successor, outcome)
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
