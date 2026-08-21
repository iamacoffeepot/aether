//! The pre-bloom scoping-run drain (ADR-0208, #5304).
//!
//! Modelled directly on `drain_and_dispatch_aggregate_verify` — same
//! ack-prefix, park, and backoff semantics — with the three bloom-shaped steps
//! removed, each for a reason that is a property of the run rather than an
//! optimisation:
//!
//! - **No `bloom_still_live`.** A scoping run has no membership by
//!   construction, so that check would retire every entry undispatched.
//! - **No `hold_overlapping_reconcile`.** That hold is about two members
//!   writing one tree inside one bloom; there is no bloom and no sibling.
//! - **No `overlay_member_advisory` and no `ModelOverride`.** The advisory
//!   returns at its first line for any command that is not
//!   `construct.implement`, and a `ModelOverride` is sealed into a *bloom's*
//!   `ConfigRegistry` — there is none to resolve against. The seat rides the
//!   payload unread: whatever the compiled line calibrates is what dispatches.
//!
//! # The reserved bloom
//!
//! `outstanding_orders.bloom` is `NOT NULL`, and every dispatch, deadline
//! sweep, and strand-recovery path reads that table. Making the column nullable
//! is a versioned rebuild of the one table the whole dispatch surface depends
//! on; supplying a reserved digest keeps it, its deadline sweep, and its strand
//! recovery working unchanged. Only two reads key `outstanding_orders` by bloom
//! — `list_bloom_dispatch_live` (called only from the member drain's
//! reconcile-overlap hold) and `lookup_named_dispatch` — and a scope run enters
//! neither. The precedent is `WorkpieceId::COMPOSITION`: a reserved id inside
//! an existing identity space, rather than a second space.

use aether_bloomery::{BloomId, ConfigRegistry, Digest, Topic, WorkHandle, control::ScopeDispatchPayload};
use aether_data::wire::from_bytes;

use crate::bloomery::ExecutorShell;
use crate::bloomery::intake::{DispatchRecord, dispatch_and_record, dispatch_nonce};
use crate::bloomery::outbox::TopicOutbox;
use crate::store::StoreBackend;

use super::transformation_has_subject;

/// The domain tag the reserved scope-run bloom digest is minted under. Its own
/// tag so it cannot collide with any other digest the estate mints.
const SCOPE_RUN_BLOOM_DOMAIN: &str = "aether.bloomery.scope_run_bloom";

/// The reserved digest a scoping run's order registry row names in place of a
/// bloom.
///
/// Not a bloom and never resolvable as one: nothing seals it, nothing claims
/// membership under it, and no view projects it. It exists so the `NOT NULL`
/// column has a truthful constant rather than a zeroed digest, which would read
/// to every later reader as an ordinary bloom nobody has heard of.
#[must_use]
pub(super) fn scope_run_bloom() -> Digest {
    Digest::of_domain_tagged(SCOPE_RUN_BLOOM_DOMAIN, b"scope-run")
}

/// Drain the scoping-run topic and submit each entry through the executor under
/// a pre-bloom order record, recording the nonce on the run's ledger.
///
/// Returns the newly-tracked handles, the highest contiguously-submitted outbox
/// sequence to ack, and the sequence of a transient submit failure that stopped
/// the drain (the shared backoff window's input) — the same triple every other
/// drain returns.
pub(super) fn drain_and_dispatch_scope(
    store: &mut dyn StoreBackend,
    executor: &ExecutorShell,
    now_unix_millis: u64,
) -> rusqlite::Result<(Vec<WorkHandle>, Option<u64>, Option<u64>)> {
    let entries = store.drain_topic(Topic::ScopeDispatch)?;
    let mut handles = Vec::new();
    let mut ack_through = None;
    let mut transient_failure = None;
    for entry in entries {
        let Ok(payload) = from_bytes::<ScopeDispatchPayload>(&entry.payload) else {
            tracing::warn!(
                target: "aether_chassis_bloomery::executor",
                sequence = entry.sequence,
                "scope-dispatch outbox entry did not decode; stopping the ack prefix to re-drain",
            );
            break;
        };
        if !transformation_has_subject(&payload.transformation.inputs, entry.sequence, "scope dispatch") {
            break;
        }

        // The subject is the displayed digest: the returning evidence binds to
        // it, and there is no candidate for it to fall back from.
        let record = DispatchRecord {
            nonce: dispatch_nonce(entry.sequence),
            bloom: BloomId(scope_run_bloom()),
            workpiece: payload.commission.clone(),
            profile: payload.profile,
            scope_revision: payload.subject,
            candidate: payload.subject,
            displayed_digest: payload.subject,
            stage: payload.stage,
            transformation: payload.transformation,
            // No sealed registry exists before a bloom does, and the scope
            // command is not `construct.implement`, so no overlay would read
            // this even if one were supplied.
            configs: ConfigRegistry::default(),
        };
        match dispatch_and_record(executor, store, &record, now_unix_millis) {
            Ok(handle) => {
                // After the order is recorded, never before: the ledger row
                // says "this run is in flight under this nonce", and a nonce
                // written for a submit that never happened would make the
                // intake walk back from an upload that cannot exist.
                store.record_scope_dispatch(&payload.commission.0, payload.ordinal, &record.nonce.0)?;
                handles.push(handle);
                ack_through = Some(entry.sequence);
            }
            Err(error) if error.is_permanent() => {
                // A permanent refusal never clears on retry, so the entry is
                // acked past and the run stops with no `dispatched` row — which
                // is what the termination rule reads as an attempt that never
                // reached a lane.
                tracing::error!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    commission = %payload.commission.0,
                    ordinal = payload.ordinal,
                    nonce = %record.nonce.0,
                    %error,
                    "scope-dispatch submit refused permanently; parking the entry instead of re-driving",
                );
                ack_through = Some(entry.sequence);
                break;
            }
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    sequence = entry.sequence,
                    %error,
                    "scope-dispatch submit/record failed; stopping the ack prefix to re-drive",
                );
                transient_failure = Some(entry.sequence);
                break;
            }
        }
    }
    Ok((handles, ack_through, transient_failure))
}

#[cfg(test)]
mod tests {
    use aether_bloomery::Digest;

    use super::scope_run_bloom;

    #[test]
    fn the_reserved_bloom_is_not_the_zero_digest() {
        // Tripwire: the registry's `bloom` column is `NOT NULL`, and the lazy
        // fill for "there is no bloom" is a zeroed digest — which reads to
        // every later reader as an ordinary bloom nobody has heard of, rather
        // than as "this order has no bloom by construction".
        assert_ne!(scope_run_bloom(), Digest::default());
    }
}
