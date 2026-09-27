//! Native dispatch proves a received `ProtocolPath` against the registry
//! (ADR-0231 §3): a typed arm decodes through `__decode_inbound`, whose
//! context carries the mail registry, so a path whose live route does not
//! publish the protocol's rows never reaches a handler.
//!
//! [`Keeper`] and [`Bystander`] stand `Live` with the contracts their own
//! `#[actor]` tables declare. Each [`Carries`] payload names one path and is
//! dispatched to a `Keeper` through its `#[actor]` arms, as the native
//! dispatcher does.

use std::sync::Arc;

use aether_actor::{Addressable, ErasedActorRef, ProtocolPath, Row, Silent};
use aether_data::{ErasedActorPath, Kind, wire};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::{Dispatch, NativeActor, NativeCtx, NativeInitCtx};
use crate::chassis::error::BootError;
use crate::mail::Source;
use crate::mail::registry::{Registry, RouteContract, noop_handler};
use crate::testing::{bare_substrate, boot_authority, manual_dispatch_ctx, registered_ref, unrouted_binding};

#[aether_data::kind(name = "test.protocol_path.poke", copy, default)]
struct Poke {
    seq: u32,
}

/// The protocol a [`Carries`] path claims: a silent [`Poke`], which only the
/// [`Keeper`] handles.
struct KeeperProtocol;

impl aether_actor::Protocol for KeeperProtocol {
    type Rows = (Row<Poke, Silent>,);
}

#[aether_data::kind(name = "test.protocol_path.carries", no_serde)]
struct Carries {
    path: ProtocolPath<KeeperProtocol>,
}

/// Covers [`KeeperProtocol`], and records every path it receives.
#[derive(Default)]
struct Keeper {
    received: Vec<ProtocolPath<KeeperProtocol>>,
}

#[aether_actor::actor]
impl NativeActor for Keeper {
    const NAMESPACE: &'static str = "test.protocol_path.keeper";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self::default())
    }

    #[handler::single]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Poke) {
        let _ = self;
    }

    #[handler::single]
    fn on_carries(&mut self, _ctx: &mut NativeCtx<'_>, mail: Carries) {
        self.received.push(mail.path);
    }
}

/// Handles [`Carries`] only, so its contract lacks [`KeeperProtocol`]'s row.
struct Bystander;

#[aether_actor::actor]
impl NativeActor for Bystander {
    const NAMESPACE: &'static str = "test.protocol_path.bystander";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_carries(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Carries) {
        let _ = self;
    }
}

/// Stand `A` `Live` at its namespace, publishing the contract its `#[actor]`
/// table declares, as the chassis boot does.
fn stand<A: NativeActor>(registry: &Registry) -> ErasedActorRef {
    let reference = registered_ref(registry, A::NAMESPACE, noop_handler());

    registry.publish_contract(&boot_authority(), reference.id(), RouteContract::of::<A>()).expect("publishes");
    reference
}

/// Dispatch a [`Carries`] naming `path` to `keeper` through its `#[actor]`
/// arms, returning whether an arm handled it.
fn deliver(keeper: &mut Keeper, binding: &Arc<NativeBinding>, path: &str) -> bool {
    let payload = wire::encode_to_vec(&ErasedActorPath::new(path).expect("a well-formed path")).expect("encodes");
    let mut ctx = manual_dispatch_ctx::<Keeper>(binding, Source::NONE);

    let handled = <Keeper as Dispatch<Keeper>>::dispatch(keeper, &mut ctx, Carries::ID, &payload);
    drop(ctx.take_raw_inbound());
    handled.is_some()
}

/// Catches native dispatch decoding without the registry: the context-free
/// decode refuses every path, so the keeper's own path would never arrive,
/// and a dispatch that skipped the proof would hand the handler the
/// bystander's and the unregistered path too.
#[test]
fn a_received_path_reaches_the_handler_only_when_its_live_route_covers_the_protocol() {
    let (registry, mailer) = bare_substrate();
    stand::<Keeper>(&registry);
    stand::<Bystander>(&registry);
    let binding = unrouted_binding(&mailer);
    let mut keeper = Keeper::default();

    assert!(deliver(&mut keeper, &binding, Keeper::NAMESPACE), "the keeper's route covers the protocol");
    assert!(!deliver(&mut keeper, &binding, Bystander::NAMESPACE), "the bystander's route lacks its row");
    assert!(!deliver(&mut keeper, &binding, "test.protocol_path.nobody"), "no live route stands there");
    assert_eq!(keeper.received.iter().map(ToString::to_string).collect::<Vec<_>>(), [Keeper::NAMESPACE]);
}
