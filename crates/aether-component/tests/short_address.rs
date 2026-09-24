//! ADR-0166 §5/§6 — `aether.component/:camera` resolved against the real
//! link-time lineage inventory.
//!
//! The host and the trampoline are the first consumer of short paths: the
//! host declares the root, the trampoline declares the one instanced child
//! beneath it, and the pair is what lets a caller write the short path the ADR
//! documents. The substrate's `AddressIndex` unit tests
//! feed hand-written facts, so nothing there observes what these two
//! `#[actor]` declarations actually submit — this test closes the loop
//! through the public `Registry::resolve_address` seam.

use aether_actor::Addressable;
use aether_component::ComponentHostCapability;
use aether_data::ActorPath;
use aether_substrate::mail::registry::noop_handler;
use aether_substrate::testing::registered_ref;
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
fn the_component_host_hole_expands_through_the_linked_inventory() {
    // The prefix is read off the cap rather than spelled out, which both keeps
    // the namespace single-owner and references the cap so aether-component's
    // inventory submissions link into this test binary — the linker drops
    // unreferenced statics out of an rlib, and without that reference the
    // host's facts never reach `AddressIndex`.
    let path = |text: &str| ActorPath::new(text).expect("fixture is a well-formed actor path");
    let short = format!("{}/:camera", ComponentHostCapability::NAMESPACE);
    let registry = Registry::new();
    registered_ref(&registry, CANONICAL, noop_handler());

    // The hole names the one instanced child namespace declared beneath the
    // host.
    let expanded = registry.resolve_address(&path(&short)).expect("short path resolves");
    assert_eq!(expanded.canonical_path, CANONICAL);

    // A child namespace is not itself a declared root, so it cannot anchor.
    assert_eq!(
        registry.resolve_address(&path("aether.embedded/:camera")),
        Err(AddressResolutionError::UnknownRoot { root: "aether.embedded".to_owned() })
    );
}
