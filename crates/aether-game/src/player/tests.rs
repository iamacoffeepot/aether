//! Real loopback and addressing coverage for the player-session tier.

#![allow(clippy::needless_pass_by_value)]
// These tests are deliberate embedders: they build a bare `TestChassis` via
// `Builder::new` rather than the `composed` boot seam production chassis use.
#![allow(clippy::disallowed_methods)]

use std::io;
use std::net::{Ipv4Addr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::{Addressable, Manual, OutboundReply, actor};
use aether_codec::frame::{FrameError, read_frame, write_frame};
use aether_data::{
    ActorId, Kind, MailboxId, SessionToken, Source, SourceAddr, Tag, Uuid, fold_lineage, wire, with_tag,
};
use aether_kinds::trace::Nanos;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::builder::{Builder, PassiveChassis};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::outbound::EgressEvent;
use aether_substrate::mail::registry::{MailDispatch, MailboxEntry, OwnedDispatch, Registry};
use aether_substrate::mail::{MailId, MailRef};
use aether_substrate::testing::{TestChassis, boot_authority, fresh_substrate_and_rx};
use serde::{Deserialize, Serialize};

use super::{
    GameGatewayCapability, GameGatewayConfig, GameGatewayParams, PlayerFrame, PlayerSessionActor, WIRE_VERSION,
};
use crate::{GridBounds, MoveDirection, MoveIntent, Poll, PollResult, SimConfig, Spawn, StateSummary, TickBundle};
use aether_tcp::{
    BindListener, ListListeners, ListListenersResult, SessionData, TcpCapability, TcpListenerActor, TcpSessionActor,
};

const LISTENER_NAME: &str = "players";
const INTERVAL_NANOS: u64 = 20_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
enum ObservedSimMail {
    Poll { mail: Poll, source: Option<MailboxId> },
    Spawn { mail: Spawn, source: Option<MailboxId> },
    Move { mail: MoveIntent, source: Option<MailboxId> },
}

pub struct TestTurnSim;

pub struct TestTurnSimParams {
    sim: SimConfig,
    retained: Vec<TickBundle>,
    observed: mpsc::Sender<ObservedSimMail>,
    defer_poll_result: bool,
}

pub struct TestTurnSimState {
    sim: SimConfig,
    retained: Vec<TickBundle>,
    observed: mpsc::Sender<ObservedSimMail>,
    defer_poll_result: bool,
}

struct PlayerTestSubstrate {
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    outbound_replies: mpsc::Receiver<EgressEvent>,
}

struct PlayerTestHarness {
    registry: Arc<Registry>,
    mailer: Arc<Mailer>,
    observed: mpsc::Receiver<ObservedSimMail>,
    chassis: PassiveChassis<TestChassis>,
    listener_port: u16,
}

#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.game.player.test.publish_bundle")]
struct PublishBundle {
    bundle: TickBundle,
}

#[actor(singleton, root)]
impl NativeActor for TestTurnSim {
    type State = TestTurnSimState;
    // ADR-0156 §3: the retained-bundle vec + observation channel are
    // construction wiring, not operator config, so they ride the `Params`
    // channel; `Config` is `()`.
    type Config = ();
    type Params = TestTurnSimParams;
    const NAMESPACE: &'static str = "aether.game.player.test.turn_sim";

    fn init((): (), params: TestTurnSimParams, _ctx: &mut NativeInitCtx<'_>) -> Result<TestTurnSimState, BootError> {
        Ok(TestTurnSimState {
            sim: params.sim,
            retained: params.retained,
            observed: params.observed,
            defer_poll_result: params.defer_poll_result,
        })
    }

    #[handler::manual]
    fn on_poll(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, poll: Poll) {
        state
            .observed
            .send(ObservedSimMail::Poll { mail: poll, source: ctx.source_mailbox() })
            .expect("observation receiver remains live");

        if let (Some(fact_sink), Some(overlap)) =
            (state.sim.fact_sink, state.retained.iter().find(|bundle| bundle.tick == 2))
        {
            let _ = ctx.send_envelope_tracked(fact_sink, TickBundle::ID, &overlap.encode_into_bytes());
        }

        if !state.defer_poll_result {
            ctx.reply(&PollResult {
                current_tick: state.retained.last().map_or(0, |bundle| bundle.tick),
                bundles: state.retained.clone(),
            });
        }
    }

    #[handler::single]
    fn on_spawn(state: &mut Self::State, ctx: &mut NativeCtx<'_>, spawn: Spawn) {
        state
            .observed
            .send(ObservedSimMail::Spawn { mail: spawn, source: ctx.source_mailbox() })
            .expect("observation receiver remains live");
    }

    #[handler::single]
    fn on_move(state: &mut Self::State, ctx: &mut NativeCtx<'_>, intent: MoveIntent) {
        state
            .observed
            .send(ObservedSimMail::Move { mail: intent, source: ctx.source_mailbox() })
            .expect("observation receiver remains live");
    }

    #[handler::single]
    fn on_publish(state: &mut Self::State, ctx: &mut NativeCtx<'_>, publish: PublishBundle) {
        if let Some(fact_sink) = state.sim.fact_sink {
            let _ = ctx.send_envelope_tracked(fact_sink, TickBundle::ID, &publish.bundle.encode_into_bytes());
        }
        if !state.retained.iter().any(|bundle| bundle.tick == publish.bundle.tick) {
            state.retained.push(publish.bundle);
            state.retained.sort_by_key(|bundle| bundle.tick);
        }
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

fn fresh_player_test_substrate() -> PlayerTestSubstrate {
    let (registry, mailer, outbound_replies) = fresh_substrate_and_rx();
    PlayerTestSubstrate { registry, mailer, outbound_replies }
}

fn boot_player_substrate() -> PlayerTestHarness {
    boot_player_substrate_with_limits(
        GameGatewayConfig::DEFAULT_MAX_ACTIVE_SESSIONS,
        GameGatewayConfig::DEFAULT_MAX_PENDING_LIVE_BUNDLES,
        false,
    )
}

fn boot_player_substrate_with_limits(
    max_active_sessions: usize,
    max_pending_live_bundles: usize,
    defer_poll_result: bool,
) -> PlayerTestHarness {
    let PlayerTestSubstrate { registry, mailer, outbound_replies } = fresh_player_test_substrate();
    let (observed_tx, observed_rx) = mpsc::channel();
    let gateway_mailbox = GameGatewayCapability::resolve(0, ());
    let turn_sim_mailbox = TestTurnSim::resolve(0, ());
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TcpCapability>(())
        .with_actor::<TestTurnSim>(TestTurnSimParams {
            sim: SimConfig { fact_sink: Some(gateway_mailbox), ring_depth: 8, grid_bounds: GridBounds::default() },
            retained: vec![bundle(1), bundle(2)],
            observed: observed_tx,
            defer_poll_result,
        })
        .with_actor_configured::<GameGatewayCapability>(
            GameGatewayParams { turn_sim_mailbox: Some(turn_sim_mailbox) },
            GameGatewayConfig {
                listener_addr: Some("127.0.0.1:0".into()),
                listener_name: LISTENER_NAME.into(),

                interval_nanos: INTERVAL_NANOS,
                max_active_sessions,
                max_pending_live_bundles,
            },
        )
        .build_passive()
        .expect("player test chassis boots");
    let listener_port = await_player_listener_port(&registry, &outbound_replies);
    PlayerTestHarness { registry, mailer, observed: observed_rx, chassis, listener_port }
}

fn enqueue<K: Kind>(registry: &Arc<Registry>, mailbox: MailboxId, mail: &K) {
    enqueue_with_source(registry, mailbox, mail, Source::NONE);
}

fn enqueue_with_source<K: Kind>(registry: &Arc<Registry>, mailbox: MailboxId, mail: &K, source: Source) {
    let entry = registry.entry(mailbox).expect("target mailbox is registered");
    let MailboxEntry::Inbox { handler, .. } = entry else {
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

fn enqueue_rooted<K: Kind>(
    registry: &Registry,
    mailer: &Mailer,
    mailbox: MailboxId,
    mail: &K,
    source: Source,
    root: MailId,
) {
    mailer.record_sent(root, root, None, root.sender, mailbox, K::ID);
    let MailboxEntry::Inbox { handler, .. } = registry.entry(mailbox).expect("target mailbox is registered") else {
        panic!("expected actor inbox");
    };
    handler.enqueue(OwnedDispatch::disarmed(
        K::ID,
        K::NAME.to_owned(),
        None,
        source,
        MailRef::from(mail.encode_into_bytes()),
        1,
        root,
        root,
        None,
        Nanos(0),
        0,
        MailboxId::NONE,
    ));
}

fn listener_reply_target() -> Source {
    Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0x3139))))
}

fn await_player_listener_port(registry: &Arc<Registry>, outbound_replies: &mpsc::Receiver<EgressEvent>) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        enqueue_with_source(
            registry,
            TcpCapability::resolve(0, ()),
            &ListListeners::default(),
            listener_reply_target(),
        );
        let list = match outbound_replies.recv_timeout(Duration::from_millis(25)) {
            Ok(EgressEvent::ToSession { kind_name, payload, .. }) => {
                assert_eq!(kind_name, ListListenersResult::NAME, "gateway listener list returned an unexpected reply");
                ListListenersResult::decode_from_bytes(&payload).expect("decode gateway listener list")
            }
            Ok(other) => panic!("gateway listener list returned unexpected egress: {other:?}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(
                    Instant::now() < deadline,
                    "gateway listener did not bind within two seconds before any player Hello"
                );
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("gateway listener reply channel disconnected"),
        };
        let matching_listener_count = list.listeners.iter().filter(|listener| listener.name == LISTENER_NAME).count();
        assert!(
            matching_listener_count <= 1,
            "gateway listener did not bind uniquely: expected one {LISTENER_NAME:?} entry, got {matching_listener_count}: {:?}",
            list.listeners
        );
        if matching_listener_count == 1 {
            let listener = list
                .listeners
                .iter()
                .find(|listener| listener.name == LISTENER_NAME)
                .expect("matching player listener count was one");
            assert!(listener.port > 0, "gateway listener bound an invalid local port");
            return listener.port;
        }
        assert!(
            Instant::now() < deadline,
            "gateway listener did not bind within two seconds before any player Hello; live listeners: {:?}",
            list.listeners
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn expected_player_session_mailbox(session_name: &str) -> MailboxId {
    MailboxId(with_tag(
        Tag::Mailbox,
        fold_lineage(
            GameGatewayCapability::resolve(0, ()).0,
            ActorId::instanced(PlayerSessionActor::NAMESPACE, session_name),
        ),
    ))
}

fn expected_tcp_session_mailbox(session_name: &str) -> MailboxId {
    let listener_carry =
        fold_lineage(TcpCapability::resolve(0, ()).0, ActorId::instanced(TcpListenerActor::NAMESPACE, LISTENER_NAME));
    MailboxId(with_tag(
        Tag::Mailbox,
        fold_lineage(listener_carry, ActorId::instanced(TcpSessionActor::NAMESPACE, session_name)),
    ))
}

fn register_player_route_collision(registry: &Registry, session_name: &str, deliveries: Arc<AtomicUsize>) -> MailboxId {
    let mailbox = expected_player_session_mailbox(session_name);
    let canonical_name =
        format!("{}/{}:{}", GameGatewayCapability::NAMESPACE, PlayerSessionActor::NAMESPACE, session_name);
    registry
        .try_register_inbox_with_id(
            &boot_authority(),
            mailbox,
            &canonical_name,
            Arc::new(move |dispatch: OwnedDispatch| {
                deliveries.fetch_add(1, Ordering::Relaxed);
                dispatch.discharge();
            }),
        )
        .expect("install test-only player-session collision authority");
    mailbox
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
    let config = GameGatewayConfig::default();
    assert_eq!(config.listener_addr, None);
    // ADR-0156 §3: the resolved `TurnSim` mailbox moved to `GameGatewayParams`;
    // its default is `None`, so a default-composed gateway stays inert.
    assert_eq!(GameGatewayParams::default().turn_sim_mailbox, None);
    assert_eq!(config.max_active_sessions, GameGatewayConfig::DEFAULT_MAX_ACTIVE_SESSIONS);
    assert_eq!(config.max_pending_live_bundles, GameGatewayConfig::DEFAULT_MAX_PENDING_LIVE_BUNDLES);
}

#[test]
fn gateway_wire_binds_with_its_exact_resolved_mailbox() {
    let PlayerTestSubstrate { registry, mailer, outbound_replies: _outbound_replies } = fresh_player_test_substrate();
    let (bind_tx, bind_rx) = mpsc::channel();
    registry.register_inline(
        TcpCapability::NAMESPACE,
        Arc::new(move |dispatch: MailDispatch<'_>| {
            if dispatch.kind == BindListener::ID {
                bind_tx
                    .send(BindListener::decode_from_bytes(dispatch.payload).expect("decode gateway bind"))
                    .expect("bind observation receiver remains live");
            }
        }),
    );

    let turn_sim_mailbox = MailboxId(0xfeed_beef);
    let _chassis = Builder::<TestChassis>::new(registry, mailer)
        .with_actor_configured::<GameGatewayCapability>(
            GameGatewayParams { turn_sim_mailbox: Some(turn_sim_mailbox) },
            GameGatewayConfig {
                listener_addr: Some("127.0.0.1:0".into()),
                listener_name: LISTENER_NAME.into(),

                interval_nanos: INTERVAL_NANOS,
                max_active_sessions: GameGatewayConfig::DEFAULT_MAX_ACTIVE_SESSIONS,
                max_pending_live_bundles: GameGatewayConfig::DEFAULT_MAX_PENDING_LIVE_BUNDLES,
            },
        )
        .build_passive()
        .expect("gateway observer chassis boots");

    let bind = bind_rx.recv_timeout(Duration::from_secs(2)).expect("gateway emits bind during wire");
    assert_eq!(bind.addr, "127.0.0.1:0");
    assert_eq!(bind.name.as_deref(), Some(LISTENER_NAME));
    assert_eq!(bind.consumer, Some(GameGatewayCapability::resolve(0, ())));
    assert_ne!(bind.consumer, Some(turn_sim_mailbox), "the configured sim id is not the tcp consumer");
}

#[test]
fn player_wire_round_trips_and_rejects_malformed_bytes() {
    let frames = [
        PlayerFrame::Hello { wire_version: WIRE_VERSION, client_name: "client".into() },
        PlayerFrame::HelloAck {
            wire_version: WIRE_VERSION,
            session_identity: MailboxId(42),
            tick: 7,
            interval_nanos: INTERVAL_NANOS,
        },
        PlayerFrame::Intent { kind: Spawn::ID, payload: vec![1, 2, 3] },
        PlayerFrame::Fact { kind: TickBundle::ID, payload: vec![4, 5] },
        PlayerFrame::Beacon { tick: 7, server_nanos: 99, interval_nanos: INTERVAL_NANOS },
        PlayerFrame::Close { reason: "done".into() },
    ];

    for frame in frames {
        let bytes = wire::to_vec(&frame).expect("encode recipient-free player frame");
        assert_eq!(wire::from_bytes::<PlayerFrame>(&bytes).expect("decode player frame"), frame);
    }
    assert!(wire::from_bytes::<PlayerFrame>(&[0xff, 0xff]).is_err());
}

/// Scheduler-backed pending-tail proof. The second frame is written before
/// the first can complete its Hello round trip. Depending on whether `TurnSim`'s
/// `PollResult` wins the race, it is either admitted as an active intent or
/// rejected in-order while the session is catching up; warn-dropping it would
/// produce neither observable outcome.
#[test]
fn frame_racing_player_birth_is_delivered_after_the_bootstrap_frame() {
    let PlayerTestHarness { observed, chassis: _chassis, listener_port, .. } = boot_player_substrate();
    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, listener_port)).expect("connect racing player client");
    client.set_read_timeout(Some(Duration::from_secs(2))).expect("set racing player timeout");

    write_frame(&mut client, &PlayerFrame::Hello { wire_version: WIRE_VERSION, client_name: "racing".into() })
        .expect("write racing Hello");
    write_frame(
        &mut client,
        &PlayerFrame::Intent {
            kind: Spawn::ID,
            payload: Spawn { entity_id: 0, cell_x: 4, cell_z: 6 }.encode_into_bytes(),
        },
    )
    .expect("write frame racing activation");

    assert!(matches!(
        observed.recv_timeout(Duration::from_secs(2)).expect("bootstrap Hello reaches the player child first"),
        ObservedSimMail::Poll { .. }
    ));
    match observed.recv_timeout(Duration::from_secs(2)) {
        Ok(ObservedSimMail::Spawn { mail, source }) => {
            assert_eq!(source, Some(expected_player_session_mailbox("conn-0")));
            assert_eq!((mail.cell_x, mail.cell_z), (4, 6));
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let close: PlayerFrame = read_frame(&mut client).expect("an early second frame closes in order");
            assert!(matches!(close, PlayerFrame::Close { reason } if reason == "player frame arrived before HelloAck"));
        }
        Ok(other) => panic!("unexpected simulation mail after bootstrap: {other:?}"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("simulation observation channel disconnected"),
    }
}

/// A test-only canonical route owns the would-be child id before the handler
/// stages it. Local init succeeds, authoritative apply rejects, and the
/// no-reply task turn must still release the original root without installing
/// a live fan-out index. This is the scheduler proof; the runtime module's
/// reducer tests cover pending capacity and close idempotence directly.
#[test]
fn owner_rejected_player_birth_settles_without_a_live_session() {
    let PlayerTestHarness { registry, mailer, chassis, .. } = boot_player_substrate();
    let session_name = "apply-collision";
    let deliveries = Arc::new(AtomicUsize::new(0));
    let collision = register_player_route_collision(&registry, session_name, Arc::clone(&deliveries));
    let source_mailbox = expected_tcp_session_mailbox(session_name);
    let correlation_id = 0x4069_C011;
    let root = MailId::new(source_mailbox, correlation_id);
    let settled = chassis.settlement_registry().subscribe_settlement(root);
    let session_data = SessionData {
        session_name: session_name.to_owned(),
        peer: "127.0.0.1:4069".to_owned(),
        bytes: wire::to_vec(&PlayerFrame::Hello { wire_version: WIRE_VERSION, client_name: "collision".into() })
            .expect("encode collision Hello"),
    };

    enqueue_rooted(
        &registry,
        &mailer,
        GameGatewayCapability::resolve(0, ()),
        &session_data,
        Source::with_correlation(SourceAddr::Component(source_mailbox), correlation_id),
        root,
    );
    settled.recv_timeout(Duration::from_secs(2)).expect("owner rejection settles after the gateway task completion");

    let turn_sim = TestTurnSim::resolve(0, ());
    let fanout_root = MailId::new(turn_sim, correlation_id + 1);
    let fanout_settled = chassis.settlement_registry().subscribe_settlement(fanout_root);
    enqueue_rooted(
        &registry,
        &mailer,
        GameGatewayCapability::resolve(0, ()),
        &bundle(7),
        Source::with_correlation(SourceAddr::Component(turn_sim), correlation_id + 1),
        fanout_root,
    );
    fanout_settled.recv_timeout(Duration::from_secs(2)).expect("post-rejection live fan-out probe settles");
    assert_eq!(deliveries.load(Ordering::Relaxed), 0, "rejected child never receives bootstrap or live fan-out mail");

    registry.drop_mailbox(collision).expect("remove test-only player-session collision authority");
}

#[test]
fn gateway_refuses_a_new_session_at_configured_capacity() {
    let PlayerTestHarness { registry, observed, chassis: _chassis, listener_port, .. } =
        boot_player_substrate_with_limits(1, GameGatewayConfig::DEFAULT_MAX_PENDING_LIVE_BUNDLES, false);
    let mut first = TcpStream::connect((Ipv4Addr::LOCALHOST, listener_port)).expect("connect first player client");
    first.set_read_timeout(Some(Duration::from_secs(2))).expect("set first-session timeout");
    write_frame(&mut first, &PlayerFrame::Hello { wire_version: WIRE_VERSION, client_name: "first".into() })
        .expect("write first Hello");
    assert!(matches!(
        observed.recv_timeout(Duration::from_secs(2)).expect("first session polls"),
        ObservedSimMail::Poll { .. }
    ));
    assert!(matches!(read_frame::<_, PlayerFrame>(&mut first), Ok(PlayerFrame::HelloAck { .. })));
    expect_fact(&mut first, 1);
    expect_fact(&mut first, 2);

    let mut refused = TcpStream::connect((Ipv4Addr::LOCALHOST, listener_port)).expect("connect refused player client");
    refused.set_read_timeout(Some(Duration::from_secs(2))).expect("set refused-session timeout");
    write_frame(&mut refused, &PlayerFrame::Hello { wire_version: WIRE_VERSION, client_name: "refused".into() })
        .expect("write refused Hello");
    match read_frame::<_, PlayerFrame>(&mut refused) {
        Err(FrameError::Io(error)) => assert!(
            !matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut),
            "capacity refusal must close the trusted TCP session, not merely time out",
        ),
        result => panic!("capacity refusal must close without spawning a player child, got {result:?}"),
    }
    assert!(
        registry.entry(expected_player_session_mailbox("conn-1")).is_none(),
        "the over-capacity connection must not spawn a PlayerSessionActor",
    );
    assert!(
        observed.recv_timeout(Duration::from_millis(100)).is_err(),
        "the over-capacity connection must not poll TurnSim",
    );

    enqueue(&registry, TestTurnSim::resolve(0, ()), &PublishBundle { bundle: bundle(3) });
    expect_fact(&mut first, 3);
}

#[test]
fn catching_up_session_closes_when_the_distinct_live_tick_buffer_is_full() {
    let PlayerTestHarness { registry, observed, chassis: _chassis, listener_port, .. } =
        boot_player_substrate_with_limits(GameGatewayConfig::DEFAULT_MAX_ACTIVE_SESSIONS, 1, true);
    let mut client =
        TcpStream::connect((Ipv4Addr::LOCALHOST, listener_port)).expect("connect catching-up player client");
    client.set_read_timeout(Some(Duration::from_secs(2))).expect("set catching-up timeout");
    write_frame(&mut client, &PlayerFrame::Hello { wire_version: WIRE_VERSION, client_name: "bounded".into() })
        .expect("write bounded Hello");
    assert!(matches!(
        observed.recv_timeout(Duration::from_secs(2)).expect("bounded session polls"),
        ObservedSimMail::Poll { .. }
    ));

    let turn_sim_mailbox = TestTurnSim::resolve(0, ());
    enqueue(&registry, turn_sim_mailbox, &PublishBundle { bundle: bundle(2) });
    enqueue(&registry, turn_sim_mailbox, &PublishBundle { bundle: bundle(2) });
    enqueue(&registry, turn_sim_mailbox, &PublishBundle { bundle: bundle(3) });

    let close: PlayerFrame = read_frame(&mut client).expect("buffer overflow returns a structured close");
    assert_eq!(
        close,
        PlayerFrame::Close { reason: "catch-up live bundle capacity 1 exceeded by tick 3".into() },
        "a duplicate tick may replace the existing slot, while the next distinct tick fails closed",
    );
}

#[test]
fn loopback_session_uses_lineage_ids_and_enforces_the_typed_allowlist() {
    let PlayerTestHarness { registry, observed, chassis: _chassis, listener_port, .. } = boot_player_substrate();
    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, listener_port)).expect("connect player client");
    client.set_read_timeout(Some(Duration::from_secs(2))).expect("set player read timeout");

    write_frame(&mut client, &PlayerFrame::Hello { wire_version: WIRE_VERSION, client_name: "loopback".into() })
        .expect("write player hello");

    let poll = observed.recv_timeout(Duration::from_secs(2)).expect("session polls for catch-up");
    let ObservedSimMail::Poll { mail, source } = poll else {
        panic!("expected catch-up poll, got {poll:?}");
    };
    assert_eq!(mail, Poll { since_tick: 0 });

    let ack: PlayerFrame = read_frame(&mut client).expect("read HelloAck");
    let PlayerFrame::HelloAck { wire_version, session_identity, tick, interval_nanos } = ack else {
        panic!("expected HelloAck, got {ack:?}");
    };
    assert_eq!(wire_version, WIRE_VERSION);
    assert_eq!(tick, 2);
    assert_eq!(interval_nanos, INTERVAL_NANOS);
    assert_eq!(session_identity, expected_player_session_mailbox("conn-0"));
    assert_eq!(source, Some(session_identity), "Poll must originate at the exact ADR-0099 child mailbox");
    assert!(registry.entry(expected_tcp_session_mailbox("conn-0")).is_some());

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
    let observed_spawn = observed.recv_timeout(Duration::from_secs(2)).expect("sim receives stamped spawn");
    let ObservedSimMail::Spawn { mail: stamped, source } = observed_spawn else {
        panic!("expected stamped spawn, got {observed_spawn:?}");
    };
    assert_eq!(source, Some(session_identity));
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

    write_frame(&mut client, &PlayerFrame::Intent { kind: Spawn::ID, payload: vec![0xff] })
        .expect("write malformed allowed intent");
    assert!(
        observed.recv_timeout(Duration::from_millis(100)).is_err(),
        "a malformed allowlisted payload must never dispatch to TurnSim",
    );

    let move_intent = MoveIntent { entity_id: 7, direction: MoveDirection::West };
    write_frame(&mut client, &PlayerFrame::Intent { kind: MoveIntent::ID, payload: move_intent.encode_into_bytes() })
        .expect("write forged move intent");
    let observed_move = observed.recv_timeout(Duration::from_secs(2)).expect("sim receives move");
    let ObservedSimMail::Move { mail: stamped, source } = observed_move else {
        panic!("expected stamped move, got {observed_move:?}");
    };
    assert_eq!(source, Some(session_identity));
    assert_eq!(stamped.entity_id, session_identity.0);
    assert_eq!(stamped.direction, move_intent.direction);

    let mut rejected =
        TcpStream::connect((Ipv4Addr::LOCALHOST, listener_port)).expect("connect rejected player client");
    rejected.set_read_timeout(Some(Duration::from_secs(2))).expect("set rejected-session read timeout");
    write_frame(
        &mut rejected,
        &PlayerFrame::Hello { wire_version: WIRE_VERSION + 1, client_name: "wrong-version".into() },
    )
    .expect("write mismatched hello");
    let close: PlayerFrame = read_frame(&mut rejected).expect("version mismatch returns structured close");
    assert!(matches!(close, PlayerFrame::Close { reason } if reason.contains("wire_version mismatch")));

    enqueue(&registry, TestTurnSim::resolve(0, ()), &PublishBundle { bundle: bundle(3) });
    expect_fact(&mut client, 3);
}
