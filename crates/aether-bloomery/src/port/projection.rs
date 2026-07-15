//! The projection port (ADR-0149 §The boundary): push typed receipts
//! outward. The projection's direction is *outward* — Bloomery's internals
//! are the truth and the adapter maintains a shadow copy; a projection never
//! drives the reducer.

use alloc::vec::Vec;

use crate::digest::Digest;
use crate::ids::BloomId;
use crate::values::LandingReceipt;

/// The projectable view state a reconcile pushes outward: the live blooms and
/// the current mainline. Idempotent and rebuildable from the journal after a
/// deletion (ADR-0149 §The boundary).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProjectionState {
    /// The live blooms to mirror.
    pub blooms: Vec<BloomId>,
    /// The current mainline head.
    pub mainline: Digest,
}

/// The outward mirror. The implementation does the I/O (writes issues,
/// checks, comments); this trait is the contract — every projection carries
/// internal ids and digests in stable metadata and is rebuildable from the
/// journal.
pub trait ProjectionBackend {
    /// The backend's error type.
    type Error;

    /// Reconcile the outward view to `state` — idempotent: reconciling the
    /// same state twice is a no-op.
    ///
    /// # Errors
    /// Backend-defined — e.g. the projection surface is unreachable.
    fn reconcile_view(&self, state: &ProjectionState) -> Result<(), Self::Error>;

    /// Project a landing receipt outward.
    ///
    /// # Errors
    /// Backend-defined — e.g. the projection surface is unreachable.
    fn project_receipt(&self, receipt: &LandingReceipt) -> Result<(), Self::Error>;
}
