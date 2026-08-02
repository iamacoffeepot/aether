//! Tests for [`super::super::mailbox::staged`] — the overlay a batch
//! reads its own pending writes through before it commits.

use std::sync::Arc;
use std::time::Duration;

use crate::config::RegistryQueueCapacities;
use crate::mail::MailboxId;
use crate::mail::mailer::Mailer;
use crate::mail::registry::Registry;
use crate::mail::registry::effect::{EffectBatch, RegistryApplied, RegistryEffect, StartingCancellation};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::scheduler::WakeSink;
use crate::testing::boot_authority as auth;

use super::support::starting_token;

/// Tripwire: a route the batch has already staged for removal must read
/// back as absent to the effects behind it in the same batch, not as
/// whatever the committed table still holds.
///
/// The owner drains its queue into one `EffectBatch`, so a cancellation
/// and a fresh reservation of the same key routinely land together. If
/// `staged_route` let a staged `None` fall through to `Inner`, the
/// re-reservation would read the cancelled route as a live occupant and
/// fail the whole batch with a `NameConflict` — an actor that respawns
/// under its own name would never come back.
#[test]
fn cancel_and_rereserve_in_one_batch_read_through_the_staged_tombstone() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );

    let name = "staged-tombstone";
    let id = MailboxId::from_name(name);
    let reserved = registry.submit(EffectBatch::new(vec![RegistryEffect::reserve_named(name.to_owned())])).unwrap();
    owner.run_once();
    let first_token = starting_token(&reserved.wait_timeout(Duration::from_millis(100)).unwrap().unwrap());

    let rebirth = registry
        .submit(EffectBatch::new(vec![
            RegistryEffect::CancelStarting { id, token: first_token },
            RegistryEffect::reserve_with_id(id, name.to_owned()),
        ]))
        .unwrap();
    owner.run_once();

    let applied = rebirth.wait_timeout(Duration::from_millis(100)).unwrap().expect("the tombstone frees the key");
    let [
        RegistryApplied::StartingCancellation(cancellation),
        RegistryApplied::Starting { id: reserved_id, token: second_token },
    ] = applied.as_slice()
    else {
        panic!("expected a cancellation followed by a fresh reservation: {applied:?}")
    };
    assert_eq!(*cancellation, StartingCancellation::Cancelled(id));
    assert_eq!(*reserved_id, id);
    assert_ne!(*second_token, first_token, "the re-reservation stands on its own activation token");

    assert_eq!(registry.lookup(name), Some(id), "the committed table carries the batch's last write");
    assert!(registry.entry(id).is_none(), "the surviving reservation is still Starting, not live");
}
