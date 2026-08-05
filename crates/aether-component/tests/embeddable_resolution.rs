//! ADR-0099 §5/§6, ADR-0119 — embeddable resolution, the close of
//! iamacoffeepot/aether#1364.
//!
//! A loaded component resolves under the reserved `aether.embedded` scope.
//! The [`Embedded`] resolver folds the `aether.embedded:<NAMESPACE>` node onto
//! the runtime parent mailbox selected by
//! [`CallerScoped`](aether_actor::CallerScoped). The same tagged mailbox id is
//! a sufficient routing seed for default, named, nested, and re-parented peer
//! resolution; no raw carry or lookup is needed. The explicit by-name verb
//! [`resolve_embedded`] still supplies the root component-host mailbox for
//! callers that deliberately address that host.

// Asserts the host-class fold differs from the bare-NAMESPACE hash, and stands
// in the `aether.component` carry by name — the primitive yields the reference
// id under test, not a sibling-cap address.
#![allow(clippy::disallowed_methods)]

use aether_actor::wasm::NO_INBOUND_SOURCE;
use aether_actor::wasm::inline::Registry;
use aether_actor::{Addressable, CallerScope, CallerScoped, Embedded, Manual, Resolve, WasmCtx};
use aether_component::{PeerCtxExt, resolve_embedded};
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

#[test]
fn embedded_peer_resolution_follows_nested_and_reparented_parents() {
    assert_eq!(<Embedded as CallerScoped>::SCOPE, CallerScope::Parent);

    let parent_a = mailbox_id_from_path("test.root/test.composite:a");
    let parent_b = mailbox_id_from_path("test.root/test.composite:b");
    let caller_a = Embedded::resolve(parent_a.0, "caller", ());
    let caller_b = Embedded::resolve(parent_b.0, "caller", ());
    let registry_a = Registry::new();
    registry_a.set_self_id(caller_a.0);
    registry_a.set_parent_id(parent_a.0);
    let registry_b = Registry::new();
    registry_b.set_self_id(caller_b.0);
    registry_b.set_parent_id(parent_b.0);
    let ctx_a: WasmCtx<'_, Manual> = WasmCtx::__new(caller_a.0, &registry_a, NO_INBOUND_SOURCE);
    let ctx_b: WasmCtx<'_, Manual> = WasmCtx::__new(caller_b.0, &registry_b, NO_INBOUND_SOURCE);

    assert_eq!(ctx_a.peer::<FixtureComponent>().mailbox_id(), FixtureComponent::resolve(parent_a.0, ()));
    assert_eq!(ctx_b.peer::<FixtureComponent>().mailbox_id(), FixtureComponent::resolve(parent_b.0, ()));
    assert_eq!(
        ctx_a.peer_named::<FixtureComponent>("fixture-3").mailbox_id(),
        Embedded::resolve(parent_a.0, "fixture-3", ()),
    );
    assert_eq!(
        ctx_b.peer_named::<FixtureComponent>("fixture-3").mailbox_id(),
        Embedded::resolve(parent_b.0, "fixture-3", ()),
    );
    assert_ne!(ctx_a.peer::<FixtureComponent>().mailbox_id(), ctx_b.peer::<FixtureComponent>().mailbox_id());
}
