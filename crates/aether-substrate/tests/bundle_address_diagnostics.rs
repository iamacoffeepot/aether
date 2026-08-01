//! ADR-0166 §5 — the structured resolution diagnostic reaching the named-mail
//! bundle path (issue 4125).
//!
//! `resolve_bundle` is what `spawn_substrate(mails=…)` and
//! `capture_frame(mails=…)` resolve through. It used to call `Registry::lookup`,
//! which collapses every `AddressResolutionError` except the path caps to
//! `None` — so an *ambiguous* abbreviated address reported "unknown recipient"
//! with no candidates and no indication that the address was ambiguous rather
//! than absent. The ambiguity is only reachable from real linked inventory, so
//! the fixtures below declare it: two instanced children beneath one root make
//! a bare discriminator under that root ambiguous by construction.
//!
//! Lives in `aether-substrate`'s test tree for the same reason
//! `unsplit_lineage_abbreviation.rs` does: the macro expands to absolute
//! `::aether_substrate::*` paths, and the index that reads the result is owned
//! here.

// The fixtures register their own canonical mailboxes: the fold of an expanded
// path is the reference value under test, not a sibling-cap address.
#![allow(clippy::disallowed_methods)]

use aether_actor::{Addressable, actor};
use aether_data::{Kind, mailbox_id_from_path};
use aether_kinds::NamedMail;
use aether_substrate::Registry;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::helpers::resolve_bundle;
use aether_substrate::mail::registry::noop_handler;
use aether_substrate::testing::boot_authority;
use serde::{Deserialize, Serialize};

#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "test.bundle_diagnostics.poke")]
struct Poke {
    value: u64,
}

/// The anchor root the two instanced children hang beneath.
struct DiagnosticsRoot;

#[actor(singleton, root)]
impl NativeActor for DiagnosticsRoot {
    type Config = ();
    const NAMESPACE: &'static str = "test.bundle_diagnostics.root";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[allow(clippy::unused_self)] // actor handler ABI always receives state
    #[handler::single]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Poke) {}
}

/// First instanced child. On its own it would let a bare discriminator elide
/// the child segment; paired with [`SecondChild`] it makes that elision
/// ambiguous instead, which is the state under test.
struct FirstChild;

#[actor(instanced, child_of(DiagnosticsRoot))]
impl NativeActor for FirstChild {
    type Config = ();
    const NAMESPACE: &'static str = "test.bundle_diagnostics.first";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[allow(clippy::unused_self)] // actor handler ABI always receives state
    #[handler::single]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Poke) {}
}

/// Second instanced child beneath the same root — the other half of the
/// ambiguity.
struct SecondChild;

#[actor(instanced, child_of(DiagnosticsRoot))]
impl NativeActor for SecondChild {
    type Config = ();
    const NAMESPACE: &'static str = "test.bundle_diagnostics.second";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[allow(clippy::unused_self)] // actor handler ABI always receives state
    #[handler::single]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Poke) {}
}

fn register(registry: &Registry, canonical: &str) {
    let mailbox_id = mailbox_id_from_path(canonical);
    registry
        .try_register_inbox_with_id(&boot_authority(), mailbox_id, canonical, noop_handler())
        .expect("canonical name is free");
}

fn bundle(recipient: &str) -> Vec<NamedMail> {
    vec![NamedMail {
        recipient_name: recipient.to_owned(),
        kind_name: <Poke as Kind>::NAME.to_owned(),
        payload: Poke { value: 1 }.encode_into_bytes(),
        count: 1,
    }]
}

/// The three outcomes the bundle path must tell apart. Before #4125 the middle
/// one rendered identically to the last: `lookup` returned `None` either way,
/// so the candidate spellings were dropped and the message claimed the
/// recipient was unknown.
#[test]
fn bundle_resolution_distinguishes_ambiguous_from_absent_and_resolves_canonical() {
    let registry = Registry::new();
    registry.register_kind(&boot_authority(), <Poke as Kind>::NAME);
    let canonical = format!("{}/{}:one", DiagnosticsRoot::NAMESPACE, FirstChild::NAMESPACE);
    register(&registry, DiagnosticsRoot::NAMESPACE);
    register(&registry, &canonical);

    // Canonical input is unchanged — it never touched the abbreviation path.
    let resolved = resolve_bundle(&registry, &bundle(&canonical), "test bundle").expect("canonical recipient resolves");
    assert_eq!(resolved.len(), 1);

    // Ambiguous: two instanced children are declared beneath the root, so a
    // bare discriminator cannot pick one. The error must say so and list both
    // spellings that would disambiguate it.
    let ambiguous = format!("{}://one", DiagnosticsRoot::NAMESPACE);
    let error =
        resolve_bundle(&registry, &bundle(&ambiguous), "test bundle").expect_err("bare discriminator ambiguous");
    assert!(error.contains("ambiguous"), "the error names the ambiguity: {error}");
    assert!(error.contains(FirstChild::NAMESPACE), "the error lists the first candidate: {error}");
    assert!(error.contains(SecondChild::NAMESPACE), "the error lists the second candidate: {error}");

    // Absent still reports absence, explicitly and distinguishably.
    let absent = format!("{}/{}:missing", DiagnosticsRoot::NAMESPACE, FirstChild::NAMESPACE);
    let error = resolve_bundle(&registry, &bundle(&absent), "test bundle").expect_err("absent recipient");
    assert!(error.contains("no live mailbox"), "an absent recipient still reports absence: {error}");
    assert!(!error.contains("ambiguous"), "an absent recipient is not reported as ambiguous: {error}");
}
