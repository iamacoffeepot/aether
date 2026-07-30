//! ADR-0166 §5/§6 — `aether.component://camera` resolved against the real
//! link-time lineage inventory.
//!
//! The host and the trampoline are the first consumer of abbreviated
//! addressing: the host declares the root, the trampoline declares the one
//! instanced child beneath it, and the pair is what lets a caller write the
//! short form the ADR documents. The substrate's `AddressIndex` unit tests
//! feed hand-written facts, so nothing there observes what these two
//! `#[actor]` declarations actually submit — this test closes the loop
//! through the public `Registry::resolve_address` seam.

// Registering the trampoline's canonical mailbox is the point: the fold of the
// expanded path is the reference value under test, not a sibling-cap address.
#![allow(clippy::disallowed_methods)]

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_data::mailbox_id_from_path;
use aether_substrate::mail::registry::noop_handler;
use aether_substrate::testing::boot_authority;
use aether_substrate::{AddressResolutionError, Registry};

/// The canonical address a loaded component named `camera` registers under.
const CANONICAL: &str = "aether.component/aether.embedded:camera";

/// Tripwire: the expansion is computed from link-time `#[actor]` output — the
/// host's `RootEntry` plus singleton `NameEntry`, the trampoline's
/// `ChildEntry` plus instanced `TemplateEntry`. It drifts if either
/// declaration loses `root` / `child_of` / its cardinality, if the embedded
/// scope is renamed, or if the substrate stops reading one of those facts,
/// none of which the substrate's synthetic-fact tests can see.
#[test]
fn the_component_host_abbreviation_expands_through_the_linked_inventory() {
    // The prefix is read off the cap rather than spelled out, which both keeps
    // the namespace single-owner and references the cap so aether-component's
    // inventory submissions link into this test binary — the linker drops
    // unreferenced statics out of an rlib, and without that reference the
    // host's facts never reach `AddressIndex`.
    let short = format!("{}://camera", ComponentHostCapability::NAMESPACE);
    let registry = Registry::new();
    let id = mailbox_id_from_path(CANONICAL);
    registry
        .try_register_inbox_with_id(&boot_authority(), id, CANONICAL, noop_handler())
        .expect("canonical name is free");

    // The bare discriminator elides the child namespace: exactly one instanced
    // child namespace is declared beneath the host.
    let elided = registry.resolve_address(&short).expect("abbreviation resolves");
    assert_eq!(elided.mailbox_id, id);
    assert_eq!(elided.canonical_path, CANONICAL);

    // The explicit child segment names the same node.
    let explicit =
        registry.resolve_address("aether.component://aether.embedded:camera").expect("canonical child segment");
    assert_eq!(explicit, elided);

    // A child namespace is not itself a declared root, so it cannot anchor.
    assert_eq!(
        registry.resolve_address("aether.embedded://camera"),
        Err(AddressResolutionError::UnknownRoot { root: "aether.embedded".to_owned() })
    );
}
