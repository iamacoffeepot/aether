//! Tests for [`super::super::mailbox::resolve`] — the name lookup walk
//! and the structured misses it reports.

use crate::mail::MailboxId;
use crate::mail::registry::{AddressResolutionError, Registry, noop_handler};
use crate::testing::boot_authority as auth;

#[test]
fn lookup_missing_returns_none() {
    let r = Registry::new();
    assert!(r.lookup("nope").is_none());
    assert!(r.entry(MailboxId(42)).is_none());
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
#[allow(
    clippy::disallowed_methods,
    reason = "the registry canonical-name test must construct the exact lineage-fold id that lookup derives"
)]
fn canonical_resolution_reports_the_registered_path_and_structured_misses() {
    let r = Registry::new();
    let canonical = "root/worker:camera";
    let id = aether_data::mailbox_id_from_path(canonical);
    r.try_register_inbox_with_id(&auth(), id, canonical, noop_handler()).unwrap();

    let resolved = r.resolve_address(canonical).expect("canonical mailbox is live");
    assert_eq!(resolved.mailbox_id, id);
    assert_eq!(resolved.canonical_path, canonical);
    assert_eq!(
        r.resolve_address("root/worker:missing"),
        Err(AddressResolutionError::NoLiveMailbox { canonical_path: "root/worker:missing".to_owned() })
    );

    let too_deep =
        (0..=aether_data::MAX_SCOPE_PATH_DEPTH).map(|index| format!("seg{index}")).collect::<Vec<_>>().join("/");
    assert_eq!(
        r.resolve_address(&too_deep),
        Err(AddressResolutionError::PathTooDeep { limit: aether_data::MAX_SCOPE_PATH_DEPTH })
    );
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
