//! ADR-0166 §4/§5 — the lineage facts an *un-split* `#[actor]` declaration
//! contributes to the link-time inventory, read back through the real
//! `Registry::resolve_address` seam.
//!
//! `#[actor]` emits a `RootEntry` / `ChildEntry` from `root` / `child_of(...)`
//! and a cardinality fact from `singleton` / `instanced`. `AddressIndex`
//! consumes the two together: it requires a cardinality fact for every
//! namespace a placement fact names, and excludes a namespace missing one from
//! the index. So the emissions are only correct jointly, and a macro gate that
//! can admit one without the other is the defect — not the presence of either
//! submission on its own, which the `#[actor]` expansion trivially guarantees.
//!
//! This lives in `aether-substrate`'s test tree for the same reason
//! `native_actor_macro.rs` does: the macro expands to absolute
//! `::aether_substrate::*` paths, and the index that reads the result is owned
//! here.

use aether_actor::{Addressable, actor};
use aether_data::ActorPath;
use aether_substrate::Registry;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::registry::noop_handler;
use aether_substrate::testing::registered_ref;

#[aether_data::kind(name = "test.unsplit_lineage.poke", copy, default, eq)]
struct Poke {
    value: u64,
}

/// Un-split root fixture — `type State = Self` (the shape ADR-0122 reserves for
/// test-only actors) plus the `root` placement, the combination that carries an
/// address-anchor claim without the split identity's authoring ceremony.
struct UnsplitRoot;

#[actor(singleton, root)]
impl NativeActor for UnsplitRoot {
    type Config = ();
    const NAMESPACE: &'static str = "test.unsplit_lineage.root";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[allow(clippy::unused_self)] // actor handler ABI always receives state
    #[handler::single]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Poke) {}
}

/// Un-split instanced child of the fixture root — the one instanced namespace
/// beneath it, which is what lets a hole name one of its instances.
struct UnsplitChild;

#[actor(instanced, child_of(UnsplitRoot))]
impl NativeActor for UnsplitChild {
    type Config = ();
    const NAMESPACE: &'static str = "test.unsplit_lineage.child";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[allow(clippy::unused_self)] // actor handler ABI always receives state
    #[handler::single]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Poke) {}
}

/// Tripwire: the short path below is expanded from link-time `#[actor]`
/// output, and it resolves only when the *cardinality* half of both
/// declarations is present alongside its placement half: the root's singleton
/// fact to anchor the path, and the child's instanced fact to fill the hole.
/// Reinstating any gate that emits a `RootEntry` / `ChildEntry` without the
/// matching singleton / instanced fact excludes that namespace from the address
/// index, so the root stops anchoring (`HalfDeclaredRoot`) or the child edge
/// stops filling the hole — either way the assertion goes red. The exclusion is
/// per-namespace, so this watches these two fixtures and not, as it once did,
/// every other namespace in the binary.
#[test]
fn an_unsplit_declaration_is_not_gated_out_of_its_cardinality_fact() {
    let registry = Registry::new();
    let canonical = format!("{}/{}:one", UnsplitRoot::NAMESPACE, UnsplitChild::NAMESPACE);
    registered_ref(&registry, &canonical, noop_handler());

    let path =
        ActorPath::new(&format!("{}/:one", UnsplitRoot::NAMESPACE)).expect("fixture is a well-formed actor path");
    assert_eq!(registry.resolve_address(&path).map(|resolved| resolved.canonical_path), Ok(canonical));
}
