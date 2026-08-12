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
use crate::values::{Evidence, LandingReceipt, ResolutionClaim, Wedge};

/// The self-contained render input a reconcile pushes outward: the current
/// mainline and every projectable bloom, each carrying its full membership.
/// A pure projection of the journal — idempotent and rebuildable after a
/// deletion (ADR-0149 §The boundary). An adapter renders entirely from this
/// value and never queries back into the store.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ViewDocument {
    /// The current mainline head.
    pub mainline: Digest,
    /// The blooms to mirror, each self-describing.
    pub blooms: Vec<BloomView>,
}

/// One bloom's outward view: its sealed identity, lifecycle status, optional
/// successor, and the full per-member render data.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
}

/// A bloom's landing-gate standing, rendered when its landing has been refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
}

/// A member's pending-decision hold, rendered for the outward mirror: the held
/// question's digest (the stable idempotency key the projected comment carries)
/// plus the human-readable decision a person reads where they already look. The
/// digest is what an adopting answer names; the prose is the question itself.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
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
