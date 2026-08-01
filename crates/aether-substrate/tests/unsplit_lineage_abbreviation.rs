//! ADR-0166 §4/§5 — the lineage facts an *un-split* `#[actor]` declaration
//! contributes to the link-time inventory, read back through the real
//! `Registry::resolve_address` seam.
//!
//! `#[actor]` emits a `RootEntry` / `ChildEntry` from `root` / `child_of(...)`
//! and a cardinality fact from `singleton` / `instanced`. `AddressIndex`
//! consumes the two together: it requires a cardinality fact for every
//! namespace a placement fact names, and rejects the whole index when one is
//! absent. So the emissions are only correct jointly, and a macro gate that can
//! admit one without the other is the defect — not the presence of either
//! submission on its own, which the `#[actor]` expansion trivially guarantees.
//!
//! This lives in `aether-substrate`'s test tree for the same reason
//! `native_actor_macro.rs` does: the macro expands to absolute
//! `::aether_substrate::*` paths, and the index that reads the result is owned
//! here.

// The fixtures register their own canonical mailboxes: the fold of an expanded
// path is the reference value under test, not a sibling-cap address.
#![allow(clippy::disallowed_methods)]

use aether_actor::{Addressable, actor};
use aether_data::mailbox_id_from_path;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::registry::noop_handler;
use aether_substrate::testing::boot_authority;
use aether_substrate::{Registry, ResolvedAddress};
use serde::{Deserialize, Serialize};

#[derive(
    Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, aether_data::Kind, aether_data::Schema,
)]
#[kind(name = "test.unsplit_lineage.poke")]
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
/// beneath it, which is what lets a bare discriminator elide the child segment.
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

fn register(registry: &Registry, canonical: &str) -> ResolvedAddress {
    let mailbox_id = mailbox_id_from_path(canonical);
    registry
        .try_register_inbox_with_id(&boot_authority(), mailbox_id, canonical, noop_handler())
        .expect("canonical name is free");

    ResolvedAddress { mailbox_id, canonical_path: canonical.to_owned() }
}

/// Tripwire: both abbreviations below are computed from link-time `#[actor]`
/// output, and each fails unless the *cardinality* half of one declaration is
/// present alongside its placement half. Reinstating any gate that emits a
/// `RootEntry` / `ChildEntry` without the matching singleton / instanced fact
/// excludes that namespace from the address index, so the root stops anchoring
/// (`HalfDeclaredRoot`) and the child edge stops eliding — both assertions go
/// red. The exclusion is per-namespace, so this watches these two fixtures and
/// not, as it once did, every other namespace in the binary.
#[test]
fn an_unsplit_declaration_is_not_gated_out_of_its_cardinality_fact() {
    let registry = Registry::new();
    let root = register(&registry, UnsplitRoot::NAMESPACE);
    let child = register(&registry, &format!("{}/{}:one", UnsplitRoot::NAMESPACE, UnsplitChild::NAMESPACE));

    // Anchoring the prefix at all needs the root's singleton fact: an instanced
    // or absent one keeps the namespace out of the root table.
    assert_eq!(registry.resolve_address(&format!("{}://", UnsplitRoot::NAMESPACE)), Ok(root));

    // Eliding the child namespace needs the child's instanced fact: a bare
    // discriminator only resolves against an instanced child edge.
    assert_eq!(registry.resolve_address(&format!("{}://one", UnsplitRoot::NAMESPACE)), Ok(child));
}
