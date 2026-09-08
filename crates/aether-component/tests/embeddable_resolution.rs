//! ADR-0099 §5/§6, ADR-0119 — embeddable resolution, the close of
//! iamacoffeepot/aether#1364.
//!
//! A loaded component resolves under the reserved `aether.embedded` scope.
//! The [`Embedded`] resolver folds the `aether.embedded:<NAMESPACE>` node onto
//! the runtime parent mailbox selected by
//! [`CallerScoped`](aether_actor::CallerScoped), which is how a co-hosted
//! caller reaches it by bare type. The by-name [`resolve_embedded`] supplies
//! the root component-host carry instead, for a caller with no such ctx to
//! resolve from — and this test pins the two to the same address.

// Asserts the host-class fold differs from the bare-NAMESPACE hash, and stands
// in the `aether.component` carry by name — the primitive yields the reference
// id under test, not a sibling-cap address.
#![allow(clippy::disallowed_methods)]

use aether_actor::{Addressable, Embedded};
use aether_component::resolve_embedded;
use aether_data::{mailbox_id_from_name, mailbox_id_from_path};

/// A fixture embeddable — stands in for a loaded wasm component, selecting the
/// [`Embedded`] resolver (ADR-0119) that `#[actor]` emits for real components.
/// `Embedded` is keyless, so the fixture is a singleton reached by type.
struct FixtureComponent;

impl Addressable for FixtureComponent {
    const NAMESPACE: &'static str = "test.embeddable.fixture";
    type Resolver = Embedded;
}

#[test]
fn embeddable_resolves_under_the_host_class() {
    // The `aether.component` host's carry — its depth-1 mailbox id (ADR-0099
    // §3), what the trampoline folds embedded children onto. Equal to
    // `<ComponentHostCapability as Addressable>::resolve(0, ())` (a root
    // singleton), which is what `resolve_embedded` supplies internally.
    let host_carry = mailbox_id_from_name("aether.component").0;

    // ADR-0119: the `Embedded` resolver folds the `aether.embedded:<NAMESPACE>`
    // node onto the carry it is handed. Given the host carry it lands on exactly
    // what the by-name verb `resolve_embedded` computes, so by-type and by-name
    // addressing agree in the host context.
    assert_eq!(
        <FixtureComponent as Addressable>::resolve(host_carry, ()),
        resolve_embedded(FixtureComponent::NAMESPACE),
        "by-type Embedded resolve (host carry) == resolve_embedded",
    );

    // A different parent carry lands somewhere else entirely. Runtime peer
    // contexts deliberately select their injected parent rather than the
    // sender's current mailbox.
    assert_ne!(
        <FixtureComponent as Addressable>::resolve(0xDEAD_BEEF, ()),
        resolve_embedded(FixtureComponent::NAMESPACE),
        "a non-host carry folds to an address the host never registered",
    );

    // resolve_embedded folds the rendered lineage
    // `aether.component/aether.embedded:<name>` (ADR-0099 §4/§5) — exactly the
    // id the host registers the loaded component under.
    assert_eq!(
        resolve_embedded(FixtureComponent::NAMESPACE),
        mailbox_id_from_path("aether.component/aether.embedded:test.embeddable.fixture"),
        "resolves to the registered [aether.component, aether.embedded:name] fold",
    );

    // The #1364 miss: the bare-NAMESPACE hash lands where nothing is registered.
    assert_ne!(
        resolve_embedded(FixtureComponent::NAMESPACE),
        mailbox_id_from_name("test.embeddable.fixture"),
        "the host-class fold differs from the bare hash — the #1364 fix",
    );
}
