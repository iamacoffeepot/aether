//! The projection port (ADR-0149 §The boundary): push a self-contained view
//! document outward. The projection's direction is *outward* — Bloomery's
//! internals are the truth and the adapter mirrors them onto the objects the
//! repository already holds; a projection never drives the reducer.
//!
//! The port hands the adapter a whole typed [`ViewDocument`], not opaque ids.
//! ADR-0149 §The boundary (as amended by [#3471]) requires the reconcile push
//! to be "self-contained view documents … an adapter never queries back into
//! the store": every field the outward mirror renders — per-workpiece
//! membership, its sealed scope revision, its approval evidence, and its
//! resolution claim once integrated — rides on the document itself, so an
//! adapter stays dumb, stateless, and rebuildable from the journal.
//!
//! [#3471]: https://github.com/iamacoffeepot/aether/issues/3471

use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use alloc::string::String;
use core::str::from_utf8;

use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::reduce::{BloomStatus, RecordedRefusal};
use crate::values::{
    CandidateRef, CompositionFinding, Evidence, LandingReceipt, OperatorHold, ResolutionClaim, SpendQuiesce,
    SurfacePathRequest, VerifyFailure, VerifyFailureSet, Wedge,
};

/// The self-contained render input a reconcile pushes outward: the current
/// mainline, the last-reported observed head, and every projectable bloom,
/// each carrying its full membership. A pure projection of the journal —
/// idempotent and rebuildable after a deletion (ADR-0149 §The boundary). An
/// adapter renders entirely from this value and never queries back into the
/// store.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "testing"), derive(Default))]
pub struct ViewDocument {
    /// The current mainline head.
    pub mainline: Digest,
    /// The head the source last reported (#4709) — the other successor base
    /// `reduce_supersede` admits, beside [`Self::mainline`].
    pub observed: Digest,
    /// The spend-quiesce marker the last crossing recorded (ADR-0192).
    ///
    /// `None` when the door is open. Carried on the document so `GET /view`
    /// and `GET /blooms` render the axis, spend, and ceiling without a
    /// query-back into the journal. `#[serde(default)]` so a reader that
    /// predates the field still decodes.
    #[serde(default)]
    pub spend_quiesce: Option<SpendQuiesce>,
    /// The blooms to mirror, each self-describing.
    pub blooms: Vec<BloomView>,
    /// A red whole-workspace base receipt, when one is holding the day
    /// (ADR-0200). Trailing plus `serde(default)` so a reader that predates
    /// the field still decodes.
    #[serde(default)]
    pub base_alert: Option<BaseAlertView>,
}

/// The day-level stop a red base receipt raises: which tree failed, and which
/// gates named the failure. `failed` is rendered as
/// [`VerifyFailure::as_str`] so the console paints gate names without owning
/// the vocabulary.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BaseAlertView {
    /// The commit the verify ran at.
    pub base: Digest,
    /// The tree it peeled to.
    pub tree: Digest,
    /// Gate names that failed, in canonical identity order.
    pub failed: Vec<String>,
    /// The evidence digest the red verdict bound.
    pub evidence: Digest,
}

impl BaseAlertView {
    /// Render `failed` from a typed set.
    #[must_use]
    pub fn from_failure_set(base: Digest, tree: Digest, failed: VerifyFailureSet, evidence: Digest) -> Self {
        Self { base, tree, failed: failed.iter().map(VerifyFailure::as_str).map(str::to_owned).collect(), evidence }
    }
}

/// One bloom's outward view: its sealed identity, lifecycle status, optional
/// successor, the full per-member render data, the composition workpiece's
/// own line when it has a cursor, a wedge, or an open finding, — when the
/// aggregate review has parked the bloom — the bloom-scoped question an
/// operator must settle, and — when an operator has pulled the brake — the
/// hold that distinguishes a frozen bloom from an idle one.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BloomView {
    /// The bloom's identity — the sealed-spec digest ([`crate::BloomSpec::id`]).
    pub id: BloomId,
    /// The bloom's position in the one-way lifecycle.
    pub status: BloomStatus,
    /// The successor that replaced this bloom, if it was superseded.
    pub superseded_by: Option<BloomId>,
    /// One entry per sealed member, in the spec's canonical order.
    pub members: Vec<MemberView>,
    /// How many landing attempts the bloom's gate has refused (#4689), and how
    /// many the sealed catalog allows.
    ///
    /// `Some` only once a landing has actually been refused, so an ordinary
    /// bloom carries nothing here. What an operator reads to tell a bloom that
    /// is *waiting* on its landing from one that is blocked by it — the two
    /// were indistinguishable in this document, which is why a red landing
    /// branch could sit unnoticed while the reactor polled it.
    pub landing_blocked: Option<LandingBlock>,
    /// The bloom's aggregate-review executor-fault series, once one has been
    /// recorded (ADR-0176); `None` for an ordinary bloom.
    ///
    /// The only outward evidence that a bloom is stalled on its *host* rather
    /// than on its work. Without it a terminal executor fault is
    /// indistinguishable from a bloom sitting quietly between dispatches —
    /// which is the shape an operator has no reason to look at.
    pub executor_fault: Option<ExecutorFaultView>,
    /// The bloom-scoped aggregate-review park (ADR-0153), once the review has
    /// raised a question the owner must settle; `None` for an ordinary bloom.
    ///
    /// Bloom-level, not a member hold: attaching it to every
    /// [`MemberView::pending_decision`] would change member semantics. The
    /// digest is always present so a live-query path that cannot resolve the
    /// question artifact still names what `adjudicate --finding` must quote.
    /// `#[serde(default)]` so a reader that predates the field still decodes.
    #[serde(default)]
    pub review_park: Option<ReviewParkView>,
    /// The composition workpiece's own line (ADR-0191): its cursor, a wedge in
    /// the same shape a member wedge renders, and the open findings whose
    /// `detail` digests `adjudicate --finding` must quote. `None` while the
    /// composition has never taken a cursor, wedged, or filed a finding — an
    /// ordinary bloom stays unchanged. Trailing and `#[serde(default)]` so a
    /// reader that predates the field still decodes.
    #[serde(default)]
    pub composition: Option<CompositionView>,
    /// The operator brake currently on this bloom (#4976); `None` while it
    /// dispatches normally. Without it a held bloom and an idle one render
    /// identically. Trailing and `#[serde(default)]` so a reader that predates
    /// the field still decodes.
    #[serde(default)]
    pub operator_hold: Option<OperatorHold>,
    /// Why the bloom's fold refused, once it has (ADR-0206). `None` while the
    /// fold has not refused — an ordinary bloom stays unchanged. Trailing and
    /// `#[serde(default)]` so a reader that predates the field still decodes.
    #[serde(default)]
    pub blocker: Option<RecordedRefusal>,
    /// Every write lease this bloom's lanes hold (ADR-0204), in path order.
    ///
    /// Bloom-scoped rather than only per-member because the question
    /// contention raises is "who holds this path", and answering it from
    /// [`MemberView::leases`] means scanning every member and inverting the
    /// map — which is how an invisible lease turns contention into an
    /// unexplained stall (ADR-0198). Empty while nothing has been observed
    /// writing, and empty again once the bloom finishes: no lease survives its
    /// bloom. Trailing and `#[serde(default)]` so a reader that predates the
    /// field still decodes.
    #[serde(default)]
    pub leases: Vec<LeaseView>,
}

/// One held write lease, rendered with everything ADR-0198 asks a lease
/// surface to show: the path, who holds it, where that holder is, and how old
/// the lease is.
///
/// `stage` is read off the holder's *current* cursor rather than stored on the
/// lease, because the operator question is "who holds this path and what are
/// they doing" — a lease taken at Construct and still held while its holder
/// refines should read Refine, not the stage of the observation that took it.
/// `None` when the holder carries no cursor at all, which is the moment
/// between an eviction and its resume.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LeaseView {
    /// The repository-relative path under lease.
    pub path: String,
    /// The member holding it.
    pub holder: WorkpieceId,
    /// Where the holder stands now, when it has a cursor.
    pub stage: Option<StageId>,
    /// When the lease was taken, in unix milliseconds — the age an operator
    /// reads against the bloom's other timestamps.
    pub acquired_at: u64,
}

/// The composition workpiece's outward line: the cursor a weave repair sits
/// at, the wedge that stops it, and the open findings an operator must name.
///
/// Every field serializes in declaration order even when it is `None` or
/// empty: `aether_data::wire` encodes structs positionally, so omitting a
/// slot shifts the next bloom's bytes into it. `#[serde(default)]` is only
/// for a human-readable reader that predates a field.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "testing"), derive(Default))]
pub struct CompositionView {
    /// The composition's stage cursor, when a weave repair (or its follow-on
    /// gate) has written one. `None` when the composition has findings or a
    /// wedge but no cursor of its own.
    #[serde(default)]
    pub cursor: Option<CompositionCursorView>,
    /// Why the composition stopped, once it has wedged; `None` while it is
    /// still working. Same [`Wedge`] shape a [`MemberView::wedge`] renders —
    /// `stage` is `wedged_at`, `evidence` is the digest a reader follows.
    #[serde(default)]
    pub wedge: Option<Wedge>,
    /// The composition findings no operator adjudication has closed, each
    /// carrying the `detail` digest `adjudicate --finding` quotes.
    #[serde(default)]
    pub findings: Vec<CompositionFinding>,
}

/// A workpiece's stage cursor as the operator reads it: the stage, how many
/// attempts that stage has taken, and the candidate it is targeting.
///
/// Shared by [`CompositionView::cursor`] and [`MemberView::cursor`] — the
/// composition cursor already proved this shape.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CompositionCursorView {
    /// The stage the composition currently sits at.
    pub stage: StageId,
    /// Attempts dispatched against that stage so far.
    pub attempts: u32,
    /// The weave the composition is repairing, when one has been captured.
    #[serde(default)]
    pub candidate: Option<CandidateRef>,
}

/// A bloom's aggregate-review park, rendered so an operator can see that the
/// bloom is waiting on an owner decision rather than sitting idle (ADR-0153).
///
/// The digest is always present — it is the finding `adjudicate --finding`
/// names. Stage, prompt, options, and blocked ride only when the question
/// artifact resolved; a missing artifact still exposes the digest.
///
/// Every field serializes in declaration order even when it is `None` or
/// empty: `aether_data::wire` encodes structs positionally, so omitting a
/// slot shifts the next bloom's bytes into it. `#[serde(default)]` is only
/// for a human-readable reader that predates a field.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "testing"), derive(Default))]
pub struct ReviewParkView {
    /// The parked [`crate::Question`]'s content-addressed digest — or, for a
    /// ceiling park, the failing review's record artifact. Either way this is
    /// the exact digest an operator adjudication names.
    pub question: Digest,
    /// The held stage, when the question artifact resolved.
    #[serde(default)]
    pub stage: Option<StageId>,
    /// The decision to be made, when the question artifact resolved.
    #[serde(default)]
    pub prompt: Option<String>,
    /// The options considered, when the question artifact resolved.
    #[serde(default)]
    pub options: Vec<String>,
    /// What the park blocks, when the question artifact resolved.
    #[serde(default)]
    pub blocked: Option<String>,
}

/// A bloom's aggregate-review executor-fault standing, rendered once its review
/// has reported that it could not judge the fold (ADR-0176).
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ExecutorFaultView {
    /// The fold tree the faults are against.
    pub subject: Digest,
    /// Faults taken on that fold so far.
    pub rolls: u32,
    /// The `AggregateReview` binding's retry budget from the sealed catalog —
    /// the bound stated rather than left for a reader to know.
    pub budget: u32,
    /// The latest fault report's artifact digest.
    pub evidence: Digest,
    /// Whether the series has reached its ceiling. `true` is terminal: the
    /// bloom dispatches nothing further and recovery is an explicit successor
    /// after the environment is repaired, not another poll.
    pub terminal: bool,
}

/// A bloom's landing-gate standing, rendered when its landing has been refused.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LandingBlock {
    /// Landing attempts refused so far.
    pub rolls: u32,
    /// The `Land` binding's retry budget from the sealed catalog — the bound
    /// stated rather than left for a reader to know.
    pub budget: u32,
}

/// One member's outward view: the admitted workpiece, the exact scope
/// revision the bloom pinned, the approval evidence bound to that revision,
/// — once the member is integrated — its resolution claim, — while its
/// stage is held on a parked question — its pending-decision, and — once it
/// has been dispatched — its stage cursor. A member is integrated iff
/// `resolution` is `Some` (the coarse per-member state the reducer tracks)
/// and held iff `pending_decision` is `Some`.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MemberView {
    /// The admitted workpiece.
    pub workpiece: WorkpieceId,
    /// The exact scope-revision digest sealed into the bloom.
    pub scope_revision: Digest,
    /// The approval evidence, bound to `scope_revision`.
    pub approval: Evidence,
    /// The member's resolution claim once integrated; `None` until then.
    pub resolution: Option<ResolutionClaim>,
    /// The member's pending-decision hold while a parked question holds its
    /// stage; `None` when the member is not held (ADR-0151). Carried on the
    /// self-contained document so the outward adapter renders the question with
    /// no query-back into the store.
    pub pending_decision: Option<PendingDecisionView>,
    /// Why the member stopped, once it has wedged; `None` while it is still
    /// working. A wedge is terminal — the member never dispatches again and the
    /// bloom can never resolve — so it has to be readable here, or a stopped
    /// member and a working one render identically.
    pub wedge: Option<Wedge>,
    /// The ancestor whose unresolved or wedged state is why this member has
    /// not entered the line (ADR-0196). `None` while the member is working,
    /// already resolved, or a root that dispatched at seal. `#[serde(default)]`
    /// so a reader that predates the field still decodes.
    #[serde(default)]
    pub blocked_by: Option<WorkpieceId>,
    /// Why this member is sitting at Verify without a dispatched attempt
    /// (#5020): the host could not run the gates. `None` while the member is
    /// working, wedged, or resolved. The findings name the missing tools
    /// verbatim. `#[serde(default)]` so a reader that predates the field
    /// still decodes.
    #[serde(default)]
    pub host_fault: Option<HostFaultView>,
    /// Machinery faults this member has taken on its current stage (ADR-0195).
    /// `0` while none have been recorded. `#[serde(default)]` so a reader that
    /// predates the field still decodes.
    #[serde(default)]
    pub machinery_rolls: u32,
    /// The sealed retry budget the machinery series is bounded by — the bound
    /// stated rather than left for a reader to know. `0` when the member has
    /// no cursor yet. `#[serde(default)]` so a reader that predates the field
    /// still decodes.
    #[serde(default)]
    pub machinery_budget: u32,
    /// Why the member stopped, once it has wedged (ADR-0195). `None` while it
    /// is still working. Distinguishes a sick host from rejected work at the
    /// door an operator reads. `#[serde(default)]` so a reader that predates
    /// the field still decodes.
    #[serde(default)]
    pub wedge_cause: Option<WedgeCause>,
    /// The member's stage cursor, when it has dispatch history; `None` for a
    /// member that has never entered the line. Same [`CompositionCursorView`]
    /// shape the composition cursor already proves — stage, attempts, and the
    /// candidate it is targeting. Trailing and `#[serde(default)]` so a reader
    /// that predates the field still decodes.
    #[serde(default)]
    pub cursor: Option<CompositionCursorView>,
    /// A construct that concluded without a candidate (#5292, #5332). `None`
    /// while the member is working, wedged, or resolved. Distinct from
    /// [`Self::pending_decision`]: a park is not an ADR-0151 question, and both
    /// liveness oracles read this field rather than reconstructing it from
    /// snapshot state. Trailing and `#[serde(default)]` so a reader that
    /// predates the field still decodes.
    #[serde(default)]
    pub park: Option<crate::MemberPark>,
    /// The surface amendment this member is waiting on (ADR-0207). `None`
    /// while it is working, wedged, or resolved. Distinct from [`Self::wedge`]
    /// (the machinery could not push it through) and from
    /// [`Self::pending_decision`] (an ADR-0151 question an answer settles):
    /// this member is waiting on a person to widen a boundary, and more
    /// attempts cannot help. Trailing and `#[serde(default)]` so a reader that
    /// predates the field still decodes.
    #[serde(default)]
    pub awaiting_surface: Option<AwaitingSurfaceView>,
    /// Why the member left the line, when an operator withdrew it or its
    /// dependency was withdrawn (#5327). `None` for every member still in the
    /// line. Distinct from [`Self::wedge`], which a member earns by exhausting
    /// a budget and which a grant can undo: a withdrawal is a person's
    /// decision and is one-way. Trailing and `#[serde(default)]` so a reader
    /// that predates the field still decodes.
    #[serde(default)]
    pub withdrawn: Option<WithdrawnView>,
    /// The repository paths this member holds a write lease on (ADR-0204), in
    /// path order. Empty for a member whose lane has written nothing yet, and
    /// for every member of a bloom that has finished — no lease survives its
    /// bloom. Trailing and `#[serde(default)]` so a reader that predates the
    /// field still decodes.
    #[serde(default)]
    pub leases: Vec<String>,
    /// The earlier-canonical sibling that took a path this member held,
    /// stopping its lane until that sibling integrates (ADR-0204). `None` for
    /// a member that is working normally. Distinct from [`Self::blocked_by`],
    /// which names a *declared* dependency that has not resolved: this member
    /// declared nothing and would have run, and the file it collided on is the
    /// only reason it is waiting. Trailing and `#[serde(default)]` so a reader
    /// that predates the field still decodes.
    ///
    /// `None` on every bloom a current coordinator walks: #5401 retracted the
    /// eviction, so a shared file is merged at integration instead. The slot
    /// stays for a projection served off a journal an older binary wrote.
    #[serde(default)]
    pub evicted_by: Option<LeaseEvictionView>,
}

/// One transition's answer to "why is this not happening" (#5281).
///
/// Read off stored facts, never re-derived: the state comes from record fields
/// the reducer already wrote and `refusal` carries the ADR-0206 refusal the
/// boundary recorded when it stopped. A boundary whose guards have not been
/// converted to gates yet reports its state with `refusal: None` rather than a
/// hand-written account of its reasoning — a second description of the decision
/// path is exactly what would drift and then lie.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TransitionWhy {
    /// The transition, by the name the machinery uses for it: `dispatch_member`,
    /// `fold`, `aggregate_verify`, `aggregate_review`, or `land`.
    pub transition: String,
    /// Where the transition stands.
    pub state: WhyState,
    /// One sentence naming the stored values behind [`Self::state`].
    pub because: String,
    /// The recorded refusal this boundary stopped on, when it recorded one
    /// (ADR-0206).
    pub refusal: Option<RecordedRefusal>,
    /// The transition further down the chain this one waits on; `None` when
    /// nothing below it is the reason. This is what makes the answer a chain
    /// rather than a flat list — not landing because no integration is
    /// recorded, no integration because the fold refused.
    pub waiting_on: Option<String>,
}

/// Where one transition or member stands (#5281).
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum WhyState {
    /// It has already happened.
    Done,
    /// It is happening now — a lane is out, or the dispatch is decided.
    InFlight,
    /// It has not happened and something named is why.
    Blocked,
    /// It ran and refused, and the refusal is recorded.
    Refused,
}

/// One member's answer to "why is this member not moving" (#5281).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MemberWhy {
    /// The member.
    pub workpiece: WorkpieceId,
    /// Where it stands.
    pub state: WhyState,
    /// One sentence naming the stored values behind [`Self::state`].
    pub because: String,
    /// The ancestor holding it out of the line, when a declared edge is why
    /// (ADR-0196) — the same answer [`MemberView::blocked_by`] carries, from
    /// the same function.
    pub blocked_by: Option<WorkpieceId>,
    /// The `dispatch_member` guard that refused this member's entry into the
    /// line, when one is stored (ADR-0206).
    ///
    /// Trailing and `#[serde(default)]`, so a reader of an older projection
    /// still decodes. `None` is the ordinary case: a member that is working,
    /// resolved, or held out by a rung the chain above already names has
    /// nothing to refuse.
    #[serde(default)]
    pub refusal: Option<RecordedRefusal>,
}

/// Why one bloom is not advancing (#5281).
///
/// A stalled member and a stalled composition have different causes, so both
/// are reported: [`Self::chain`] runs from the land down to member dispatch,
/// each rung naming the one below it, and [`Self::members`] answers per member.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct WhyDocument {
    /// The bloom asked about.
    pub bloom: BloomId,
    /// Its status, so the answer is readable without a second request.
    pub status: BloomStatus,
    /// The transitions, outermost first: land, aggregate review, aggregate
    /// verify, fold, member dispatch.
    pub chain: Vec<TransitionWhy>,
    /// One answer per sealed member, in sealed order.
    pub members: Vec<MemberWhy>,
}

/// Why a member's lane stopped for a file another member took (ADR-0204),
/// rendered so the board can tell it from a member still working.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LeaseEvictionView {
    /// The earlier-canonical member that took the path.
    pub by: WorkpieceId,
    /// The contended path — the whole reason, named.
    pub path: String,
    /// When the eviction was decided, in unix milliseconds, so an operator
    /// reads the lease's age the way ADR-0198 asks a lease surface to show it.
    pub evicted_at: u64,
}

/// A member withdrawn from a walking bloom (#5327), rendered so the board can
/// tell it from a member still working without opening the journal.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct WithdrawnView {
    /// `"operator"` when an operator named this member, `"dependency"` when an
    /// ancestor's withdrawal stranded it. A stable string rather than the
    /// value enum because this is the outward wire an absent-tolerant console
    /// reads.
    pub cause: String,
    /// The withdrawn ancestor, for a `"dependency"` cause; `None` otherwise.
    pub depends_on: Option<WorkpieceId>,
    /// Why, in the operator's own words.
    pub reason: String,
    /// Who decided.
    pub operator: String,
}

/// A member awaiting a surface amendment (ADR-0207), rendered so an operator
/// reads which paths are needed without opening an evidence file.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct AwaitingSurfaceView {
    /// The stage that declined.
    pub stage: StageId,
    /// The sealed revision the requested paths are additions to.
    pub scope_revision: Digest,
    /// The lane's evidence artifact.
    pub evidence: Digest,
    /// The requested paths and their one-line reasons.
    pub paths: Vec<SurfacePathRequest>,
    /// The lane's one-line summary.
    pub summary: String,
    /// Requests this member has made in this bloom.
    pub requests: u32,
}

/// Why a member stopped dispatching (ADR-0195).
///
/// A `Work` wedge exhausted the stage's ordinary retry or repair budget. A
/// `Machinery` wedge exhausted the independent machinery series — the host
/// never judged the candidate.
#[derive(aether_data::Schema, Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum WedgeCause {
    /// The stage's work or repair budget was exhausted.
    Work,
    /// The independent machinery-retry budget was exhausted.
    Machinery,
}

/// A member's host-fault hold, rendered so an operator can see that Verify
/// is waiting on the executor host rather than on the candidate (#5020).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[cfg_attr(any(test, feature = "testing"), derive(Default))]
pub struct HostFaultView {
    /// The preflight findings — the missing tools, listed verbatim.
    pub findings: String,
}

/// A member's pending-decision hold, rendered for the outward mirror: the held
/// question's digest (the stable idempotency key the projected comment carries)
/// plus the human-readable decision a person reads where they already look. The
/// digest is what an adopting answer names; the prose is the question itself.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PendingDecisionView {
    /// The held [`crate::Question`]'s content-addressed digest — the exact
    /// digest an answer adopts, carried in the projection's stable metadata.
    pub question: Digest,
    /// The held stage.
    pub stage: StageId,
    /// The decision to be made, in plain language.
    pub prompt: String,
    /// The options considered, each with its consequence.
    pub options: Vec<String>,
    /// What the pending decision blocks.
    pub blocked: String,
}

/// The self-contained render input a commission replica needs (ADR-0199).
///
/// Wholly from the local commission and its journal view. A caller-supplied
/// GitHub title or body is not an input — the adapter renders this document
/// and never reads platform edits back.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CommissionProjection {
    /// The workpiece this commission is.
    pub workpiece: WorkpieceId,
    /// Digest of the stored intent statement.
    pub intent: Digest,
    /// Digest of the current scope revision, when one has been written.
    pub scope_revision: Option<Digest>,
    /// The approval's signer identity, when a signed approval is stored.
    pub approval_signer: Option<String>,
    /// Digest of the stored approval statement, when one exists.
    pub approval_digest: Option<Digest>,
    /// Lifecycle spelling (`open`, `cancelled`, `landed`).
    pub status: String,
    /// The issue number recorded from this projector's own create, if any.
    /// Frozen at enqueue time — later creates persist into the store row, not
    /// this snapshot. The reactor overlays the store's recorded number before
    /// projecting; `find_issue` is advisory crash-recovery only.
    #[serde(default)]
    pub recorded_issue: Option<u64>,
    /// The commission's own title — the first markdown heading of its intent —
    /// or empty when the intent carries no heading.
    ///
    /// Rendered into the replica's title so six freshly authored commissions
    /// are six distinguishable rows in an issue list rather than six copies of
    /// one constant. Trailing and defaulted, so an outbox row enqueued before
    /// this field existed still decodes and renders the untitled form.
    #[serde(default)]
    pub title: String,
}

/// The first markdown heading of an intent statement's words, or `None`.
///
/// Deliberately only a heading. An intent with no heading has no title, and the
/// closest available substitute — its first line of prose — is a sentence, not a
/// name; putting one in an issue title reads worse than the untitled form the
/// caller falls back to.
///
/// Capped at [`MAX_TITLE_CHARS`] on a character boundary, because the intent is
/// authored text and a heading long enough to be refused by the mirror would
/// stall the replica rather than shorten it.
#[must_use]
pub fn intent_title(words: &[u8]) -> Option<String> {
    let heading = from_utf8(words)
        .ok()?
        .lines()
        .find_map(|line| line.trim().strip_prefix('#').map(|rest| rest.trim_start_matches('#').trim()))
        .filter(|heading| !heading.is_empty())?;

    Some(heading.chars().take(MAX_TITLE_CHARS).collect())
}

/// How many characters of an intent heading reach the replica's title.
///
/// GitHub refuses an issue title past 256; this leaves room for the ` — status`
/// suffix beside it with margin, and a heading this long is a paragraph anyway.
pub const MAX_TITLE_CHARS: usize = 180;

/// A landing receipt together with the landed bloom's membership — the whole
/// render input a receipt projection needs.
///
/// [`LandingReceipt`] names a bloom, a previous base, and a new head, and no
/// membership, so a receipt drained from the outbox after a restart could not
/// reach the objects it belongs on. The reducer holds the landed bloom's
/// members at the moment it mints the receipt, so it carries them here rather
/// than leaving an adapter to read them back out of the store — the
/// self-contained-document rule this port is built on.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ProjectedReceipt {
    /// The receipt itself, unchanged.
    pub receipt: LandingReceipt,
    /// The landed bloom's members, in the sealed spec's canonical order.
    pub members: Vec<WorkpieceId>,
}

/// The outward mirror. The implementation does the I/O (writes comments on the
/// objects the repository already holds); this trait is the contract — every
/// projection carries internal ids and digests in stable metadata and is
/// rebuildable from the journal.
pub trait ProjectionBackend {
    /// The backend's error type.
    type Error;

    /// Reconcile the outward view to `view` — idempotent: reconciling the
    /// same document twice is a no-op. The document is the whole render
    /// input; the adapter never queries back into the store.
    ///
    /// # Errors
    /// Backend-defined — e.g. the projection surface is unreachable.
    fn reconcile_view(&self, view: &ViewDocument) -> Result<(), Self::Error>;

    /// Project a landing receipt outward, onto every object the landed bloom's
    /// membership reaches.
    ///
    /// # Errors
    /// Backend-defined — e.g. the projection surface is unreachable.
    fn project_receipt(&self, receipt: &ProjectedReceipt) -> Result<(), Self::Error>;

    /// Project one commission. `Some(number)` is an issue this projector owns
    /// and may retitle; `None` is a commission whose workpiece already names
    /// an object it must not own.
    ///
    /// # Errors
    /// Backend-defined — e.g. the projection surface is unreachable.
    fn project_commission(&self, projection: &CommissionProjection) -> Result<Option<u64>, Self::Error>;
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::{MAX_TITLE_CHARS, intent_title};

    #[test]
    fn the_first_heading_names_the_commission() {
        assert_eq!(
            intent_title(b"# Refuse a contradictory workpiece\n\nProblem\n").as_deref(),
            Some("Refuse a contradictory workpiece"),
        );
        assert_eq!(
            intent_title(b"## Problem\n\n# Later\n").as_deref(),
            Some("Problem"),
            "the first heading wins whatever its depth",
        );
    }

    #[test]
    fn an_intent_with_no_heading_has_no_title() {
        // The alternative — the first line of prose — is a sentence, not a name,
        // and reads worse in an issue list than the untitled fallback does.
        assert_eq!(intent_title(b"ship the commission store\n\nmore prose\n"), None);
        assert_eq!(intent_title(b"#\n#   \n"), None, "an empty heading is not a title");
        assert_eq!(intent_title(b""), None);
    }

    #[test]
    fn a_heading_longer_than_the_cap_is_truncated_on_a_character_boundary() {
        // The intent is authored text. A title past GitHub's own ceiling would
        // stall the replica rather than shorten it, and slicing bytes out of a
        // multi-byte heading would panic.
        let heading = "# ".to_string() + &"é".repeat(MAX_TITLE_CHARS * 2);
        let title = intent_title(heading.as_bytes()).expect("a long heading is still a heading");

        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
        assert!(title.chars().all(|character| character == 'é'));
    }

    #[test]
    fn intent_bytes_that_are_not_text_have_no_title() {
        assert_eq!(intent_title(&[0xff, 0xfe, b'#', b' ', b'x']), None);
    }
}
