//! Failure modes for native actor spawning.
//!
//! Which of them a caller can see synchronously is the whole distinction
//! between the two builders: staging returns local validation, parent
//! reservation, and initialization failures on the spot, while the global
//! facts — namespace ownership, tombstones, routes, storage, owner
//! decisions — are authoritative only at owner time and arrive later on the
//! birth's own completion.

use std::time::Duration;

use aether_actor::NamespaceError;
use aether_data::ActorPathError;

use crate::chassis::error::BootError;
use crate::mail::registry::NativeHoldRefusal;

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
    /// [`NamespaceError`]. A guest birth's namespace is held to the same
    /// segment grammar and refused the same way.
    SubnameInvalid(NamespaceError),
    /// The child's composed canonical name — the parent's proven path, then
    /// `{A::NAMESPACE}:{subname}` — is not an ADR-0166 actor path: in practice a
    /// lineage deeper than `MAX_SCOPE_PATH_DEPTH` or longer than
    /// `MAX_SCOPE_PATH_BYTES`. Reported at staging, before `A::init`.
    PathInvalid(ActorPathError),
    /// The publication table refused `A` its namespace (ADR-0241 §3):
    /// another type sharing `A::NAMESPACE` was born first in this engine, or
    /// `A` is not among the types linked there.
    NativeHold(NativeHoldRefusal),
    /// A guest birth names a namespace the publication table does not bind
    /// to the birth's module (ADR-0241 §3): it is unpublished, native, or
    /// held by another module. Decided by the registry owner, so it arrives
    /// on the birth's completion.
    GuestNotPublished { namespace: String },
    /// A guest birth asked for a placement ADR-0241 §5 does not name: a
    /// keyless child, `parent/NS`. Reported at staging, before `H::init`.
    GuestPlacement,
    /// The full name was previously live and has been retired. Names
    /// don't recycle within a substrate's lifetime (ADR-0079 §Drop /
    /// lifecycle); pick a different subname.
    SubnameRetired { full_name: String },
    /// The full name is currently bound to a live mailbox.
    SubnameInUse { full_name: String },
    /// `A::init` returned an error. The actor's partial state dropped
    /// before this returns; no dispatcher thread was spawned.
    InitFailed(BootError),
    /// `A::wire` returned an error (ADR-0247 rule 3). The hook was entered,
    /// so the actor ran its `unwire` and closed before this was reported;
    /// it never went live, nothing it sent left, and its name is free for
    /// another attempt unless the birth published the name before `wire`
    /// (the pre-seal direct commit does, and that name is spent).
    WireFailed(BootError),
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
    BirthWedged { full_name: String, waited: Duration },
}
