//! Downstream TestBench consumer for the player session tier (ADR-0144/0145).
//!
//! Boots an active loopback `GameGatewayCapability`, loads a real
//! `aether.kit.sim`, a camera, and two named `aether.kit.client` instances with
//! distinct spawn cells. The clients dial the gateway over real TCP, run the
//! recipient-free `PlayerFrame` handshake, and render the authoritative scene.
//! A held W key is injected via `aether.input`; the rendered client view is then
//! captured to a PNG.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use aether_actor::Addressable;
use aether_capabilities::component::resolve_embedded;
use aether_capabilities::tcp::{ListListeners, ListListenersResult};
use aether_capabilities::{GameGatewayCapability, GameGatewayConfig, WasmTrampoline};
use aether_data::Kind;
use aether_kinds::{Key, LoadComponent, LoadResult, NamedMail, Render, keycode};
use aether_kit::camera::{CameraOrbitSet, OrbitParams};
use aether_kit::{CellPosition, GridBounds, PlayerClientConfig, SimConfig};
use aether_substrate_bundle::test_bench::test_helpers::has_wgpu_adapter;
use aether_substrate_bundle::test_bench::{BenchOp, TestBench};

const WASM_PATH: &str = "/home/runner/_work/aether/aether/target/wasm32-unknown-unknown/release/aether_kit.wasm";
const SIM_NAME: &str = "player-turn-sim";
const CAMERA_NAME: &str = "camera";
const LISTENER_NAME: &str = "players";
const INTERVAL_NANOS: u64 = 20_000_000;

fn embedded_addr(name: &str) -> String {
    format!("aether.component/{}:{name}", WasmTrampoline::NAMESPACE)
}

fn named<K: Kind>(recipient: &str, mail: &K) -> NamedMail {
    NamedMail {
        recipient_name: recipient.to_owned(),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

fn gateway_listener_port(bench: &mut TestBench) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let list = bench
            .execute(vec![("list", BenchOp::send_and_await("aether.tcp", &ListListeners::default()))])
            .expect("list listeners")
            .reply::<ListListenersResult>("list")
            .expect("decode listener list");
        if let Some(listener) = list.listeners.iter().find(|l| l.name == LISTENER_NAME) {
            assert!(listener.port > 0, "gateway listener bound an invalid port");
            return listener.port;
        }
        assert!(Instant::now() < deadline, "gateway listener never bound: {:?}", list.listeners);
        thread::sleep(Duration::from_millis(10));
    }
}

fn load(bench: &mut TestBench, wasm: &[u8], name: &str, export: &str, config: Vec<u8>) {
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent {
                    wasm: wasm.to_vec(),
                    name: Some(name.to_owned()),
                    config,
                    export: Some(export.to_owned()),
                },
            ),
        )])
        .expect("load sequence")
        .reply::<LoadResult>("load")
        .expect("decode LoadResult");
    match loaded {
        LoadResult::Ok { mailbox_id, .. } => {
            assert_eq!(mailbox_id, resolve_embedded(name), "loaded {name} at unexpected mailbox");
            eprintln!("loaded {name} ({export}) at {mailbox_id}");
        }
        LoadResult::Err { error } => panic!("load {name}: {error}"),
    }
}

fn main() {
    if !has_wgpu_adapter() {
        eprintln!("SKIP: no wgpu adapter available; cannot boot TestBench render target");
        return;
    }

    let wasm = std::fs::read(WASM_PATH).expect("read prebuilt aether_kit.wasm");
    let gateway = GameGatewayCapability::resolve(0, ());
    let turn_sim_mailbox = resolve_embedded(SIM_NAME);

    let mut bench = TestBench::builder()
        .size(256, 256)
        .game_gateway(GameGatewayConfig {
            listener_addr: Some("127.0.0.1:0".into()),
            listener_name: LISTENER_NAME.into(),
            turn_sim_mailbox: Some(turn_sim_mailbox),
            interval_nanos: INTERVAL_NANOS,
            max_active_sessions: GameGatewayConfig::DEFAULT_MAX_ACTIVE_SESSIONS,
            max_pending_live_bundles: GameGatewayConfig::DEFAULT_MAX_PENDING_LIVE_BUNDLES,
        })
        .build()
        .expect("boot active game gateway TestBench");

    let port = gateway_listener_port(&mut bench);
    eprintln!("gateway listener bound on 127.0.0.1:{port}");

    // Real authoritative simulation, pushing every tick bundle to the gateway.
    let sim_config = SimConfig { fact_sink: Some(gateway), ring_depth: 64, grid_bounds: GridBounds::default() };
    load(&mut bench, &wasm, SIM_NAME, "aether.kit.sim", sim_config.encode_into_bytes());

    // Projection lives in a separately-loaded camera actor.
    load(&mut bench, &wasm, CAMERA_NAME, "aether.kit.camera", Vec::new());

    // Two player clients with distinct spawn cells; each dials the gateway.
    let clients = [
        ("client-alpha", "alpha", CellPosition { cell_x: -2, cell_z: 2 }),
        ("client-bravo", "bravo", CellPosition { cell_x: 2, cell_z: 2 }),
    ];
    let server_addr = format!("127.0.0.1:{port}");
    for (name, client_name, spawn_cell) in clients {
        let config = PlayerClientConfig {
            server_addr: server_addr.clone(),
            client_name: client_name.to_owned(),
            spawn_cell,
            grid_bounds: GridBounds::default(),
        };
        load(&mut bench, &wasm, name, "aether.kit.client", config.encode_into_bytes());
    }

    // Aim the boot "main" camera down at the grid.
    bench
        .execute(vec![(
            "aim",
            BenchOp::send_mail(
                &embedded_addr(CAMERA_NAME),
                &CameraOrbitSet {
                    name: "main".to_owned(),
                    params: OrbitParams {
                        distance: Some(14.0),
                        pitch: Some(-1.3),
                        yaw: Some(0.0),
                        speed: Some(0.0),
                        fov_y_rad: Some(0.9),
                        target: Some([0.0, 0.0, 0.0]),
                    },
                },
            ),
        )])
        .expect("aim camera");

    // Let the real TCP handshakes complete and the first bundles flow. TCP runs
    // on its own threads, so interleave wall-clock sleeps with sim turns.
    for _ in 0..40 {
        bench.execute(vec![("tick", BenchOp::advance(1))]).expect("advance handshake");
        thread::sleep(Duration::from_millis(15));
    }

    // Inject a held W (press, no release) — both clients emit a North MoveIntent
    // every tick while held.
    bench
        .execute(vec![("hold-w", BenchOp::send_mail("aether.input", &Key { code: keycode::KEY_W }))])
        .expect("inject held W");
    for _ in 0..12 {
        bench.execute(vec![("walk", BenchOp::advance(1))]).expect("advance walk");
        thread::sleep(Duration::from_millis(15));
    }

    let client_addrs: Vec<String> = clients.iter().map(|(name, ..)| embedded_addr(name)).collect();
    let redraw: Vec<NamedMail> = client_addrs.iter().map(|addr| named(addr, &Render)).collect();
    let result = bench
        .execute(vec![("frame", BenchOp::capture_with_mails(redraw, Vec::new()))])
        .expect("capture sequence");
    let png = result.captured("frame").expect("captured PNG present");

    let out = PathBuf::from("/tmp/dogfood-player-view.png");
    std::fs::write(&out, png).expect("write PNG");
    eprintln!("captured {} bytes -> {}", png.len(), out.display());
    eprintln!(
        "observed tick_bundle facts: {}",
        bench.count_observed("aether.sim.tick_bundle")
    );
    eprintln!("DONE");
}
