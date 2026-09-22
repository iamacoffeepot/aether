//! Proven references for declared dependencies: on a ctx upgraded with
//! `__for_actor::<Dependent>()`, `actor_ref::<Dep>()` mints the position
//! `actor::<Dep>()` folds, for a `One` and for an `Embedded` dependency.

use super::{NO_INBOUND_SOURCE, Registry, WasmCtx};
use crate::reference::AnyActorRef;
use crate::{Addressable, DependsOn, Embedded, One};
use aether_data::MailboxId;

struct Dependent;

impl Addressable for Dependent {
    const NAMESPACE: &'static str = "test.actor_ref.dependent";
    type Resolver = One;
}

struct OneDep;

impl Addressable for OneDep {
    const NAMESPACE: &'static str = "test.actor_ref.one_dep";
    type Resolver = One;
}

struct EmbeddedDep;

impl Addressable for EmbeddedDep {
    const NAMESPACE: &'static str = "test.actor_ref.embedded_dep";
    type Resolver = Embedded;
}

impl DependsOn<OneDep> for Dependent {}
impl DependsOn<EmbeddedDep> for Dependent {}

/// `actor_ref` and `actor` share one derivation: the reference proves the
/// folded position for both declarable strategies. Owned logic: the shared
/// `actor_with_namespace` fold behind both doors.
#[test]
fn actor_ref_mints_the_position_actor_folds_for_one_and_embedded_dependencies() {
    let registry = Registry::new();
    registry.set_self_id(0xC000);
    registry.set_parent_id(0xC001);
    let mut ctx = WasmCtx::__new(0xC000, &registry, NO_INBOUND_SOURCE);
    let ctx = ctx.__for_actor::<Dependent>();

    assert_eq!(ctx.actor_ref::<OneDep>().id(), ctx.actor::<OneDep>().mailbox_id());
    assert_eq!(ctx.actor_ref::<EmbeddedDep>().id(), ctx.actor::<EmbeddedDep>().mailbox_id());
}

/// `sender` mints the threaded dispatch source: `None` for
/// `NO_INBOUND_SOURCE`, `Some` proving the threaded id otherwise. Owned
/// logic: the `source_mailbox` lift, which adds no source-classification of
/// its own.
#[test]
fn sender_mints_the_threaded_source_and_none_without_one() {
    let registry = Registry::new();
    registry.set_self_id(0xC000);
    registry.set_parent_id(0xC001);

    let mut hook = WasmCtx::__new(0xC000, &registry, NO_INBOUND_SOURCE);
    assert_eq!(hook.__for_actor::<Dependent>().sender(), None);

    let threaded = MailboxId(0xC002);
    let mut mail = WasmCtx::__new(0xC000, &registry, threaded.0);
    assert_eq!(mail.__for_actor::<Dependent>().sender(), Some(AnyActorRef::new(threaded)));
}
