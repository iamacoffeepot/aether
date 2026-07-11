//! Real loopback acceptance coverage for the player-session tier.

#![allow(clippy::needless_pass_by_value)]

use std::io;
use std::net::TcpStream;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use aether_actor::{Addressable, actor};
use aether_codec::frame::{FrameError, read_frame, write_frame};
use aether_data::{Kind, MailboxId, SessionToken, Source, Uuid};
use aether_kinds::descriptors;
use aether_kinds::trace::Nanos;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::MailId;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::{EgressEvent, HubOutbound};
use aether_substrate::mail::registry::{MailboxEntry, OwnedDispatch, Registry};
use aether_substrate::mail::{MailRef, SourceAddr};
use aether_substrate::testing::TestChassis;
use serde::{Deserialize, Serialize};

use super::{PlayerFrame, PlayerGatewayCapability, PlayerGatewayConfig, WIRE_VERSION};
use crate::game::{
    GridBounds, MoveDirection, MoveIntent, Poll, PollResult, SimConfig, Spawn, StateSummary, TickBundle,
};
use crate::tcp::{BindListener, BindListenerResult, TcpCapability};

const INTERVAL_NANOS: u64 = 20_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
enum ObservedSimMail {
    Poll(Poll),
    Spawn(Spawn),
    Move(MoveIntent),
}

pub struct TestTurnSim;

pub struct TestTurnSimConfig {
    sim: SimConfig,
    retained: Vec<TickBundle>,
    observed: mpsc::Sender<ObservedSimMail>,
}

pub struct TestTurnSimState {
    sim: SimConfig,
    retained: Vec<TickBundle>,
    observed: mpsc::Sender<ObservedSimMail>,
    pending_poll: Option<MailboxId>,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.game.player.test.publish_bundle")]
struct PublishBundle {
    bundle: TickBundle,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Copy, Debug, Default)]
#[kind(name = "aether.game.player.test.release_poll")]
struct ReleasePoll {}

#[actor(singleton)]
impl NativeActor for TestTurnSim {
    type State = TestTurnSimState;
    type Config = TestTurnSimConfig;
    const NAMESPACE: &'static str = "aether.game.player.test.turn_sim";

    fn init(config: TestTurnSimConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<TestTurnSimState, BootError> {
        Ok(TestTurnSimState {
            sim: config.sim,
            retained: config.retained,
            observed: config.observed,
            pending_poll: None,
        })
    }

    #[handler::single]
    fn on_poll(state: &mut Self::State, ctx: &mut NativeCtx<'_>, poll: Poll) {
        state.observed.send(ObservedSimMail::Poll(poll)).expect("observation receiver remains live");
        state.pending_poll = ctx.source_mailbox();
    }

    #[handler::single]
    fn on_spawn(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, spawn: Spawn) {
        state.observed.send(ObservedSimMail::Spawn(spawn)).expect("observation receiver remains live");
    }

    #[handler::single]
    fn on_move(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, intent: MoveIntent) {
        state.observed.send(ObservedSimMail::Move(intent)).expect("observation receiver remains live");
    }

    #[handler::single]
    fn on_publish(state: &mut Self::State, ctx: &mut NativeCtx<'_>, publish: PublishBundle) {
        if let Some(fact_sink) = state.sim.fact_sink {
            ctx.fanout([fact_sink], &publish.bundle);
        }
        if !state.retained.iter().any(|bundle| bundle.tick == publish.bundle.tick) {
            state.retained.push(publish.bundle);
            state.retained.sort_by_key(|bundle| bundle.tick);
        }
    }

    #[handler::single]
    fn on_release_poll(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _release: ReleasePoll) {
        let Some(target) = state.pending_poll.take() else {
            return;
        };
        let current_tick = state.retained.last().map_or(0, |bundle| bundle.tick);
        ctx.fanout([target], &PollResult { bundles: state.retained.clone(), current_tick });
    }
}

fn bundle(tick: u64) -> TickBundle {
    TickBundle {
        tick,
        superseded_through: tick,
        trajectory: Vec::new(),
        summary: StateSummary { tick, entities: Vec::new() },
    }
}

fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>, mpsc::Receiver<EgressEvent>) {
    let registry = Arc::new(Registry::new());
    for descriptor in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(descriptor);
    }
    let (outbound, rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    (registry, mailer, rx)
}

fn boot_player_substrate()
-> (Arc<Registry>, mpsc::Receiver<EgressEvent>, mpsc::Receiver<ObservedSimMail>, PassiveChassis<TestChassis>) {
    let (registry, mailer, egress) = fresh_substrate();
    let (observed_tx, observed_rx) = mpsc::channel();
    let gateway = PlayerGatewayCapability::resolve(0, ());
    let turn_sim = TestTurnSim::resolve(0, ());
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), mailer)
        .with_actor::<TcpCapability>(())
        .with_actor::<TestTurnSim>(TestTurnSimConfig {
            sim: SimConfig { fact_sink: Some(gateway), ring_depth: 8, grid_bounds: GridBounds::default() },
            retained: vec![bundle(1), bundle(2)],
            observed: observed_tx,
        })
        .with_actor::<PlayerGatewayCapability>(PlayerGatewayConfig {
            turn_sim: Some(turn_sim),
            tick_interval_nanos: INTERVAL_NANOS,
        })
        .build_passive()
        .expect("player test chassis boots");
    (registry, egress, observed_rx, chassis)
}

fn enqueue<K: Kind>(registry: &Registry, namespace: &str, mail: &K, source: Source) {
    let id = registry.lookup(namespace).expect("mailbox registered");
    let MailboxEntry::Inbox { handler, .. } = registry.entry(id).expect("mailbox entry") else {
        panic!("expected actor inbox");
    };
    handler.enqueue(OwnedDispatch::disarmed(
        K::ID,
        K::NAME.to_owned(),
        None,
        source,
        MailRef::from(mail.encode_into_bytes()),
        1,
        MailId::NONE,
        MailId::NONE,
        None,
        Nanos(0),
        0,
        MailboxId::NONE,
    ));
}

fn bind_player_listener(registry: &Registry, egress: &mpsc::Receiver<EgressEvent>) -> u16 {
    let reply_source = Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xfeed))));
    enqueue(
        registry,
        TcpCapability::NAMESPACE,
        &BindListener {
            addr: "127.0.0.1:0".into(),
            name: Some("players".into()),
            consumer: Some(PlayerGatewayCapability::NAMESPACE.into()),
        },
        reply_source,
    );
    let EgressEvent::ToSession { payload, .. } = egress.recv_timeout(Duration::from_secs(2)).expect("bind reply")
    else {
        panic!("bind reply must use loopback egress");
    };
    match BindListenerResult::decode_from_bytes(&payload).expect("decode bind result") {
        BindListenerResult::Ok { local_port, .. } => local_port,
        BindListenerResult::Err { reason, .. } => panic!("bind player listener: {reason}"),
    }
}

fn expect_fact(stream: &mut TcpStream, expected_tick: u64) {
    let frame: PlayerFrame = read_frame(stream).expect("read fact frame");
    let PlayerFrame::Fact { kind, payload } = frame else {
        panic!("expected fact frame, got {frame:?}");
    };
    assert_eq!(kind, TickBundle::ID);
    let fact = TickBundle::decode_from_bytes(&payload).expect("decode tick bundle fact");
    assert_eq!(fact.tick, expected_tick);

    let frame: PlayerFrame = read_frame(stream).expect("read beacon frame");
    let PlayerFrame::Beacon { tick, interval_nanos, .. } = frame else {
        panic!("expected beacon frame, got {frame:?}");
    };
    assert_eq!(tick, expected_tick, "fact and beacon must come from the same completed bundle");
    assert_eq!(interval_nanos, INTERVAL_NANOS);
}

#[test]
fn gateway_config_is_inert_by_default() {
    assert_eq!(PlayerGatewayConfig::default().turn_sim, None);
}

#[test]
fn loopback_session_enforces_identity_allowlist_and_tick_bundle_delivery() {
    let (registry, egress, observed, _chassis) = boot_player_substrate();
    let port = bind_player_listener(&registry, &egress);
    let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connect player client");
    client.set_read_timeout(Some(Duration::from_secs(2))).expect("set player read timeout");

    write_frame(&mut client, &PlayerFrame::Hello { wire_version: WIRE_VERSION, client_name: "loopback".into() })
        .expect("write player hello");
    assert_eq!(
        observed.recv_timeout(Duration::from_secs(2)).expect("session polls for catch-up"),
        ObservedSimMail::Poll(Poll { since_tick: 0 }),
    );

    enqueue(&registry, TestTurnSim::NAMESPACE, &PublishBundle { bundle: bundle(2) }, Source::NONE);
    enqueue(&registry, TestTurnSim::NAMESPACE, &ReleasePoll::default(), Source::NONE);

    let ack: PlayerFrame = read_frame(&mut client).expect("read HelloAck");
    let PlayerFrame::HelloAck { wire_version, session_identity, tick, interval_nanos } = ack else {
        panic!("expected HelloAck, got {ack:?}");
    };
    assert_eq!(wire_version, WIRE_VERSION);
    assert_eq!(tick, 2);
    assert_eq!(interval_nanos, INTERVAL_NANOS);
    assert_ne!(session_identity, MailboxId::NONE);

    expect_fact(&mut client, 1);
    expect_fact(&mut client, 2);
    client.set_read_timeout(Some(Duration::from_millis(100))).expect("set overlap probe timeout");
    assert!(
        matches!(read_frame::<_, PlayerFrame>(&mut client), Err(FrameError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::TimedOut),
        "poll/live overlap at tick 2 must be emitted once",
    );
    client.set_read_timeout(Some(Duration::from_secs(2))).expect("restore player read timeout");

    let forged = Spawn { entity_id: 0xdead_beef, cell_x: 3, cell_z: -2 };
    write_frame(&mut client, &PlayerFrame::Intent { kind: Spawn::ID, payload: forged.encode_into_bytes() })
        .expect("write forged spawn");
    let ObservedSimMail::Spawn(stamped) =
        observed.recv_timeout(Duration::from_secs(2)).expect("sim receives stamped spawn")
    else {
        panic!("expected stamped spawn");
    };
    assert_eq!(stamped.entity_id, session_identity.0, "server session identity overwrites the client claim");
    assert_eq!((stamped.cell_x, stamped.cell_z), (forged.cell_x, forged.cell_z));

    write_frame(
        &mut client,
        &PlayerFrame::Intent { kind: Poll::ID, payload: Poll { since_tick: 0 }.encode_into_bytes() },
    )
    .expect("write non-allowlisted intent");
    assert!(
        observed.recv_timeout(Duration::from_millis(100)).is_err(),
        "an unknown intent kind must never dispatch to TurnSim",
    );

    let move_intent = MoveIntent { entity_id: 7, direction: MoveDirection::West };
    write_frame(&mut client, &PlayerFrame::Intent { kind: MoveIntent::ID, payload: move_intent.encode_into_bytes() })
        .expect("write forged move intent");
    let ObservedSimMail::Move(stamped) = observed.recv_timeout(Duration::from_secs(2)).expect("sim receives move")
    else {
        panic!("expected stamped move");
    };
    assert_eq!(stamped.entity_id, session_identity.0);
    assert_eq!(stamped.direction, move_intent.direction);

    enqueue(&registry, TestTurnSim::NAMESPACE, &PublishBundle { bundle: bundle(3) }, Source::NONE);
    expect_fact(&mut client, 3);
}
