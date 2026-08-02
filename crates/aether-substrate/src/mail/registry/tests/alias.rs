//! Tests for [`super::super::mailbox::alias`] — logical inline-child
//! alias routes and the parent they follow.

use std::panic;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::RegistryQueueCapacities;
use crate::mail::mailer::Mailer;
use crate::mail::registry::effect::{EffectBatch, PreparedAliasRoute, RegistryEffect, RegistryEffectError};
use crate::mail::registry::owner::RegistryOwnerLease;
use crate::mail::registry::{MailboxEntry, Registry, noop_handler};
use crate::mail::{KindId, Mail, MailboxId};
use crate::scheduler::WakeSink;
use crate::testing::boot_authority as auth;

use super::support::{activation_barrier, prepared_test_spawn, starting_token};

/// Manual state-machine proof: an alias batch can land while its parent is
/// still Starting; alias-addressed mail joins that parent's parked tail and is
/// delivered only after the exact activation barrier promotes the parent.
/// Scheduler recruitment is covered separately by the component integration
/// suite.
#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the registry alias test intentionally folds the canonical path whose prepared id it validates"
)]
fn manual_owner_cycles_alias_to_starting_parent_parks_until_parent_promotes() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let deliveries = Arc::new(Mutex::new(Vec::new()));
    let scheduled = Arc::new(AtomicUsize::new(0));
    let parent_name = "alias-starting-parent";
    let parent_id = MailboxId::from_name(parent_name);
    let (_, _, _, birth) =
        prepared_test_spawn(&registry, &mailer, parent_name, Arc::clone(&deliveries), scheduled, vec![parent_id], 1);
    let birth_completion = registry.submit(EffectBatch::new(vec![birth])).unwrap();
    owner.run_once();
    let token = starting_token(&birth_completion.wait_timeout(Duration::from_millis(100)).unwrap().unwrap());
    let starting_inventory_generation = registry.inventory().mailbox_generation;

    let alias_name = format!("{parent_name}/aether.embedded:widget");
    let alias_id = aether_data::mailbox_id_from_path(&alias_name);
    let alias_completion = registry
        .submit(EffectBatch::new(vec![RegistryEffect::PublishAlias(PreparedAliasRoute::new(
            alias_id,
            alias_name.clone(),
            parent_id,
        ))]))
        .unwrap();
    owner.run_once();
    assert!(alias_completion.wait_timeout(Duration::from_millis(100)).unwrap().is_ok());
    assert!(registry.route_lookup(KindId(7), alias_id).is_starting());
    assert!(registry.entry(alias_id).is_none(), "compatibility projection does not expose the Starting parent");
    assert!(
        registry.inventory().mailboxes.iter().all(|descriptor| descriptor.id != alias_id),
        "an alias following a Starting parent is not announced as live"
    );
    assert_eq!(
        registry.inventory().mailbox_generation,
        starting_inventory_generation,
        "publishing an alias to a Starting parent emits no public inventory change"
    );

    mailer.push(Mail::new(alias_id, KindId(7), vec![2], 1));
    owner.run_once();
    assert!(deliveries.lock().unwrap().is_empty(), "alias mail remains parked before the barrier");

    mailer.push(activation_barrier(parent_id, token, 1));
    owner.run_once();
    assert_eq!(*deliveries.lock().unwrap(), [1, 2], "bootstrap precedes the alias-addressed parked tail");
    assert!(matches!(registry.entry(alias_id), Some(MailboxEntry::Inbox { .. })));
    assert_eq!(registry.lookup(&alias_name), Some(alias_id));
    assert!(
        registry.inventory().mailboxes.iter().any(|descriptor| descriptor.id == alias_id),
        "parent promotion announces its logical aliases in the same inventory publication"
    );
}

#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "the registry alias conflict test intentionally folds the canonical path whose prepared id it validates"
)]
fn logical_alias_repeat_is_idempotent_and_conflicting_target_is_rejected() {
    let registry = Arc::new(Registry::new());
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
    let first_handler = noop_handler();
    let first_parent = registry.register_inbox(&auth(), "alias-parent-first", Arc::clone(&first_handler));
    let second_parent = registry.register_inbox(&auth(), "alias-parent-second", noop_handler());
    let owner = RegistryOwnerLease::attach(
        auth(),
        &registry,
        &mailer,
        WakeSink::detached(),
        RegistryQueueCapacities::default(),
    );
    let alias_name = "alias-parent-first/aether.embedded:widget";
    let alias_id = aether_data::mailbox_id_from_path(alias_name);
    let submit = |target_parent| {
        registry
            .submit(EffectBatch::new(vec![RegistryEffect::PublishAlias(PreparedAliasRoute::new(
                alias_id,
                alias_name,
                target_parent,
            ))]))
            .unwrap()
    };

    let first = submit(first_parent);
    owner.run_once();
    assert!(first.wait_timeout(Duration::from_millis(100)).unwrap().is_ok());
    let published_generation = registry.inventory().mailbox_generation;

    let repeat = submit(first_parent);
    owner.run_once();
    assert!(repeat.wait_timeout(Duration::from_millis(100)).unwrap().is_ok());
    assert_eq!(registry.inventory().mailbox_generation, published_generation, "an exact repeat publishes nothing");

    let conflict = submit(second_parent);
    owner.run_once();
    assert!(matches!(conflict.wait_timeout(Duration::from_millis(100)).unwrap(), Err(RegistryEffectError::Name(_))));
    let Some(MailboxEntry::Inbox { handler, .. }) = registry.entry(alias_id) else {
        panic!("accepted alias still projects its target inbox")
    };
    assert!(Arc::ptr_eq(&handler, &first_handler), "rejection leaves the first logical target unchanged");
}
