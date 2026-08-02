//! Tests for [`super::super::mailbox::publish`] — which writes advance a
//! published generation and which publish nothing at all.

use crate::mail::registry::{Registry, noop_handler};
use crate::mail::{KindId, MailboxId};
use crate::testing::boot_authority as auth;

#[test]
fn route_generations_advance_only_for_successful_mutations() {
    let r = Registry::new();
    let kind = KindId(0);
    let initial = r.route_lookup(kind, MailboxId::NONE).generation();

    assert!(r.try_register_inbox(&auth(), "aether.chassis", noop_handler()).is_err());
    assert_eq!(r.route_lookup(kind, MailboxId::NONE).generation(), initial);

    let id = r.try_register_inbox(&auth(), "generation", noop_handler()).expect("fresh route");
    let inserted = r.route_lookup(kind, id).generation();
    assert!(inserted > initial);

    assert!(r.try_register_inbox(&auth(), "generation", noop_handler()).is_err());
    assert_eq!(r.route_lookup(kind, id).generation(), inserted);

    r.drop_mailbox(&auth(), id).expect("live route drops");
    let dropped = r.route_lookup(kind, id).generation();
    assert!(dropped > inserted);

    assert!(r.drop_mailbox(&auth(), id).is_err());
    assert_eq!(r.route_lookup(kind, id).generation(), dropped);

    r.try_register_inbox(&auth(), "generation", noop_handler()).expect("dropped route re-registers");
    let reregistered = r.route_lookup(kind, id).generation();
    assert!(reregistered > dropped);

    assert!(r.remove_closure(&auth(), id));
    let removed = r.route_lookup(kind, id).generation();
    assert!(removed > reregistered);
    assert!(r.entry(id).is_none());

    assert!(!r.remove_closure(&auth(), id));
    assert_eq!(r.route_lookup(kind, id).generation(), removed);
}

#[test]
fn kind_publication_advances_only_for_new_definitions() {
    let r = Registry::new();
    assert_eq!(r.kind_generation(), 0);

    let first = r.register_kind(&auth(), "aether.tick");
    assert_eq!(r.kind_generation(), 1);
    assert_eq!(r.kind_id("aether.tick"), Some(first));

    assert_eq!(r.register_kind(&auth(), "aether.tick"), first);
    assert_eq!(r.kind_generation(), 1, "idempotent registration must not fabricate a generation");

    let second = r.register_kind(&auth(), "aether.key");
    assert_eq!(r.kind_generation(), 2);
    assert_eq!(r.kind_name(second).as_deref(), Some("aether.key"));
    assert_eq!(r.list_kind_descriptors().len(), 2);
}
