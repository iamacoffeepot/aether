//! What one buffered send records in the outbound window until the flush
//! turns it back into routed mail, plus the two non-mail work items a handler
//! can stage alongside it.

use crate::mail::registry::effect::{PreparedSpawnCommit, RegistryBatch, RegistryBatchResult};
use crate::mail::ring::MailLoc;
use crate::mail::{MailId, MailRef, MailboxId, Source};

/// Where a buffered mail's payload lives until flush (2c,
/// iamacoffeepot/aether#1110).
pub(super) enum PendingPayload {
    /// Written into the actor's ring in place at send time; carries the
    /// location to mint a [`MailRef::InRing`] from at flush.
    InRing(MailLoc),
    /// The copy-out fallback when the ring could not take the mail
    /// (transiently full, or a payload larger than the ring).
    Owned(Vec<u8>),
    /// An already-owned component payload admitted while staged activation
    /// holds guest-authored mail. Unlike native sends, the wasm host function
    /// has already transferred ownership of its `Vec`, so retaining the
    /// resulting `MailRef` avoids copying it into the native actor ring.
    #[cfg(feature = "wasm")]
    Prebuilt(MailRef),
}

/// One outbound mail a handler buffered, pending flush. A native payload is
/// already in the ring (`InRing`) or copied out (`Owned`); a component payload
/// is retained as its prebuilt [`MailRef`]. The rest is route metadata
/// (correlation-derived `reply_to`/`mail_id`, inherited lineage) the flush
/// stamps onto the [`Mail`] it builds.
pub(super) struct PendingMail {
    pub(super) recipient: u64,
    pub(super) kind: u64,
    pub(super) payload: PendingPayload,
    pub(super) count: u32,
    pub(super) reply_to: Source,
    pub(super) mail_id: MailId,
    pub(super) root: MailId,
    pub(super) parent_mail: Option<MailId>,
}

/// One guest-authored mail the staged activation hold admitted, paired with
/// the canonical origin its direct component dispatch supplies to an inbox or
/// inline handler (ADR-0165). Recorded beside the window rather than on every
/// [`PendingMail`] because the origin of an ordinary native send is always the
/// binding's own mailbox: the native window keeps the exact `Vec<Mail>` shape
/// [`BlobProducer::flush`](super::blob::work::BlobProducer::flush) consumes, and
/// no flush pays a per-mail route tag for a case only a wasm trampoline reaches
/// (iamacoffeepot/aether#4178).
#[derive(Clone, Copy)]
pub(super) struct ComponentOrigin {
    pub(super) mail_id: MailId,
    pub(super) sender: MailboxId,
}

/// The canonical origin a released mail dispatches under, or `None` for
/// ordinary native mail. `origins` is empty on every steady-state flush, so
/// the lookup is a length check there and only ever walks the handful of sends
/// one guest `wire` produced.
pub(super) fn component_origin(origins: &[ComponentOrigin], mail_id: MailId) -> Option<MailboxId> {
    origins.iter().find(|origin| origin.mail_id == mail_id).map(|origin| origin.sender)
}

pub(super) struct PendingBirthWork {
    pub(super) after_mail: usize,
    pub(super) recipient: MailboxId,
    pub(super) commit: PreparedSpawnCommit,
}

pub(super) struct PendingOwnerBatchWork {
    pub(super) batch: RegistryBatch,
    pub(super) completion: super::offload::blocking::DeferredCompletion<RegistryBatchResult>,
}
