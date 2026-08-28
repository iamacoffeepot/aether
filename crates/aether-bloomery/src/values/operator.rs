//! The manager override's vocabulary (#4957, #4976): the moves an operator makes
//! when the machine has run out of its own, or is about to spend on something
//! that looks wrong.
//!
//! Every other escape from a stopped bloom is a *machine* move — another model
//! lap, a supersession, abandonment. None of them is the move an operator
//! actually wants when they have read the remaining defect and are prepared to
//! answer for it: close the finding and let the bloom proceed, or supply the fix
//! themselves and let the gates judge it exactly as they judge a lane's. And
//! none of them is the move for a bloom that has not stopped yet but should —
//! the brake that freezes new dispatch while the laps already running finish
//! ([`OperatorHold`]).
//!
//! Both are recorded rather than merely performed. An override is the one act in
//! the pipeline whose authority is a person rather than a verdict, so what it
//! waived, why, and who said so is the whole audit trail — which is why `reason`
//! and `operator` are fields of the value and not optional context around it. A
//! blank one is refused at both doors rather than defaulted, because a default
//! reason is a waiver nobody signed.
//!
//! What an override never does is stand in for an approval. It adjudicates
//! findings and retry budgets; a member whose sealed approval resolves above
//! `auto` still needs its signed statement, and both doors refuse a bloom whose
//! membership is not fully approved rather than carrying it to a landing.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest};
use crate::ids::WorkpieceId;
use crate::values::CandidateRef;

/// What the operator decided about the findings they adjudicated.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Disposition {
    /// The findings are accepted as they stand: the operator has read them and
    /// judges the composed tree landable anyway.
    Accepted,
    /// The findings are deferred to work that is already filed.
    ///
    /// The issue number is required rather than optional, which is the whole
    /// difference between deferring a finding and losing one: a waived defect
    /// that names no filed work is indistinguishable from a defect nobody
    /// mentioned again.
    Deferred {
        /// The filed follow-up the findings are deferred to.
        issue: u64,
    },
}

/// One operator adjudication of a bloom's open composition findings (#4957).
///
/// The subject is the composition's findings channel
/// ([`BloomRecord::composition_findings`](crate::BloomRecord::composition_findings)),
/// never a member: a member that has passed its review is immutable (ADR-0191
/// §4), so there is nothing about one for an adjudication to reopen. What it
/// closes is what the composition's own review, verify, or landing gate raised
/// and could not repair inside its budget.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Adjudication {
    /// The findings being closed, named by the verdict artifact digest each
    /// [`CompositionFinding`](crate::CompositionFinding) carries as its
    /// `detail`. Every one must be an open finding on the bloom — an
    /// adjudication cannot close what was never raised.
    pub findings: Vec<Digest>,
    /// Accepted as they stand, or deferred to filed work.
    pub disposition: Disposition,
    /// Why, in the operator's own words — carried into the landing proposal so
    /// the merged history names what was waived and on what grounds.
    pub reason: String,
    /// Who adjudicated. An unsigned identity, deliberately: it records the
    /// decider, and it is not and cannot become the signed authority an
    /// above-`auto` approval needs.
    pub operator: String,
}

/// Content-addressed so the REST door can key an adjudication's idempotency on
/// what it says. Two identical waivers are one waiver; a second one that differs
/// in any field — a different finding set, a different reason — is a distinct
/// act and admits on its own.
impl ContentAddressed for Adjudication {
    const DOMAIN: &'static str = "aether.bloomery.adjudication";
}

/// One operator-supplied repair candidate (#4957): the fix the operator pushed
/// to the workpiece's candidate ref, offered to the ordinary gates.
///
/// Only the model lap is skipped. The candidate re-enters at `Verify` and faces
/// the same mechanical suite and the same delta-confirm review a lane's would,
/// so a bad operator fix bounces exactly where a bad lane's does. That is what
/// makes this an execution decision rather than an authorization: the operator
/// is choosing who writes the code, never who judges it.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OperatorRepair {
    /// The workpiece being repaired — a wedged member, or the reserved
    /// [`WorkpieceId::COMPOSITION`](crate::WorkpieceId::COMPOSITION) when the
    /// operator wove the composition themselves.
    pub workpiece: WorkpieceId,
    /// The candidate the operator pushed: the tree returned evidence binds, and
    /// the commit the verifying lane checks out. Both, because they are
    /// independent digests (ADR-0152) and the gate needs each for a different
    /// thing.
    pub candidate: CandidateRef,
    /// Why the operator took the lap themselves.
    pub reason: String,
    /// Who supplied it — the decider this attempt is journaled under.
    pub operator: String,
}

/// Content-addressed for the reason [`Adjudication`] is: the default
/// idempotency key is the repair's own digest, so a resent request is one
/// dispatch and a genuinely different candidate is its own.
impl ContentAddressed for OperatorRepair {
    const DOMAIN: &'static str = "aether.bloomery.operator_repair";
}

/// The operator change the coordinator will write (ADR-0205): a signed
/// candidate waiting to become a bloom with no members.
///
/// Shaped after [`OperatorRepair`]: the same [`CandidateRef`] pair, the same
/// required non-blank `reason` and `operator` for the audit trail, and the same
/// content-addressed request so the digest is the idempotency key. It is also a
/// configuration kind — its address in the bloom-wide registry is what gives a
/// memberless bloom a distinct identity, because two empty specs over one base
/// would otherwise collide on [`BloomSpec`](crate::BloomSpec)'s digest.
#[derive(aether_data::Kind, aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[kind(name = "aether.bloomery.operator_proposal")]
pub struct OperatorProposal {
    /// The candidate the operator supplied: the tree returned evidence binds,
    /// and the commit the verifying lane checks out.
    pub candidate: CandidateRef,
    /// Why the operator is asking the coordinator to write this.
    pub reason: String,
    /// Who proposed it — the decider this request is journaled under.
    pub operator: String,
}

impl ContentAddressed for OperatorProposal {
    const DOMAIN: &'static str = "aether.bloomery.operator_proposal";
}

/// One edge of an operator hold (#4976): the words and the identity behind
/// putting a bloom's dispatch on the brake, or taking it back off.
///
/// The same shape serves both edges because both say exactly the same two
/// things, and neither says anything else — a hold carries no scope, no
/// priority, and no expiry. It is bloom-level and flat: freeze new dispatch
/// (member laps and the two aggregate gates), decide, release. What varies
/// between raising it and dropping it is the fact that carries it, not the
/// value.
///
/// [`reason`](Self::reason) and [`operator`](Self::operator) are fields rather
/// than optional context for the reason they are on [`Adjudication`]: a brake
/// pulled on a running bloom is an act no verdict produced, so the record of who
/// pulled it and why is its whole product. Both doors refuse a blank one rather
/// than defaulting it.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OperatorHold {
    /// Why the bloom was held, or why it is being let go — in the operator's
    /// own words.
    pub reason: String,
    /// Who asked for it. An unsigned identity, exactly as
    /// [`Adjudication::operator`] is: it records the decider, and a hold
    /// authorizes nothing.
    pub operator: String,
}

/// Content-addressed for the reason [`Adjudication`] is, and with the same
/// effect: the default idempotency key is the hold's own digest, so a resent
/// request is a duplicate rather than a second brake. The two doors prefix the
/// key with their own route, so a hold and a release stating identical words are
/// still distinct acts.
impl ContentAddressed for OperatorHold {
    const DOMAIN: &'static str = "aether.bloomery.operator_hold";
}

/// Why a member left the line without resolving (#5327).
///
/// The two causes are kept apart because they answer different questions for
/// whoever reads the record later. An operator withdrawal is a decision, and
/// its reason is the operator's own words; a dependency withdrawal is a
/// consequence, and its reason is the ancestor that made the member
/// unreachable. Flattening them would lose which of the two a reader is
/// looking at.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum WithdrawalCause {
    /// An operator named this member directly.
    Operator,
    /// This member's construct base was withdrawn, so it can never enter the
    /// line: the ancestor it depends on will never carry a claim.
    Dependency {
        /// The withdrawn ancestor.
        on: WorkpieceId,
    },
}

/// One member's terminal departure from a walking bloom (#5327).
///
/// Modelled on [`OperatorHold`] rather than on a wedge: a wedge is something a
/// member *earns* by exhausting a budget, and this is an act no verdict
/// produced. So [`reason`](Self::reason) and [`operator`](Self::operator) are
/// fields for the reason they are fields there — the audit trail is the whole
/// product — and both doors refuse a blank one rather than defaulting it.
///
/// Withdrawal is one-way. There is no un-withdraw, deliberately: a member
/// wrongly withdrawn is re-scoped and sealed into a later bloom, which is
/// exactly what releasing its claim ref makes possible. It buys no attempt,
/// revokes no sibling's claim, and does not un-resolve the bloom.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Withdrawal {
    /// The member leaving the line.
    pub workpiece: WorkpieceId,
    /// Whether an operator named it, or an ancestor's withdrawal stranded it.
    pub cause: WithdrawalCause,
    /// In the operator's own words. Blank is refused at both doors.
    pub reason: String,
    /// Who decided. Unsigned, exactly as [`OperatorHold::operator`] is: it
    /// records the decider, and a withdrawal authorizes nothing.
    pub operator: String,
}

/// Content-addressed for the reason [`OperatorHold`] is: the default
/// idempotency key is the withdrawal set's own digest, so a resent request is
/// one act rather than two.
impl ContentAddressed for Withdrawal {
    const DOMAIN: &'static str = "aether.bloomery.withdrawal";
}
