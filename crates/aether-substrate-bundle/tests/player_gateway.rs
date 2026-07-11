//! Real `aether.tcp` + game gateway + `TurnSim` acceptance coverage.

#![allow(clippy::print_stderr)]

use std::fs;
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_capabilities::GameGatewayConfig;
use aether_capabilities::component::resolve_embedded;
use aether_capabilities::game::{GameGatewayCapability, PlayerFrame, PlayerSessionActor, WIRE_VERSION};
use aether_codec::frame::{read_frame, write_frame};
use aether_data::{ActorId, Kind, MailboxId, Tag, fold_lineage, with_tag};
use aether_kinds::{LoadComponent, LoadResult};
use aether_kit::{GridBounds, MoveDirection, MoveIntent, Poll, SimConfig, Spawn, TickBundle};
use aether_substrate_bundle::test_bench::{BenchOp, TestBench, test_helpers::require_runtime};

const SIM_NAME: &str = "player-turn-sim";
const LISTENER_NAME: &str = "players";
const INTERVAL_NANOS: u64 = 20_000_000;

fn reserve_loopback_addr() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve loopback port");
    listener.local_addr().expect("reserved listener address").to_string()
}

fn connect_when_bound(addr: &str) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match TcpStream::connect(addr) {
            Ok(stream) => return stream,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("connect player client to {addr}: {error}"),
        }
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

fn load_turn_sim(bench: &mut TestBench, wasm: Vec<u8>) {
    let config = SimConfig {
        fact_sink: Some(GameGatewayCapability::resolve(0, ())),
        ring_depth: 8,
        grid_bounds: GridBounds::default(),
    };
    let loaded = bench
        .execute(vec![(
            "load-turn-sim",
            BenchOp::send_and_await(
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

fn advance(bench: &mut TestBench) {
    bench.execute(vec![("advance", BenchOp::advance(1))]).expect("advance TurnSim one tick");
}

#[test]
fn real_turn_sim_gateway_stamps_identity_and_streams_catch_up_and_live_bundles() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let listener_addr = reserve_loopback_addr();
    let turn_sim_mailbox = resolve_embedded(SIM_NAME);
    let mut bench = TestBench::builder()
        .size(96, 96)
        .game_gateway(GameGatewayConfig {
            listener_addr: Some(listener_addr.clone()),
            listener_name: LISTENER_NAME.into(),
            turn_sim_mailbox: Some(turn_sim_mailbox),
            interval_nanos: INTERVAL_NANOS,
        })
        .build()
        .expect("boot active game gateway TestBench");
    load_turn_sim(&mut bench, fs::read(wasm_path).expect("read aether-kit wasm"));

    let mut first = connect_when_bound(&listener_addr);
    first.set_read_timeout(Some(Duration::from_secs(2))).expect("set first client timeout");
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
        advance(&mut bench);
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
        advance(&mut bench);
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
    advance(&mut bench);
    let after_unknown = read_bundle_and_beacon(&mut first);
    let entity = after_unknown
        .summary
        .entities
        .iter()
        .find(|entity| entity.entity_id == identity.0)
        .expect("spawned identity remains after unknown kind");
    assert_eq!((entity.cell_x, entity.cell_z), (0, 1), "unknown kind must produce no sim action");

    let mut second = connect_when_bound(&listener_addr);
    second.set_read_timeout(Some(Duration::from_secs(2))).expect("set second client timeout");
    let (second_identity, current_tick) = hello(&mut second, "second");
    assert_eq!(second_identity, expected_player_session_mailbox("conn-1"));
    assert_eq!(current_tick, after_unknown.tick);
    for expected_tick in 1..=current_tick {
        assert_eq!(read_bundle_and_beacon(&mut second).tick, expected_tick);
    }
    assert!(moved.tick < after_unknown.tick);
}
