//! ADR-0099 §5/§6, ADR-0119 — embeddable resolution, the close of
//! iamacoffeepot/aether#1364.
//!
//! A loaded component resolves under the reserved `aether.embedded` scope.
//! The [`Embedded`] resolver folds the `aether.embedded:<NAMESPACE>` node onto
//! the caller's carry (ADR-0119 — caller-relative); the by-name verb
//! [`resolve_embedded`] supplies the `aether.component` host carry, landing on
//! the mailbox the host registered the component under instead of the bare
//! `hash(NAMESPACE)` that misses.

// Asserts the host-class fold differs from the bare-NAMESPACE hash, and stands
// in the `aether.component` carry by name — the primitive yields the reference
// id under test, not a sibling-cap address.
#![allow(clippy::disallowed_methods)]

use aether_actor::{Addressable, Embedded};
use aether_capabilities::resolve_embedded;
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

    // ADR-0119: the `Embedded` resolver is caller-RELATIVE — it folds the
    // `aether.embedded:<NAMESPACE>` node onto the caller's carry. Given the host
    // carry it lands on exactly what the by-name verb `resolve_embedded`
    // computes, so by-type and by-name addressing agree in the host context.
    assert_eq!(
        <FixtureComponent as Addressable>::resolve(host_carry, ()),
        resolve_embedded(FixtureComponent::NAMESPACE),
        "by-type Embedded resolve (host carry) == resolve_embedded",
    );

    // A different caller carry resolves to a different mailbox — caller-relative
    // (the opposite of the old hardcoded-host override).
    assert_ne!(
        <FixtureComponent as Addressable>::resolve(0, ()),
        <FixtureComponent as Addressable>::resolve(0xDEAD_BEEF, ()),
        "Embedded folds onto the caller carry",
    );

    // resolve_embedded folds the rendered lineage
    // `aether.component/aether.embedded:<name>` (ADR-0099 §4/§5) — exactly the
    // id the host registers the loaded component under, and exactly what the
    // by-name verb `loaded::<R>(name)` computes.
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
