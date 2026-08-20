//! The admission gate: run the ADR hard gate → completeness → tier resolution →
//! pre-approval override over one admission request, and form the auto-tier
//! `approval` [`Evidence`] directly.

use aether_bloomery::{Digest, Evidence, EvidenceKind, Observation, Provenance, Statement, digest_of};
use serde::{Deserialize, Serialize};

use super::policy::{ApprovalPolicy, Tier};

/// The host-projected facts the pre-seal gate decides over. The host populates
/// these from the workpiece's current projection (the GitHub issue in the
/// migration transition); the gate itself is a pure decision over them.
#[derive(Clone, Debug)]
pub struct AdmissionRequest {
    /// The member subject the formed `approval` binds to — its workpiece, scope
    /// revision, and sealed configuration together (ADR-0174,
    /// [`Membership::subject`](aether_bloomery::Membership::subject)).
    pub subject: Digest,
    /// The declared-surface globs (the `## Declared surface` block).
    pub declared_surface: Vec<String>,
    /// The completeness facts the gate fails closed on.
    pub completeness: Completeness,
    /// The ADR-maturity of the change, for the unconditional hard gate.
    pub adr_touch: AdrTouch,
    /// Whether an owner-actor-verified `approval:pre-approved` override is
    /// present — waives the tier (to `auto`), never the gate checks, and never a
    /// firing ADR gate.
    pub pre_approved: bool,
    /// The digest of the exact projection facts the gate evaluated (issue #3583,
    /// rider 3). An `auto`-tier approval folds this into its supporting record so
    /// the sealed evidence attests precisely which facts the gate saw — the same
    /// projection bytes, hashed, cannot then be swapped without moving the
    /// approval's `detail`.
    pub projection_digest: Digest,
}

/// The completeness facts a scope revision must satisfy before it is admissible.
/// Every field is a fail-closed check: a `false` (or a wrong count) refuses the
/// gate rather than forming an approval.
// The many bools are the point: this is a checklist of independent completeness
// signals the host projects, not a state machine — a two-variant enum per signal
// would only rename `true`/`false` without adding meaning.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Completeness {
    /// `## Problem statement` present and non-empty.
    pub has_problem_statement: bool,
    /// `## Design notes` present and non-empty.
    pub has_design_notes: bool,
    /// `## Implementation plan` present and non-empty.
    pub has_implementation_plan: bool,
    /// Every referenced ADR PR has merged.
    pub referenced_adr_prs_merged: bool,
    /// The number of model routings declared — admission requires exactly one.
    pub model_routing_count: usize,
    /// Whether the workpiece is blocked (a blocked one is inadmissible).
    pub blocked: bool,
    /// The declared surface is fresh against the current base.
    pub declared_surface_fresh: bool,
    /// Every `## Depends on` dependency is satisfied: a co-sealed member of this
    /// seal, or a commission whose status is `Landed`.
    pub dependencies_all_closed: bool,
    /// Umbrella integrity holds (not a decomposition umbrella whose children fail
    /// to back-reference).
    pub umbrella_integrity: bool,
}

/// The maturity of the ADRs a change touches — the axis the unconditional hard
/// gate routes on (a glob matches paths, not maturity, so this cannot live in the
/// policy file).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AdrTouch {
    /// The change touches no ADR.
    None,
    /// The change writes a NEW ADR or edits an ESTABLISHED (non-`Proposed`) one —
    /// routes to the owner unconditionally, waiving no override.
    NewOrEstablished,
    /// The change edits only still-`Proposed` ADRs — defers to the tier policy.
    ProposedOnly,
}

/// A completeness check that failed closed, naming which one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Incompleteness {
    /// `## Problem statement` missing or empty.
    MissingProblemStatement,
    /// `## Design notes` missing or empty.
    MissingDesignNotes,
    /// `## Implementation plan` missing or empty.
    MissingImplementationPlan,
    /// A referenced ADR PR has not merged.
    ReferencedAdrPrUnmerged,
    /// Not exactly one model routing.
    ModelRouting(usize),
    /// The workpiece is blocked.
    Blocked,
    /// The declared surface is stale against the current base.
    StaleDeclaredSurface,
    /// A `## Depends on` dependency is neither a co-sealed member nor a landed
    /// commission.
    OpenDependency,
    /// Umbrella integrity does not hold.
    UmbrellaIntegrity,
}

/// The gate's decision for one workpiece.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Decision {
    /// A completeness check failed closed — no approval is formed.
    Incomplete(Incompleteness),
    /// The tier resolved `auto` (and no ADR gate fired): the gate formed the
    /// `approval` [`Evidence`] directly, bound to the scope revision.
    AutoApproved(Evidence),
    /// The tier resolved above `auto` (or the ADR hard gate fired): an
    /// owner-authorized signed [`Statement`] must populate the approval
    /// ([`approval_from_statement`](super::approval_from_statement)). Carries the
    /// resolved tier for the record.
    RequiresStatement(Tier),
}

/// The pre-seal approve gate over one tier policy.
#[derive(Clone, Debug)]
pub struct Gate<'policy> {
    policy: &'policy ApprovalPolicy,
}

impl<'policy> Gate<'policy> {
    /// Build a gate over a parsed tier policy.
    #[must_use]
    pub fn new(policy: &'policy ApprovalPolicy) -> Self {
        Self { policy }
    }

    /// Run the gate over one admission request: ADR hard gate → completeness →
    /// tier resolution → pre-approval override. An `auto` result forms the
    /// `approval` [`Evidence`] directly; anything above `auto` (or a firing ADR
    /// gate) requires a signed statement.
    #[must_use]
    pub fn evaluate(&self, request: &AdmissionRequest) -> Decision {
        if let Some(incompleteness) = check_completeness(&request.completeness) {
            return Decision::Incomplete(incompleteness);
        }
        // The ADR hard gate fires unconditionally and cannot be waived by the
        // pre-approval override; a still-Proposed touch (or no touch) defers to
        // the tier policy.
        let adr_fires = request.adr_touch == AdrTouch::NewOrEstablished;
        let tier = if adr_fires {
            Tier::Human
        } else if request.pre_approved {
            Tier::Auto
        } else {
            self.policy.resolve_surface(&request.declared_surface)
        };
        if tier == Tier::Auto {
            Decision::AutoApproved(auto_approval(request.subject, request.projection_digest))
        } else {
            Decision::RequiresStatement(tier)
        }
    }
}

/// The first completeness check that fails closed, or `None` if the revision is
/// complete.
fn check_completeness(completeness: &Completeness) -> Option<Incompleteness> {
    if !completeness.has_problem_statement {
        return Some(Incompleteness::MissingProblemStatement);
    }
    if !completeness.has_design_notes {
        return Some(Incompleteness::MissingDesignNotes);
    }
    if !completeness.has_implementation_plan {
        return Some(Incompleteness::MissingImplementationPlan);
    }
    if !completeness.referenced_adr_prs_merged {
        return Some(Incompleteness::ReferencedAdrPrUnmerged);
    }
    if completeness.model_routing_count != 1 {
        return Some(Incompleteness::ModelRouting(completeness.model_routing_count));
    }
    if completeness.blocked {
        return Some(Incompleteness::Blocked);
    }
    if !completeness.declared_surface_fresh {
        return Some(Incompleteness::StaleDeclaredSurface);
    }
    if !completeness.dependencies_all_closed {
        return Some(Incompleteness::OpenDependency);
    }
    if !completeness.umbrella_integrity {
        return Some(Incompleteness::UmbrellaIntegrity);
    }
    None
}

/// The source label the auto-tier approval's supporting observation carries.
const AUTO_APPROVAL_SOURCE: &str = "aether.bloomery.approve_gate:auto-tier";

/// The observed words the auto-tier approval's supporting statement asserts.
const AUTO_APPROVAL_WORDS: &[u8] = b"aether.bloomery.approve_gate: policy resolved auto tier";

/// Form the `approval` [`Evidence`] for an `auto`-tier pass — bound to the exact
/// member `subject` (so the seal-time `validate_member_admission` accepts it) and
/// detailing a content-addressed observation record of the grant. An auto
/// approval is *context* (the gate observed the policy resolve `auto`), never
/// instruction — so its supporting artifact is an
/// [`Provenance::ObservationAttestation`], carrying no author signature.
///
/// The supporting record's `parents` pin both the `subject` the approval binds
/// and the `projection_digest` of the exact facts the gate evaluated
/// (issue #3583, rider 3), so the approval's `detail` attests precisely which
/// projection produced the `auto` grant — a swapped projection moves the digest,
/// and the digest is folded into `detail`.
fn auto_approval(subject: Digest, projection_digest: Digest) -> Evidence {
    let record = Statement {
        words: AUTO_APPROVAL_WORDS.to_vec(),
        provenance: Provenance::ObservationAttestation(Observation { source: AUTO_APPROVAL_SOURCE.to_owned() }),
        parents: vec![subject, projection_digest],
    };
    Evidence { subject, kind: EvidenceKind::Approval, detail: digest_of(&record) }
}
