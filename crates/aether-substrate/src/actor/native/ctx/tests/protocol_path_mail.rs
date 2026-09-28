//! Native dispatch proves a received `ProtocolPath` against the registry
//! (ADR-0231 §3): a typed arm decodes through `__decode_inbound`, whose
//! context carries the mail registry, so a path whose live route does not
//! publish the protocol's rows never reaches a handler.
//!
//! [`Keeper`] and [`Bystander`] stand `Live` with the contracts their own
//! `#[actor]` tables declare. Each [`Carries`] payload names one path and is
//! dispatched to a `Keeper` through its `#[actor]` arms, as the native
//! dispatcher does. The accepted manual path is resolved and receives a typed
//! send through its protocol reference.

use std::sync::Arc;
use std::sync::mpsc;

use aether_actor::{Addressable, ErasedActorRef, Manual, ProtocolPath, Row, Undeclared};
use aether_data::{ErasedActorPath, Kind, wire};

use crate::actor::native::binding::NativeBinding;
use crate::actor::native::{Dispatch, NativeActor, NativeCtx, NativeInitCtx};
use crate::chassis::error::BootError;
use crate::mail::Source;
use crate::mail::registry::{InboxHandler, OwnedDispatch, Registry, RouteContract, noop_handler};
use crate::testing::{bare_substrate, boot_authority, manual_dispatch_ctx, registered_ref, unrouted_binding};

#[aether_data::kind(name = "test.protocol_path.poke", copy, default)]
struct Poke {
    seq: u32,
}

/// The protocol a [`Carries`] path claims: a manual [`Poke`], which only the
/// [`Keeper`] handles and which promises no reply shape.
struct KeeperProtocol;

impl aether_actor::Protocol for KeeperProtocol {
    type Rows = (Row<Poke, Undeclared>,);
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

    #[handler::manual]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_, Self, Manual>, _mail: Poke) {
        let _ = self;
    }

    #[handler::single]
    fn on_carries(&mut self, ctx: &mut NativeCtx<'_>, mail: Carries) {
        let reference = ctx.resolve(&mail.path).expect("the decoded path remains live");
        ctx.send_to(reference, &Poke { seq: 7 });
        self.received.push(mail.path);
    }
}

/// Handles the right kind with the wrong reply shape, so it must not cover
/// the manual protocol.
struct SilentPoke;

#[aether_actor::actor]
impl NativeActor for SilentPoke {
    const NAMESPACE: &'static str = "test.protocol_path.silent_poke";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::single]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_>, _mail: Poke) {
        let _ = self;
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
fn stand<A: NativeActor>(registry: &Registry, handler: Arc<dyn InboxHandler>) -> ErasedActorRef {
    let reference = registered_ref(registry, A::NAMESPACE, handler);

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
/// bystander's and the unregistered path too. The sink also catches a resolve
/// that fails after decode or a typed send that does not route through the
/// manual protocol reference.
#[test]
fn a_received_path_reaches_the_handler_only_when_its_live_route_covers_the_protocol() {
    let (registry, mailer) = bare_substrate();
    let (tx, rx) = mpsc::channel();
    stand::<Keeper>(
        &registry,
        Arc::new(move |dispatch: OwnedDispatch| {
            dispatch.discharge();
            let _ = tx.send(dispatch);
        }),
    );
    stand::<SilentPoke>(&registry, noop_handler());
    stand::<Bystander>(&registry, noop_handler());
    let binding = unrouted_binding(&mailer);
    let mut keeper = Keeper::default();

    assert!(deliver(&mut keeper, &binding, Keeper::NAMESPACE), "the keeper's route covers the protocol");
    assert!(
        !deliver(&mut keeper, &binding, SilentPoke::NAMESPACE),
        "the same kind with a silent reply does not cover the manual row",
    );
    assert!(!deliver(&mut keeper, &binding, Bystander::NAMESPACE), "the bystander's route lacks its row");
    assert!(!deliver(&mut keeper, &binding, "test.protocol_path.nobody"), "no live route stands there");
    assert_eq!(keeper.received.iter().map(ToString::to_string).collect::<Vec<_>>(), [Keeper::NAMESPACE]);
    assert_eq!(rx.try_recv().expect("the resolved manual protocol receives the typed send").kind, Poke::ID);
}
