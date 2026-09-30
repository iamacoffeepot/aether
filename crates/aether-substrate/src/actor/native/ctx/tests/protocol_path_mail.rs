//! Native dispatch proves a received `ProtocolPath` against the registry
//! (ADR-0231 §3): a typed arm decodes through `__decode_inbound`, whose
//! context carries the mail registry, so a path whose route does not publish
//! the protocol's rows, or that no route has stood at, never reaches a
//! handler.
//!
//! The booted [`Keeper`] and the spawned [`SilentPoke`] and [`Bystander`]
//! stand `Live` with the contracts their own `#[actor]` tables declare. Each
//! [`Carries`] payload names one path and crosses the boundary to the
//! `Keeper` as a proven call, as a wire `Call` does, so the keeper's own
//! dispatch decodes it. The accepted unchecked path is resolved and receives a
//! typed send through its protocol reference.

use aether_actor::{Addressable, ErasedActorRef, ProtocolPath, Row, Unchecked, Undeclared};
use aether_data::{ErasedActorPath, Kind, wire};

use crate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, Subname};
use crate::chassis::error::BootError;
use crate::testing::{PumpedDriver, bare_substrate, boot_bare_test_chassis};

#[aether_data::kind(name = "test.protocol_path.poke", copy, default)]
struct Poke {
    seq: u32,
}

/// The protocol a [`Carries`] path claims: an unchecked [`Poke`], which only the
/// [`Keeper`] handles and which promises no reply shape.
struct KeeperProtocol;

impl aether_actor::Protocol for KeeperProtocol {
    type Rows = (Row<Poke, Undeclared>,);
}

#[aether_data::kind(name = "test.protocol_path.carries", no_serde)]
struct Carries {
    path: ProtocolPath<KeeperProtocol>,
}

/// Covers [`KeeperProtocol`], and records every path it receives and every
/// poke that reaches it.
#[derive(Default)]
struct Keeper {
    received: Vec<ProtocolPath<KeeperProtocol>>,
    pokes: Vec<u32>,
}

#[aether_actor::actor(singleton, root)]
impl NativeActor for Keeper {
    const NAMESPACE: &'static str = "test.protocol_path.keeper";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self::default())
    }

    #[handler::unchecked(reason = "test: a cast-only receiver exercising the unchecked row")]
    fn on_poke(&mut self, _ctx: &mut NativeCtx<'_, Self, Unchecked>, mail: Poke) {
        self.pokes.push(mail.seq);
    }

    #[handler::single]
    fn on_carries(&mut self, ctx: &mut NativeCtx<'_>, mail: Carries) {
        let reference = ctx.resolve(&mail.path).expect("the decoded path remains live");
        ctx.send_to(reference, &Poke { seq: 7 });
        self.received.push(mail.path);
    }
}

/// Handles the right kind with the wrong reply shape, so it must not cover
/// the unchecked protocol.
struct SilentPoke;

#[aether_actor::actor(instanced)]
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

#[aether_actor::actor(instanced)]
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

/// Deliver a [`Carries`] naming `path` to the keeper as a proven boundary
/// call, and settle it.
fn deliver(driver: &mut PumpedDriver<Keeper>, path: &ErasedActorPath) {
    let keeper = ErasedActorPath::new(Keeper::NAMESPACE).expect("the keeper's namespace is a path");
    let payload = wire::encode_to_vec(path).expect("encodes");
    let call = driver.chassis().accept_call(&keeper, Carries::ID, payload).expect("the keeper proves live");
    let (root, _settled) = driver.chassis().deliver_tracked(call, None);

    driver.settle(&[root]);
}

/// Catches native dispatch decoding without the registry: the context-free
/// decode refuses every path, so the keeper's own path would never arrive,
/// and a dispatch that skipped the proof would hand the handler the
/// bystander's and the unregistered path too. The keeper's pokes also catch
/// a resolve that fails after decode or a typed send that does not route
/// through the unchecked protocol reference.
#[test]
fn a_received_path_reaches_the_handler_only_when_its_live_route_covers_the_protocol() {
    let (registry, mailer) = bare_substrate();
    let chassis = boot_bare_test_chassis(&registry, &mailer);
    let silent = chassis.spawn_actor_for_test::<SilentPoke>(Subname::Named("peer"), (), ()).finish().expect("spawns");
    let bystander = chassis.spawn_actor_for_test::<Bystander>(Subname::Named("peer"), (), ()).finish().expect("spawns");
    let path = |peer: ErasedActorRef| chassis.actor_path(peer).expect("a spawned peer keeps its path");
    let (silent, bystander) = (path(silent.erase()), path(bystander.erase()));
    let mut driver = PumpedDriver::<Keeper>::boot(chassis, (), ());
    let received = |driver: &PumpedDriver<Keeper>| {
        driver
            .read_state(|keeper| keeper.received.iter().map(ToString::to_string).collect::<Vec<_>>())
            .expect("the keeper is live")
    };

    deliver(&mut driver, &ErasedActorPath::new(Keeper::NAMESPACE).expect("a well-formed path"));
    assert_eq!(received(&driver), [Keeper::NAMESPACE], "the keeper's route covers the protocol");

    deliver(&mut driver, &silent);
    assert_eq!(received(&driver).len(), 1, "the same kind with a silent reply does not cover the unchecked row");

    deliver(&mut driver, &bystander);
    assert_eq!(received(&driver).len(), 1, "the bystander's route lacks its row");

    deliver(&mut driver, &ErasedActorPath::new("test.protocol_path.nobody").expect("a well-formed path"));
    assert_eq!(received(&driver).len(), 1, "no route has stood there");

    assert_eq!(
        driver.read_state(|keeper| keeper.pokes.clone()).expect("the keeper is live"),
        [7],
        "the resolved unchecked protocol receives the typed send",
    );
}
