//! Proven references for declared dependencies: on a ctx upgraded with
//! `__for_actor::<Dependent>()`, `actor_ref::<Dep>()` mints the position the
//! dependency's resolver folds from the caller scope it selects, for a `One`
//! and for an `Embedded` dependency.

use super::{NO_INBOUND_SOURCE, Registry, WasmCtx};
use crate::mail::Mail;
use crate::reference::ErasedActorRef;
use crate::wasm::{ActorInitError, WasmInitCtx};
use crate::{Addressable, Embedded, One};
use aether_data::MailboxId;

struct Dependent;

#[crate::actor(depends(OneDep, EmbeddedDep))]
impl crate::WasmActor for Dependent {
    const NAMESPACE: &'static str = "test.actor_ref.dependent";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self)
    }

    #[fallback]
    fn fallback(&mut self, _ctx: &mut WasmCtx<'_>, _mail: Mail<'_>) {
        let _ = self;
    }
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

/// The reference proves the resolver's fold for both declarable strategies:
/// `One` ignores the caller's carry, and `Embedded` seeds from the logical
/// parent rather than the ctx's own mailbox. Owned logic: the caller-scope
/// selection in `actor_ref`.
#[test]
fn actor_ref_mints_the_resolver_fold_for_one_and_embedded_dependencies() {
    let registry = Registry::new();
    registry.set_self_id(0xC000);
    registry.set_parent_id(0xC001);
    let mut ctx = WasmCtx::__new(0xC000, &registry, NO_INBOUND_SOURCE);
    let ctx = ctx.__for_actor::<Dependent>();

    assert_eq!(ctx.actor_ref::<OneDep>().id(), OneDep::resolve(0xC000, ()));
    assert_eq!(ctx.actor_ref::<EmbeddedDep>().id(), EmbeddedDep::resolve(0xC001, ()));
}

/// `sender` mints the threaded dispatch source: `None` for
/// `NO_INBOUND_SOURCE`, `Some` proving the threaded id otherwise. Owned
/// logic: the threaded source-field read, which adds no source-classification
/// of its own.
#[test]
fn sender_mints_the_threaded_source_and_none_without_one() {
    let registry = Registry::new();
    registry.set_self_id(0xC000);
    registry.set_parent_id(0xC001);

    let mut hook = WasmCtx::__new(0xC000, &registry, NO_INBOUND_SOURCE);
    assert_eq!(hook.__for_actor::<Dependent>().sender(), None);

    let threaded = MailboxId(0xC002);
    let mut mail = WasmCtx::__new(0xC000, &registry, threaded.0);
    assert_eq!(mail.__for_actor::<Dependent>().sender(), Some(ErasedActorRef::new(threaded)));
}
