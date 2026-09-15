//! The admin-mode vocabulary (ADR-0219): the acts an operator makes while a
//! bloom is explicitly out of the machine's hands.
//!
//! Every other operator move in [`super::operator`] is a single decision the
//! machine immediately acts on — a brake, a waiver, a candidate. None of them
//! is what a broken bloom actually needs, which is a *window*: stop the
//! dispatch, make several repairs that only make sense together, then hand the
//! bloom back and let the ordinary gates judge what is now there. Making those
//! repairs one at a time against a running reactor is how a red review ends up
//! buying a model lap over members that were already withdrawn.
//!
//! So admin mode is a session, and every act inside it is journaled the same
//! way: an [`AdminNote`] naming the operator and their reason, plus what the
//! act did. [`reason`](AdminNote::reason) and [`operator`](AdminNote::operator)
//! are fields rather than optional context for the reason they are on
//! [`Adjudication`](super::Adjudication) — an act no verdict produced has its
//! audit trail as its whole product — and every door refuses a blank one rather
//! than defaulting it.
//!
//! What admin mode never becomes is a way around a gate's *judgment*. A waiver
//! is recorded as an operator adjudication over findings the bloom actually
//! raised; a supplied candidate re-enters at the ordinary gate position; a
//! re-run is a real dispatch. The one place the operator can outrun a gate — a
//! waived mechanical verify — is refused unless they say in the request that
//! they know what they are doing.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::{ContentAddressed, Digest};
use crate::ids::{StageId, WorkpieceId};
use crate::values::CandidateRef;

/// Who is acting inside an admin session, and why.
///
/// Carried by every admin fact rather than spread across each one's fields, so
/// a new act cannot ship without an audit trail and the seven doors share one
/// blank-refusal check.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AdminNote {
    /// Why, in the operator's own words. Blank is refused at every door.
    pub reason: String,
    /// Who is acting. An unsigned identity, exactly as
    /// [`Adjudication::operator`](super::Adjudication::operator) is: it records
    /// the decider, and admin mode authorizes nothing.
    pub operator: String,
}

impl AdminNote {
    /// Whether both halves say something. A door that answers `false` refuses
    /// rather than defaulting: a default reason is a waiver nobody signed.
    #[must_use]
    pub fn stated(&self) -> bool {
        !self.reason.trim().is_empty() && !self.operator.trim().is_empty()
    }
}

/// Content-addressed so each door can key its idempotency on what the request
/// says. Two identical acts are one act; a second one differing in any field is
/// a distinct act and admits on its own.
impl ContentAddressed for AdminNote {
    const DOMAIN: &'static str = "aether.bloomery.admin_note";
}

/// Cancel one running dispatch without charging anyone for it.
///
/// The nonce is the host's dispatch identity, so the reducer records it rather
/// than validating it — the REST door resolves it against the outstanding-order
/// registry before admitting, which is the only place that registry is
/// readable. What the reducer owns is the consequence: no attempt, no repair
/// roll, no machinery roll, and no cursor movement. A cancelled lane is a lane
/// that never ran, which is exactly the difference between this and the retry
/// door.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AdminLaneCancel {
    /// The workpiece whose lane is being stopped.
    pub workpiece: WorkpieceId,
    /// The host dispatch nonce the order is keyed by.
    pub nonce: String,
    /// Who, and why.
    pub note: AdminNote,
}

impl ContentAddressed for AdminLaneCancel {
    const DOMAIN: &'static str = "aether.bloomery.admin_lane_cancel";
}

/// Hand a workpiece a candidate, without the wedge precondition
/// [`OperatorRepair`](super::OperatorRepair) carries.
///
/// The generalization of the repair door, and the same candidate pair
/// (ADR-0152): the tree returned evidence binds, and the commit the gate's
/// worker checks out. A member lands at `Verify` and the composition lands on
/// its aggregate gates, so the ordinary gates judge what the operator supplied
/// — the dispatch itself is withheld until the session closes, because admin
/// mode dispatches nothing.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AdminCandidate {
    /// The member, or the reserved composition id.
    pub workpiece: WorkpieceId,
    /// The candidate the operator pushed.
    pub candidate: CandidateRef,
    /// Who, and why.
    pub note: AdminNote,
}

impl ContentAddressed for AdminCandidate {
    const DOMAIN: &'static str = "aether.bloomery.admin_candidate";
}

/// Run one stage again on the candidate the workpiece is holding.
///
/// Distinct from the retry door, which journals an executor fault and spends a
/// machinery roll: this spends nothing, because the operator is not asserting
/// the stage failed to judge its subject — they are asserting the *record* now
/// says something different from what the stage last judged.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AdminRerun {
    /// The member, or the reserved composition id.
    pub workpiece: WorkpieceId,
    /// The stage to run. Refused unless the record can honestly run the
    /// workpiece from it.
    pub stage: StageId,
    /// Dispatch immediately rather than when the session closes. Still subject
    /// to the single-writer rules — the dispatch is an ordinary work order.
    pub now: bool,
    /// Who, and why.
    pub note: AdminNote,
}

impl ContentAddressed for AdminRerun {
    const DOMAIN: &'static str = "aether.bloomery.admin_rerun";
}

/// Void a red verdict's findings so the gate counts as passed for landing.
///
/// Evidence-bound on purpose: a waiver names the finding digests it voids, and
/// every one has to be a finding this bloom actually raised. That is the whole
/// difference between waiving a defect and inventing a pass — what the ledger
/// records is an operator adjudication over named evidence, never a synthesized
/// green verdict.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AdminWaiver {
    /// The gate whose verdict is being voided.
    pub gate: StageId,
    /// The verdict artifact digests being voided — the `detail` each
    /// [`CompositionFinding`](super::CompositionFinding) carries, or the
    /// bloom-scope park's own question.
    pub findings: Vec<Digest>,
    /// The operator's acknowledgement that waiving a *mechanical* gate lands
    /// code no gate proved. Ignored for a review waiver and required for a
    /// verify one.
    pub acknowledged_unverified: bool,
    /// Who, and why.
    pub note: AdminNote,
}

impl ContentAddressed for AdminWaiver {
    const DOMAIN: &'static str = "aether.bloomery.admin_waiver";
}

/// Discard a completed lap's captured candidate.
///
/// The workpiece's cursor keeps its stage and its spent counters and goes back
/// to the candidate it held before that lap. The lap's evidence stays on the
/// record — a dropped candidate is still something that happened, and erasing
/// its verdict would leave the journal claiming a lap that never ran.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AdminLapDrop {
    /// The workpiece whose candidate is reverting.
    pub workpiece: WorkpieceId,
    /// The host dispatch nonce of the lap being dropped — recorded, not
    /// validated, exactly as [`AdminLaneCancel::nonce`] is.
    pub nonce: String,
    /// Who, and why.
    pub note: AdminNote,
}

impl ContentAddressed for AdminLapDrop {
    const DOMAIN: &'static str = "aether.bloomery.admin_lap_drop";
}

/// What one journaled admin act did.
///
/// One enum rather than one record decision per verb: the acts differ in what
/// they moved and not at all in how they are recorded, and a reader of the
/// session wants them in one ordered list.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AdminActKind {
    /// The session opened.
    Entered,
    /// The session closed.
    Exited,
    /// A running lane was cancelled, charging nobody.
    LaneCancelled {
        /// The workpiece whose lane stopped.
        workpiece: WorkpieceId,
        /// The host dispatch nonce.
        nonce: String,
    },
    /// A workpiece was handed a candidate.
    CandidateSet {
        /// The workpiece.
        workpiece: WorkpieceId,
        /// What it now holds.
        candidate: CandidateRef,
    },
    /// A stage was queued to run again.
    Rerun {
        /// The workpiece.
        workpiece: WorkpieceId,
        /// The stage.
        stage: StageId,
        /// Whether the work order went out at once rather than on exit.
        now: bool,
    },
    /// A red verdict's findings were voided.
    Waived {
        /// The gate whose verdict was voided.
        gate: StageId,
        /// The verdict artifacts voided.
        findings: Vec<Digest>,
        /// Whether the operator acknowledged landing unverified code.
        acknowledged_unverified: bool,
    },
    /// A completed lap's candidate was discarded.
    LapDropped {
        /// The workpiece.
        workpiece: WorkpieceId,
        /// The host dispatch nonce of the discarded lap.
        nonce: String,
        /// The candidate that was thrown away.
        discarded: CandidateRef,
        /// The candidate the cursor went back to.
        restored: CandidateRef,
    },
    /// The session closed onto a landing that stands on a waiver.
    ///
    /// The land record ADR-0219 asks for. It rides the admin log rather than
    /// [`LandingReceipt`](super::LandingReceipt), which is embedded mid-enum in
    /// the frozen decision mirrors and cannot gain a field without one.
    LandedOnWaiver {
        /// The head being proposed.
        head: Digest,
        /// The verdict artifacts the landing stands on a waiver of.
        waivers: Vec<Digest>,
    },
}

/// One journaled admin act: what it did, and on whose word.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AdminAct {
    /// What happened.
    pub kind: AdminActKind,
    /// Who, and why.
    pub note: AdminNote,
}

impl ContentAddressed for AdminAct {
    const DOMAIN: &'static str = "aether.bloomery.admin_act";
}

impl AdminAct {
    /// The verdict artifacts this act voided, or nothing for an act that voided
    /// none — what the view and the land record read to render a waiver.
    #[must_use]
    pub fn waived(&self) -> &[Digest] {
        match &self.kind {
            AdminActKind::Waived { findings, .. } => findings,
            _ => &[],
        }
    }
}
