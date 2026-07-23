//! Real-TCP player client scenarios through the shipped `aether-kit-sim` wasm.
//! The camera the first scenario composes lives in `aether-kit-commons`, so that
//! scenario loads two wasm modules — the camera export from `aether-kit-commons`, the
//! client export from `aether-kit-sim`.

use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use std::fs;
use std::io::Read;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_codec::frame::{read_frame, write_frame};
use aether_component::ComponentHostCapability;
use aether_component::component::resolve_embedded;
use aether_data::{Kind, MailboxId};
use aether_game::{
    GameGatewayCapability, GameGatewayConfig, GameGatewayParams, PlayerFrame, PlayerSessionActor, WIRE_VERSION,
};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::require_runtime;
use aether_harness_substrate_capture::visual::{ColorRegionStats, decode_png, target_color_stats};
use aether_kinds::{DropComponent, DropResult, Key, KeyRelease, LoadComponent, LoadResult, WindowId, keycode};
use aether_kit_commons::camera::{CameraSetMode, ModeInit, OrbitParams};
use aether_kit_sim::{
    EntityState, GridBounds, MoveDirection, MoveIntent, PlayerClientConfig, Poll, PollResult, SimConfig, Spawn,
    StateSummary, TickBundle,
};
use aether_tcp::{ListListeners, ListListenersResult, TcpCapability};

const CAMERA_NAME: &str = "client-camera";
const CLIENT_NAME: &str = "player-client";
const SIM_NAME: &str = "client-turn-sim";
const LISTENER_NAME: &str = "players";
const INTERVAL_NANOS: u64 = 20_000_000;
const TCP_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_WIDTH: u32 = 192;
const FRAME_HEIGHT: u32 = 144;
const TEST_WINDOW_ID: WindowId = WindowId(1);
const ENTITY_SRGB: [u8; 3] = [255, 0, 255];
const ENTITY_COLOR_TOLERANCE: u8 = 4;

#[derive(Debug)]
enum ControlledEvent {
    Handshake { client_name: String, spawn: Spawn },
    Move(MoveIntent),
}

#[derive(Debug)]
enum ControlledCommand {
    StaleSummary,
    NewerSummary,
    ExpectClientClose,
}

fn component_address(name: &str) -> String {
    format!("aether.component/{}:{name}", aether_component::WasmTrampoline::NAMESPACE)
}

fn load_export(harness: &mut SubstrateHarness, wasm: &[u8], export: &str, name: &str, config: Vec<u8>) -> MailboxId {
    let result = harness
        .execute(vec![(
            "load",
            HarnessOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some(name.to_owned()),
                    config,
                    export: Some(export.to_owned()),
                },
            ),
        )])
        .expect("load component sequence")
        .reply::<LoadResult>("load")
        .expect("decode LoadResult");
    match result {
        LoadResult::Ok { mailbox_id, name: loaded_name, .. } => {
            assert_eq!(loaded_name, component_address(name));
            mailbox_id
        }
        LoadResult::Err { error } => panic!("load {export}: {error}"),
    }
}

fn drop_component(harness: &mut SubstrateHarness, mailbox_id: MailboxId) {
    let result = harness
        .execute(vec![(
            "drop",
            HarnessOp::send_and_await(ComponentHostCapability::NAMESPACE, &DropComponent { mailbox_id }),
        )])
        .expect("drop player client");
    match result.reply::<DropResult>("drop").expect("decode DropResult") {
        DropResult::Ok => {}
        DropResult::Err { error } => panic!("drop player client: {error}"),
    }
}

fn controlled_bundle(tick: u64, cell_x: i32) -> TickBundle {
    TickBundle {
        tick,
        superseded_through: tick,
        trajectory: Vec::new(),
        summary: StateSummary { tick, entities: vec![EntityState { entity_id: 77, cell_x, cell_z: 0 }] },
    }
}

fn write_bundle(stream: &mut TcpStream, bundle: &TickBundle) {
    let tick = bundle.tick;
    write_frame(stream, &PlayerFrame::Fact { kind: TickBundle::ID, payload: bundle.encode_into_bytes() })
        .expect("controlled peer writes TickBundle fact");
    write_frame(
        stream,
        &PlayerFrame::Beacon { tick, server_nanos: tick * INTERVAL_NANOS, interval_nanos: INTERVAL_NANOS },
    )
    .expect("controlled peer writes tick beacon");
}

#[allow(clippy::disallowed_methods)] // Native loopback peer is test infrastructure outside the actor settlement tree.
fn spawn_controlled_peer(
    listener: TcpListener,
    session_identity: MailboxId,
) -> (mpsc::Receiver<ControlledEvent>, mpsc::Sender<ControlledCommand>, thread::JoinHandle<()>) {
    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("controlled peer accepts client");
        stream.set_read_timeout(Some(TCP_TIMEOUT)).expect("set controlled peer read timeout");
        stream.set_write_timeout(Some(TCP_TIMEOUT)).expect("set controlled peer write timeout");

        let hello: PlayerFrame = read_frame(&mut stream).expect("controlled peer reads Hello");
        let PlayerFrame::Hello { wire_version, client_name } = hello else {
            panic!("expected Hello, got {hello:?}");
        };
        assert_eq!(wire_version, WIRE_VERSION);
        write_frame(
            &mut stream,
            &PlayerFrame::HelloAck {
                wire_version: WIRE_VERSION,
                session_identity,
                tick: 0,
                interval_nanos: INTERVAL_NANOS,
            },
        )
        .expect("controlled peer writes HelloAck");

        let spawn: PlayerFrame = read_frame(&mut stream).expect("controlled peer reads Spawn intent");
        let PlayerFrame::Intent { kind, payload } = spawn else {
            panic!("expected Spawn intent, got {spawn:?}");
        };
        assert_eq!(kind, Spawn::ID);
        let spawn = Spawn::decode_from_bytes(&payload).expect("decode controlled Spawn intent");
        write_bundle(&mut stream, &controlled_bundle(1, 0));
        event_tx.send(ControlledEvent::Handshake { client_name, spawn }).expect("report controlled handshake");

        let movement: PlayerFrame = read_frame(&mut stream).expect("controlled peer reads MoveIntent");
        let PlayerFrame::Intent { kind, payload } = movement else {
            panic!("expected MoveIntent, got {movement:?}");
        };
        assert_eq!(kind, MoveIntent::ID);
        event_tx
            .send(ControlledEvent::Move(MoveIntent::decode_from_bytes(&payload).expect("decode controlled MoveIntent")))
            .expect("report controlled movement");

        assert!(matches!(
            command_rx.recv_timeout(TCP_TIMEOUT).expect("receive stale command"),
            ControlledCommand::StaleSummary
        ));
        write_bundle(&mut stream, &controlled_bundle(1, 1));

        assert!(matches!(
            command_rx.recv_timeout(TCP_TIMEOUT).expect("receive newer command"),
            ControlledCommand::NewerSummary
        ));
        write_bundle(&mut stream, &controlled_bundle(2, 1));

        assert!(matches!(
            command_rx.recv_timeout(TCP_TIMEOUT).expect("receive client-close command"),
            ControlledCommand::ExpectClientClose
        ));
        let mut trailing = [0_u8; 1];
        assert_eq!(stream.read(&mut trailing).expect("controlled peer observes client teardown"), 0);
    });
    (event_rx, command_tx, handle)
}

fn capture_entity(harness: &mut SubstrateHarness, label: &'static str) -> ColorRegionStats {
    let captured = harness
        .execute(vec![("settle", HarnessOp::advance(3)), (label, HarnessOp::capture())])
        .expect("settle and capture client scene");
    let image = decode_png(captured.captured(label).expect("capture client frame")).expect("decode client frame");
    target_color_stats(&image, ENTITY_SRGB, ENTITY_COLOR_TOLERANCE, None)
}

fn wait_for_authoritative_entity(harness: &mut SubstrateHarness, sim_address: &str) -> EntityState {
    let deadline = Instant::now() + TCP_TIMEOUT;
    loop {
        harness.execute(vec![("advance", HarnessOp::advance(1))]).expect("advance player loop");
        let poll = harness
            .execute(vec![("poll", HarnessOp::send_and_await(sim_address, &Poll { since_tick: 0 }))])
            .expect("poll TurnSim")
            .reply::<PollResult>("poll")
            .expect("decode PollResult");
        if let Some(entity) = poll.bundles.last().and_then(|bundle| bundle.summary.entities.first()).copied() {
            return entity;
        }
        assert!(Instant::now() < deadline, "client Spawn never reached TurnSim");
        thread::sleep(Duration::from_millis(10));
    }
}

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
            "gateway listener did not bind uniquely: expected one {LISTENER_NAME:?} entry, got \
             {matching_listener_count}: {:?}",
            list.listeners
        );
        if let Some(listener) = list.listeners.iter().find(|listener| listener.name == LISTENER_NAME) {
            assert!(listener.port > 0, "gateway listener bound an invalid local port");
            return listener.port;
        }
        assert!(
            Instant::now() < deadline,
            "gateway listener did not bind within two seconds before loading the player client; live listeners: {:?}",
            list.listeners
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn controlled_peer_proves_framing_input_and_atomic_visual_replacement() {
    let Some(camera_wasm_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let Some(client_wasm_path) = require_runtime("aether_kit_sim") else {
        return;
    };
    let camera_wasm = fs::read(camera_wasm_path).expect("read aether-kit-commons wasm");
    let client_wasm = fs::read(client_wasm_path).expect("read aether-kit-sim wasm");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind controlled loopback peer");
    let server_addr = listener.local_addr().expect("controlled peer address").to_string();
    let session_identity = resolve_embedded("controlled-player-session");
    let (event_rx, command_tx, peer) = spawn_controlled_peer(listener, session_identity);
    let mut harness = SubstrateHarness::builder()
        .size(FRAME_WIDTH, FRAME_HEIGHT)
        .with_render()
        .with_component_host()
        .with_actor::<TcpCapability>(())
        .build()
        .expect("boot controlled client SubstrateHarness");

    load_export(&mut harness, &camera_wasm, "aether.kit.camera", CAMERA_NAME, Vec::new());
    harness
        .execute(vec![(
            "camera",
            HarnessOp::send_mail(
                component_address(CAMERA_NAME),
                &CameraSetMode {
                    name: "main".into(),
                    mode: ModeInit::Orbit(OrbitParams {
                        distance: Some(7.0),
                        pitch: Some(-1.2),
                        yaw: Some(0.0),
                        speed: Some(0.0),
                        fov_y_rad: Some(0.9),
                        target: Some([1.0, 0.0, 0.5]),
                    }),
                },
            ),
        )])
        .expect("configure client camera");
    let client_mailbox = load_export(
        &mut harness,
        &client_wasm,
        "aether.kit.client",
        CLIENT_NAME,
        PlayerClientConfig {
            server_addr,
            client_name: "controlled".into(),
            spawn_cell: aether_kit_sim::CellPosition { cell_x: 0, cell_z: 0 },
            grid_bounds: GridBounds { min_cell_x: -1, max_cell_x: 2, min_cell_z: -1, max_cell_z: 1 },
        }
        .encode_into_bytes(),
    );

    let ControlledEvent::Handshake { client_name, spawn } =
        event_rx.recv_timeout(TCP_TIMEOUT).expect("controlled handshake completes")
    else {
        panic!("controlled peer reported movement before handshake");
    };
    assert_eq!(client_name, "controlled");
    assert_eq!(spawn.entity_id, session_identity.0);
    assert_eq!((spawn.cell_x, spawn.cell_z), (0, 0));
    thread::sleep(Duration::from_millis(25));
    let initial = capture_entity(&mut harness, "initial");
    assert!(initial.matching > 20, "expected visible magenta marker at initial cell: {initial:?}");

    let press_east = Key { window: TEST_WINDOW_ID, code: keycode::KEY_D };
    let release_east = KeyRelease { window: TEST_WINDOW_ID, code: keycode::KEY_D };
    harness
        .execute(vec![
            ("press-east", HarnessOp::window_event(TEST_WINDOW_ID, &press_east)),
            ("emit-move", HarnessOp::advance(1)),
            ("release-east", HarnessOp::window_event(TEST_WINDOW_ID, &release_east)),
        ])
        .expect("drive held east input through the synthetic window");
    let ControlledEvent::Move(intent) = event_rx.recv_timeout(TCP_TIMEOUT).expect("controlled MoveIntent arrives")
    else {
        panic!("controlled peer reported a second handshake");
    };
    assert_eq!(intent.entity_id, session_identity.0);
    assert_eq!(intent.direction, MoveDirection::East);

    command_tx.send(ControlledCommand::StaleSummary).expect("ask peer for stale summary");
    thread::sleep(Duration::from_millis(25));
    let stale = capture_entity(&mut harness, "stale");
    let initial_centroid = initial.centroid.expect("initial marker centroid");
    let stale_centroid = stale.centroid.expect("stale marker centroid");
    assert!(
        (stale_centroid.x - initial_centroid.x).abs() <= 2.0 && (stale_centroid.y - initial_centroid.y).abs() <= 2.0,
        "stale complete summary must not move the marker: initial={initial:?}, stale={stale:?}"
    );

    command_tx.send(ControlledCommand::NewerSummary).expect("ask peer for newer summary");
    thread::sleep(Duration::from_millis(25));
    let moved = capture_entity(&mut harness, "moved");
    let moved_centroid = moved.centroid.expect("moved marker centroid");
    assert!(
        moved_centroid.x > initial_centroid.x + 8.0,
        "newer complete summary must move the marker east: initial={initial:?}, moved={moved:?}"
    );

    command_tx.send(ControlledCommand::ExpectClientClose).expect("ask peer to observe client teardown");
    drop_component(&mut harness, client_mailbox);
    peer.join().expect("controlled peer exits cleanly");
}

#[test]
fn active_gateway_turn_sim_loop_spawns_and_moves_the_server_identity() {
    let Some(wasm_path) = require_wasm("aether_kit_sim") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read aether-kit-sim wasm");
    let sim_mailbox = resolve_embedded(SIM_NAME);
    let mut harness = SubstrateHarness::builder()
        .with_component_host()
        .with_actor::<TcpCapability>(())
        .size(FRAME_WIDTH, FRAME_HEIGHT)
        .with_actor_configured::<GameGatewayCapability>(
            GameGatewayParams { turn_sim_mailbox: Some(sim_mailbox) },
            GameGatewayConfig {
                listener_addr: Some("127.0.0.1:0".into()),
                listener_name: LISTENER_NAME.into(),

                interval_nanos: INTERVAL_NANOS,
                max_active_sessions: GameGatewayConfig::DEFAULT_MAX_ACTIVE_SESSIONS,
                max_pending_live_bundles: GameGatewayConfig::DEFAULT_MAX_PENDING_LIVE_BUNDLES,
            },
        )
        .build()
        .expect("boot active gateway SubstrateHarness");
    let listener_port = gateway_listener_port(&mut harness);
    load_export(
        &mut harness,
        &wasm,
        "aether.kit.sim",
        SIM_NAME,
        SimConfig {
            fact_sink: Some(GameGatewayCapability::resolve(0, ())),
            ring_depth: 16,
            grid_bounds: GridBounds::default(),
        }
        .encode_into_bytes(),
    );
    load_export(
        &mut harness,
        &wasm,
        "aether.kit.client",
        CLIENT_NAME,
        PlayerClientConfig {
            server_addr: format!("{}:{listener_port}", Ipv4Addr::LOCALHOST),
            client_name: "gateway-loop".into(),
            spawn_cell: aether_kit_sim::CellPosition { cell_x: 1, cell_z: 1 },
            grid_bounds: GridBounds::default(),
        }
        .encode_into_bytes(),
    );

    let sim_address = component_address(SIM_NAME);
    let spawned = wait_for_authoritative_entity(&mut harness, &sim_address);
    assert_eq!(
        spawned.entity_id,
        PlayerSessionActor::resolve(GameGatewayCapability::resolve(0, ()).0, "conn-0").0,
        "the full client-to-sim loop runs under the server-assigned player-session identity",
    );
    assert_eq!((spawned.cell_x, spawned.cell_z), (1, 1));

    harness
        .execute(vec![
            (
                "press-west",
                HarnessOp::window_event(TEST_WINDOW_ID, &Key { window: TEST_WINDOW_ID, code: keycode::KEY_A }),
            ),
            ("emit-west", HarnessOp::advance(1)),
            (
                "release-west",
                HarnessOp::window_event(TEST_WINDOW_ID, &KeyRelease { window: TEST_WINDOW_ID, code: keycode::KEY_A }),
            ),
        ])
        .expect("drive west input through active gateway");

    let deadline = Instant::now() + TCP_TIMEOUT;
    loop {
        let entity = wait_for_authoritative_entity(&mut harness, &sim_address);
        if (entity.cell_x, entity.cell_z) == (0, 1) {
            assert_eq!(entity.entity_id, spawned.entity_id);
            break;
        }
        assert!(Instant::now() < deadline, "client MoveIntent never traversed gateway and TurnSim");
        thread::sleep(Duration::from_millis(10));
    }
}
