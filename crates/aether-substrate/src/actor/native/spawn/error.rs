//! Failure modes for native actor spawning.
//!
//! Which of them a caller can see synchronously is the whole distinction
//! between the two builders: staging returns local validation, parent
//! reservation, and initialization failures on the spot, while the global
//! facts — namespace ownership, tombstones, routes, storage, owner
//! decisions — are authoritative only at owner time and arrive later on the
//! birth's own completion.

use std::any::TypeId;
use std::time::Duration;

use aether_actor::NamespaceError;

use crate::chassis::error::BootError;
use crate::mail::MailboxId;

/// Failure modes for native actor spawning.
///
/// [`HandlerSpawnBuilder::stage`](super::HandlerSpawnBuilder::stage) and [`HandlerSpawnBuilder::stage_with`](super::HandlerSpawnBuilder::stage_with)
/// return local validation, parent-reservation, and initialization failures
/// synchronously. Global namespace, tombstone, route, storage, and owner
/// decisions are authoritative only when the registry owner applies the staged
/// birth; those failures arrive later on the [`SpawnOutcome::result`](super::SpawnOutcome::result) of the
/// matching ADR-0093 `TaskDone<SpawnOutcome, _>`. The transitional eager
/// [`SpawnBuilder::finish`](super::SpawnBuilder::finish) path returns all of its failures directly.
#[derive(Debug)]
pub enum SpawnError {
    /// Subname is empty, contains `:`, has control / whitespace
    /// chars, or exceeds the byte cap. See
    /// [`NamespaceError`].
    SubnameInvalid(NamespaceError),
    /// `A::NAMESPACE` is already owned by a different `TypeId`. Trips
    /// when an `Instanced` type tries to spawn under a namespace a
    /// `Singleton` already owns (or vice versa). ADR-0079 unique-owner
    /// invariant.
    NamespaceOwnedByOtherType { namespace: &'static str, owning_type: TypeId },
    /// The full name was previously live and has been retired. Names
    /// don't recycle within a substrate's lifetime (ADR-0079 §Drop /
    /// lifecycle); pick a different subname.
    SubnameRetired { full_name: String },
    /// The full name is currently bound to a live mailbox.
    SubnameInUse { full_name: String },
    /// `A::init` returned an error. The actor's partial state dropped
    /// before this returns; no dispatcher thread was spawned.
    InitFailed(BootError),
    /// The registry owner closed before it could authoritatively apply the
    /// staged birth.
    OwnerClosed,
    /// Storage or cost reservation rejected the prepared activation.
    ActivationRejected,
    /// A post-seal external birth was accepted by the registry owner and then
    /// nothing decided it within the spawn path's patience budget (30 s).
    /// Reachable only from a wedged worker pool — the activation runs at its
    /// scheduler home, so a pool that never schedules it leaves the caller
    /// with no answer. Reported rather than fatal: the caller is an external
    /// thread that can log it, retry, or tear the chassis down.
    BirthWedged { mailbox_id: MailboxId, waited: Duration },
}
