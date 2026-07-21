//! Real `aether.tcp` + game gateway + `TurnSim` acceptance coverage.
//!
//! Minimal composition (issue #3764): the three-crate integration needs
//! the component host (`TurnSim` wasm), tcp (real loopback sessions), and
//! the active gateway on the harness basics — every assertion reads the
//! `PlayerFrame` stream off the socket, so no render cap (and no wgpu
//! gate) is composed; the sim's frame output warn-drops harmlessly.

#![allow(clippy::print_stderr)]

use std::fs;
use std::net::{Ipv4Addr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_codec::frame::{read_frame, write_frame};
use aether_component::component::resolve_embedded;
use aether_data::{Kind, MailboxId};
use aether_game::{
    GameGatewayCapability, GameGatewayConfig, GameGatewayParams, PlayerFrame, PlayerSessionActor, WIRE_VERSION,
};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{LoadComponent, LoadResult};
use aether_kit::{GridBounds, MoveDirection, MoveIntent, Poll, SimConfig, Spawn, TickBundle};
use aether_tcp::TcpCapability;
use aether_tcp::{ListListeners, ListListenersResult};

const SIM_NAME: &str = "player-turn-sim";
const LISTENER_NAME: &str = "players";
const INTERVAL_NANOS: u64 = 20_000_000;
const TCP_TIMEOUT: Duration = Duration::from_secs(10);

fn gateway_listener_port(harness: &mut SubstrateHarness) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let list = harness
            .execute(vec![("list-player-listener", HarnessOp::send_and_await("aether.tcp", &ListListeners::default()))])
            .expect("list game gateway listener")
            .reply::<ListListenersResult>("list-player-listener")
            .expect("decode game gateway listener list");
        let matching_listener_count = list.listeners.iter().filter(|listener| listener.name == LISTENER_NAME).count();
        assert!(
            matching_listener_count <= 1,
            "gateway listener did not bind uniquely: expected one {LISTENER_NAME:?} entry, got {matching_listener_count}: {:?}",
            list.listeners
        );
        if let Some(listener) = list.listeners.iter().find(|listener| listener.name == LISTENER_NAME) {
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
    PlayerSessionActor::resolve(GameGatewayCapability::resolve(0, ()).0, session_name)
}

fn load_turn_sim(harness: &mut SubstrateHarness, wasm: Vec<u8>) {
    let config = SimConfig {
        fact_sink: Some(GameGatewayCapability::resolve(0, ())),
        ring_depth: 8,
        grid_bounds: GridBounds::default(),
    };
    let loaded = harness
        .execute(vec![(
            "load-turn-sim",
            HarnessOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm,
                    name: Some(SIM_NAME.into()),
                    config: config.encode_into_bytes(),
                    export: Some("aether.kit.sim".into()),
                },
            ),
        )])
        .expect("load TurnSim sequence")
        .reply::<LoadResult>("load-turn-sim")
        .expect("decode TurnSim LoadResult");

    match loaded {
        LoadResult::Ok { mailbox_id, .. } => assert_eq!(mailbox_id, resolve_embedded(SIM_NAME)),
        LoadResult::Err { error } => panic!("load TurnSim: {error}"),
    }
}

fn hello(stream: &mut TcpStream, client_name: &str) -> (MailboxId, u64) {
    write_frame(stream, &PlayerFrame::Hello { wire_version: WIRE_VERSION, client_name: client_name.into() })
        .expect("write Hello");
    let ack: PlayerFrame = read_frame(stream).expect("read HelloAck");
    let PlayerFrame::HelloAck { wire_version, session_identity, tick, interval_nanos } = ack else {
        panic!("expected HelloAck, got {ack:?}");
    };
    assert_eq!(wire_version, WIRE_VERSION);
    assert_eq!(interval_nanos, INTERVAL_NANOS);
    (session_identity, tick)
}

fn read_bundle_and_beacon(stream: &mut TcpStream) -> TickBundle {
    let fact: PlayerFrame = read_frame(stream).expect("read fact frame");
    let PlayerFrame::Fact { kind, payload } = fact else {
        panic!("expected fact frame, got {fact:?}");
    };
    assert_eq!(kind, TickBundle::ID);
    let bundle = TickBundle::decode_from_bytes(&payload).expect("decode TickBundle fact");

    let beacon: PlayerFrame = read_frame(stream).expect("read beacon frame");
    let PlayerFrame::Beacon { tick, interval_nanos, .. } = beacon else {
        panic!("expected beacon frame, got {beacon:?}");
    };
    assert_eq!(tick, bundle.tick);
    assert_eq!(interval_nanos, INTERVAL_NANOS);
    bundle
}

fn advance(harness: &mut SubstrateHarness) {
    harness.execute(vec![("advance", HarnessOp::advance(1))]).expect("advance TurnSim one tick");
}

#[test]
fn real_turn_sim_gateway_stamps_identity_and_streams_catch_up_and_live_bundles() {
    let Some(wasm_path) = require_wasm("aether_kit") else {
        return;
    };
    let turn_sim_mailbox = resolve_embedded(SIM_NAME);
    let mut harness = SubstrateHarness::builder()
        .with_component_host()
        .with_actor::<TcpCapability>(())
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
        .build()
        .expect("boot active game gateway SubstrateHarness");
    let listener_port = gateway_listener_port(&mut harness);
    load_turn_sim(&mut harness, fs::read(wasm_path).expect("read aether-kit wasm"));

    let mut first = TcpStream::connect((Ipv4Addr::LOCALHOST, listener_port)).expect("connect first player client");
    first.set_read_timeout(Some(TCP_TIMEOUT)).expect("set first client timeout");
    let (identity, tick) = hello(&mut first, "first");
    assert_eq!(tick, 0);
    assert_eq!(identity, expected_player_session_mailbox("conn-0"));

    write_frame(
        &mut first,
        &PlayerFrame::Intent {
            kind: Spawn::ID,
            payload: Spawn { entity_id: 0xdead_beef, cell_x: 1, cell_z: 1 }.encode_into_bytes(),
        },
    )
    .expect("write forged Spawn");
    thread::sleep(Duration::from_millis(25));

    let spawned = loop {
        advance(&mut harness);
        let bundle = read_bundle_and_beacon(&mut first);
        if bundle.summary.entities.iter().any(|entity| entity.entity_id == identity.0) {
            break bundle;
        }
        assert!(bundle.tick < 4, "forged Spawn never reached TurnSim under the assigned identity");
    };
    let entity = spawned
        .summary
        .entities
        .iter()
        .find(|entity| entity.entity_id == identity.0)
        .expect("spawned identity remains in the authoritative summary");
    assert_eq!((entity.cell_x, entity.cell_z), (1, 1));
    assert!(!spawned.summary.entities.iter().any(|entity| entity.entity_id == 0xdead_beef));

    write_frame(
        &mut first,
        &PlayerFrame::Intent {
            kind: MoveIntent::ID,
            payload: MoveIntent { entity_id: 7, direction: MoveDirection::West }.encode_into_bytes(),
        },
    )
    .expect("write forged MoveIntent");
    thread::sleep(Duration::from_millis(25));
    let moved = loop {
        advance(&mut harness);
        let bundle = read_bundle_and_beacon(&mut first);
        let entity = bundle
            .summary
            .entities
            .iter()
            .find(|entity| entity.entity_id == identity.0)
            .expect("spawned identity remains while moving");
        if (entity.cell_x, entity.cell_z) == (0, 1) {
            break bundle;
        }
        assert!(bundle.tick <= spawned.tick + 3, "MoveIntent never reached TurnSim");
    };

    write_frame(
        &mut first,
        &PlayerFrame::Intent { kind: Poll::ID, payload: Poll { since_tick: 0 }.encode_into_bytes() },
    )
    .expect("write non-allowlisted kind");
    thread::sleep(Duration::from_millis(25));
    advance(&mut harness);
    let after_unknown = read_bundle_and_beacon(&mut first);
    let entity = after_unknown
        .summary
        .entities
        .iter()
        .find(|entity| entity.entity_id == identity.0)
        .expect("spawned identity remains after unknown kind");
    assert_eq!((entity.cell_x, entity.cell_z), (0, 1), "unknown kind must produce no sim action");

    let mut second = TcpStream::connect((Ipv4Addr::LOCALHOST, listener_port)).expect("connect second player client");
    second.set_read_timeout(Some(TCP_TIMEOUT)).expect("set second client timeout");
    let (second_identity, current_tick) = hello(&mut second, "second");
    assert_eq!(second_identity, expected_player_session_mailbox("conn-1"));
    assert_eq!(current_tick, after_unknown.tick);
    for expected_tick in 1..=current_tick {
        assert_eq!(read_bundle_and_beacon(&mut second).tick, expected_tick);
    }
    assert!(moved.tick < after_unknown.tick);
}
