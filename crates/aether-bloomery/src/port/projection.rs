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

use crate::digest::Digest;
use crate::ids::{BloomId, StageId, WorkpieceId};
use crate::reduce::BloomStatus;
use crate::values::{Evidence, LandingReceipt, ResolutionClaim, SpendQuiesce, Wedge};

/// The self-contained render input a reconcile pushes outward: the current
/// mainline, the last-reported observed head, and every projectable bloom,
/// each carrying its full membership. A pure projection of the journal —
/// idempotent and rebuildable after a deletion (ADR-0149 §The boundary). An
/// adapter renders entirely from this value and never queries back into the
/// store.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
}

/// One bloom's outward view: its sealed identity, lifecycle status, optional
/// successor, the full per-member render data, and — when the aggregate review
/// has parked the bloom — the bloom-scoped question an operator must settle.
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
/// — once the member is integrated — its resolution claim, and — while its
/// stage is held on a parked question — its pending-decision. A member is
/// integrated iff `resolution` is `Some` (the coarse per-member state the
/// reducer tracks) and held iff `pending_decision` is `Some`.
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
}

/// A member's host-fault hold, rendered so an operator can see that Verify
/// is waiting on the executor host rather than on the candidate (#5020).
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
}
