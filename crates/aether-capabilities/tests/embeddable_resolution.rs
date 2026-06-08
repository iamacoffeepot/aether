//! ADR-0099 §5/§6 — embeddable resolution, the close of
//! iamacoffeepot/aether#1364.
//!
//! A peer that names a loaded component by type (`ctx.actor::<Camera>()`)
//! resolves it through the **embedding-host class** under the reserved
//! `aether.embedded` namespace, landing on the mailbox the host registered
//! it under instead of the bare `hash(NAMESPACE)` that misses. This drives
//! the mechanism through a fixture; wiring real shipped components (e.g.
//! `aether-camera`) to expose a peer-nameable marker is follow-up.

use aether_actor::{Actor, Embeddable, Singleton};
use aether_capabilities::resolve_embedded;
use aether_data::{mailbox_id_from_name, mailbox_id_from_path};

/// A fixture embeddable — stands in for a loaded wasm component. The
/// `#[derive(Embeddable)]` surface emits the `Singleton::resolve` override
/// that delegates to the embedding-host class; the author names no parent
/// and the macro writes no namespace literal (ADR-0099 §5 read-from-owner).
#[derive(Embeddable)]
struct FixtureComponent;

impl Actor for FixtureComponent {
    const NAMESPACE: &'static str = "test.embeddable.fixture";
}

#[test]
fn embeddable_resolves_through_host_class_not_bare_hash() {
    // An embeddable's address is absolute — rooted at the component host,
    // not relative to whoever addresses it — so its `resolve` ignores the
    // caller's lineage carry. A native peer and a wasm peer therefore
    // compute the same id; there is no transport branch in the fold below.
    assert_eq!(
        FixtureComponent::resolve(0),
        FixtureComponent::resolve(0xDEAD_BEEF),
        "embeddable address is caller-carry-independent",
    );

    // The derive delegates to the host-class composition.
    assert_eq!(
        FixtureComponent::resolve(0),
        resolve_embedded(FixtureComponent::NAMESPACE),
        "#[derive(Embeddable)] delegates to resolve_embedded",
    );

    // resolve_embedded folds the rendered lineage
    // `aether.component/aether.embedded:<name>` (ADR-0099 §4/§5) — exactly the
    // id the host registers the loaded component under, and exactly what the
    // by-name verb `loaded::<R>(name)` computes (it folds the same
    // `aether.embedded:<name>` node onto the same `aether.component` carry).
    assert_eq!(
        resolve_embedded(FixtureComponent::NAMESPACE),
        mailbox_id_from_path("aether.component/aether.embedded:test.embeddable.fixture"),
        "resolves to the registered [aether.component, aether.embedded:name] fold",
    );

    // The #1364 miss: the bare-NAMESPACE hash lands on a mailbox nothing is
    // registered under. The host-class fold lands on the registered one.
    assert_ne!(
        FixtureComponent::resolve(0),
        mailbox_id_from_name("test.embeddable.fixture"),
        "the host-class fold differs from the bare hash — the #1364 fix",
    );
}

/// A second name yields a distinct id under the same class — the per-instance
/// uniqueness ADR-0099 §6 relies on (`aether.embedded:cam2` differs from the
/// default-name instance), reached by the caller via `loaded::<R>("cam2")`.
#[test]
fn embeddable_non_default_name_folds_a_distinct_leaf() {
    assert_eq!(
        resolve_embedded("cam2"),
        mailbox_id_from_path("aether.component/aether.embedded:cam2"),
    );
    assert_ne!(
        resolve_embedded("cam2"),
        resolve_embedded(FixtureComponent::NAMESPACE),
        "distinct subnames resolve to distinct mailboxes under the shared class",
    );
}
