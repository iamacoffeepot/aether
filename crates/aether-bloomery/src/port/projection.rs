//! The projection port (ADR-0149 §The boundary): push a self-contained view
//! document outward. The projection's direction is *outward* — Bloomery's
//! internals are the truth and the adapter maintains a shadow copy; a
//! projection never drives the reducer.
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

use crate::digest::Digest;
use crate::ids::{BloomId, WorkpieceId};
use crate::reduce::BloomStatus;
use crate::values::{Evidence, LandingReceipt, ResolutionClaim};

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
}

/// One member's outward view: the admitted workpiece, the exact scope
/// revision the bloom pinned, the approval evidence bound to that revision,
/// and — once the member is integrated — its resolution claim. A member is
/// integrated iff `resolution` is `Some` (the coarse per-member state the
/// reducer tracks).
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
}

/// The outward mirror. The implementation does the I/O (writes issues,
/// checks, comments); this trait is the contract — every projection carries
/// internal ids and digests in stable metadata and is rebuildable from the
/// journal.
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

    /// Project a landing receipt outward.
    ///
    /// # Errors
    /// Backend-defined — e.g. the projection surface is unreachable.
    fn project_receipt(&self, receipt: &LandingReceipt) -> Result<(), Self::Error>;
}
