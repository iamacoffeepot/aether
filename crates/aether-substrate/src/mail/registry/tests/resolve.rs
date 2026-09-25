//! Tests for [`super::super::mailbox::resolve`] — the name lookup walk
//! and the structured misses it reports.

use aether_data::tagged_id::{Tag, with_tag};
use aether_data::{ActorId, ActorPath, MAILBOX_DOMAIN, fnv1a_64_prefixed, fold_lineage};

use crate::mail::MailboxId;
use crate::mail::registry::{AddressResolutionError, Registry, canonical_mailbox_id, lineage_mailbox_id, noop_handler};
use crate::testing::boot_authority as auth;

#[test]
fn lookup_missing_returns_none() {
    let r = Registry::new();
    assert!(r.lookup("nope").is_none());
    assert!(r.entry_at(MailboxId(42)).is_none());
}

#[test]
fn lookup_over_depth_scope_path_is_resolution_miss() {
    let r = Registry::new();
    // One segment past `MAX_SCOPE_PATH_DEPTH`: rejected before the fold.
    let name = (0..=aether_data::MAX_SCOPE_PATH_DEPTH).map(|i| format!("seg{i}")).collect::<Vec<_>>().join("/");
    assert!(r.lookup(&name).is_none());
}

#[test]
fn lookup_over_bytes_scope_path_is_resolution_miss() {
    let r = Registry::new();
    // Single segment longer than the byte cap (depth stays 1).
    let name = "a".repeat(aether_data::MAX_SCOPE_PATH_BYTES + 1);
    assert!(r.lookup(&name).is_none());
}

#[test]
fn canonical_resolution_reports_the_registered_path_and_structured_misses() {
    let r = Registry::new();
    let canonical = "root/worker:camera";
    let id = lineage_mailbox_id(canonical);
    r.try_register_inbox_with_id(&auth(), id, canonical, noop_handler()).unwrap();

    let path = |text| ActorPath::new(text).expect("fixture is a well-formed actor path");
    let resolved = r.resolve_address(&path(canonical)).expect("canonical mailbox is live");
    assert_eq!(resolved.mailbox_id, id);
    assert_eq!(resolved.canonical_path, canonical);
    assert_eq!(
        r.resolve_address(&path("root/worker:missing")),
        Err(AddressResolutionError::NoLiveMailbox { canonical_path: "root/worker:missing".to_owned() })
    );
}

#[test]
fn lineage_fold_is_the_node_chain_and_meets_the_canonical_id_at_depth_one() {
    // Tripwire: lookup by path meets registration by name only while the
    // depth-1 fold equals the id a by-name registration takes, and a nested
    // path must fold node by node rather than hash the joined string.
    for name in ["aether.component", "aether.embedded:camera"] {
        assert_eq!(lineage_mailbox_id(name).0, canonical_mailbox_id(name).0, "{name}");
    }

    let path = "root/scope:7/leaf";
    let chain = fold_lineage(
        fold_lineage(ActorId::singleton("root").0, ActorId::instanced("scope", "7")),
        ActorId::singleton("leaf"),
    );
    assert_eq!(lineage_mailbox_id(path).0, with_tag(Tag::Mailbox, chain));
    assert_ne!(lineage_mailbox_id(path).0, with_tag(Tag::Mailbox, fnv1a_64_prefixed(MAILBOX_DOMAIN, path.as_bytes())));
}

#[test]
fn mailbox_name_reverse_lookup() {
    let r = Registry::new();
    let a = r.register_inbox(&auth(), "physics", noop_handler());
    let b = r.register_inbox(&auth(), "graphics", noop_handler());
    assert_eq!(r.mailbox_name(a).as_deref(), Some("physics"));
    assert_eq!(r.mailbox_name(b).as_deref(), Some("graphics"));
    assert!(r.mailbox_name(MailboxId(999)).is_none());
}
