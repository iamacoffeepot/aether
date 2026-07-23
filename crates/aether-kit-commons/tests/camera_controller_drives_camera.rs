//! Acceptance: a held key drives the keyboard camera controller, which steers
//! the peer camera component, which scrolls the rendered view (issue 2820).
//!
//! Loads three `aether-kit-commons` actors — `WorldView` (paints the ground),
//! `CameraComponent` (the projection state machine, loaded under its default
//! name `aether.kit.camera`), and `CameraController` (the keyboard driver, loaded
//! with an init-config) — paints a high-contrast chunk, and captures three
//! frames: after the controller seeds the camera (idle), after a held `D`
//! pans the orbit target across the ground, and after the key is released.
//! The controller owns no pixels; the honest rendered signal that the whole
//! `key → controller → aether.kit.camera.orbit.set → camera → view_proj → world`
//! chain composed is that the pan frame differs from the seeded frame, while
//! the released frame matches the pan frame (the zero-mail-idle invariant, end
//! to end). The controller's per-tick integration math is pinned by its own
//! unit tests; this is the composition-and-motion proof the harness split
//! routes to `SubstrateHarness`.
//!
//! Skipped when no wgpu adapter is available or the `aether_kit_commons` wasm has not
//! been pre-built (the shared `require_runtime` gate). CI sets
//! `AETHER_REQUIRE_RUNTIME=1` to turn either skip into a hard failure.

// Integration-test skip diagnostic: emit via stderr so `cargo test` surfaces
// "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]

use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use std::fs;

use aether_actor::Addressable;
use aether_data::Kind;
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::require_runtime;
use aether_harness_substrate_capture::visual::{background_top_left, coverage, decode_png, mean_absolute_error};
use aether_kinds::keycode::KEY_D;
use aether_kinds::{Key, KeyRelease, LoadComponent, LoadResult, NamedMail, Render, WindowId, WindowSize};
use aether_kit_commons::camera::controller::ControllerConfig;
use aether_kit_terrain::SetChunk;
use aether_kit_terrain::world::Material;

/// Capture surface — a 4:3 frame the camera's aspect matches once the
/// `WindowSize` below lands.
const WINDOW_WIDTH: u32 = 128;
const WINDOW_HEIGHT: u32 = 96;
const TEST_WINDOW_ID: WindowId = WindowId(1);

/// Chunk edge in cells (`CELLS_PER_CHUNK`), mirrored here so the split-paint
/// plane below is the right length.
const CHUNK_EDGE: usize = 16;

/// The full trampoline address a loaded component registers at (ADR-0099 §4):
/// the component host `/`-joined to the trampoline node under `name`.
fn component_address(name: &str) -> String {
    format!("aether.component/{}:{name}", aether_component::WasmTrampoline::NAMESPACE)
}

/// A `NamedMail` carrying `mail`'s wire encoding to `recipient` — the capture
/// bundle / input-injection envelope.
fn envelope<K: Kind>(recipient: &str, mail: &K) -> NamedMail {
    NamedMail {
        recipient_name: recipient.to_owned(),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

/// Load one `aether_kit_commons` export under `name` with optional init-config bytes,
/// blocking on `LoadResult` so the component is instantiated and subscribed
/// before the next op.
fn load_kit_export(harness: &mut SubstrateHarness, wasm: &[u8], export: &str, name: &str, config: Vec<u8>) {
    let loaded = harness
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
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { name: addr, .. } => {
            assert!(addr.ends_with(&format!(":{name}")), "export {export} should register under :{name}; got {addr}");
        }
        LoadResult::Err { error } => panic!("load {export}: {error}"),
    }
}

/// A single chunk 0 underlay plane split down the middle — grass on the west
/// half, stone on the east — so a sharp, world-anchored material boundary sits
/// in the scene. That boundary (plus the chunk's edges against the background)
/// is the high-contrast feature the camera scrolls over as the target pans.
fn split_chunk() -> SetChunk {
    let mut underlay = vec![0u8; CHUNK_EDGE * CHUNK_EDGE];
    for z in 0..CHUNK_EDGE {
        for x in 0..CHUNK_EDGE {
            let material = if x < CHUNK_EDGE / 2 {
                Material::Grass
            } else {
                Material::Stone
            };
            underlay[z * CHUNK_EDGE + x] = material.to_u8();
        }
    }
    SetChunk {
        chunk_x: 0,
        chunk_z: 0,
        underlay,
        underlay_points: Vec::new(),
        height_points: Vec::new(),
        overlay: Vec::new(),
        overlay_mask: Vec::new(),
        height: Vec::new(),
        region: Vec::new(),
        water_plane: Vec::new(),
        smoothing: Vec::new(),
    }
}

/// Capture one frame that draws both the world ground and the camera's
/// projection: the camera's `Render` publishes its (controller-driven)
/// `view_proj`, the world-view's `Render` replays the painted ground under it,
/// both into the accumulator right before the GPU readback.
fn capture_scene(harness: &mut SubstrateHarness, camera: &str, world: &str, label: &'static str) -> Vec<u8> {
    let pre = vec![envelope(camera, &Render), envelope(world, &Render)];
    let captured =
        harness.execute(vec![(label, HarnessOp::capture_with_mails(pre, Vec::new()))]).expect("capture-with-mails");
    captured.captured(label).expect("capture step ran").to_vec()
}

/// **The keyboard camera controller, end to end.** A held `D` pans the orbit
/// target east across the painted chunk; the world-anchored ground scrolls
/// under the camera, so the pan frame differs from the seeded frame. Releasing
/// the key freezes the pose, so the next frame matches the pan frame — the
/// zero-mail-idle invariant proven through the full render chain. Proves the
/// controller co-loads with the camera (the export wiring this change adds),
/// drives it over the loaded-peer path, and that input reaches the rendered
/// view without the controller ever touching the render sink itself.
#[test]
#[allow(clippy::cast_precision_loss)]
fn held_key_pans_the_camera_over_the_painted_world() {
    // The world export moved to the `aether-kit-terrain` wasm; the camera and
    // its controller stay in the `aether-kit-commons` wasm, so this scenario loads both.
    let Some(terrain_path) = require_runtime("aether_kit_terrain") else {
        return;
    };
    let Some(kit_path) = require_runtime("aether_kit_commons") else {
        return;
    };
    let terrain_wasm = fs::read(&terrain_path).expect("read kit-terrain wasm");
    let kit_wasm = fs::read(&kit_path).expect("read kit wasm");
    // Composition: GPU captures + wasm loads; every Key / WindowSize is
    // mailed straight to a component mailbox, so no input fan-out cap.
    let mut harness = SubstrateHarness::builder()
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .with_render()
        .with_component_host()
        .build()
        .expect("boot");

    let world = component_address("world");
    // The controller resolves its target camera by the camera export's default
    // load name (`aether.kit.camera`), so the camera must be loaded under it.
    let camera = component_address("aether.kit.camera");
    let controller = component_address("controller");

    load_kit_export(&mut harness, &terrain_wasm, "aether.kit.world", "world", Vec::new());
    load_kit_export(&mut harness, &kit_wasm, "aether.kit.camera", "aether.kit.camera", Vec::new());
    // Default config drives the camera's boot `"main"` orbit camera — the
    // documented baseline. Loaded last so the camera instance exists when the
    // controller's `wire()` seed mail arrives.
    let config = ControllerConfig::default().encode_into_bytes();
    load_kit_export(&mut harness, &kit_wasm, "aether.kit.camera-controller", "controller", config);

    // Paint the split chunk and feed the camera a real window aspect, then
    // settle the seed + subscriptions before the first capture.
    harness
        .execute(vec![
            ("paint", HarnessOp::send_mail(world.as_str(), &split_chunk())),
            (
                "aspect",
                HarnessOp::send_mail(
                    camera.as_str(),
                    &WindowSize { window: TEST_WINDOW_ID, width: WINDOW_WIDTH, height: WINDOW_HEIGHT },
                ),
            ),
            ("settle", HarnessOp::advance(2)),
        ])
        .expect("paint + settle");

    let seeded = capture_scene(&mut harness, &camera, &world, "seeded");

    // Hold D (no release): each tick the controller pans the orbit target east
    // into the chunk and mails the delta to the camera. 48 ticks at the default
    // 0.15 m/tick pan walks the target ~7 m across the 16 m chunk.
    harness
        .execute(vec![
            ("press_d", HarnessOp::send_mail(controller.as_str(), &Key { window: TEST_WINDOW_ID, code: KEY_D })),
            ("pan", HarnessOp::advance(48)),
        ])
        .expect("hold D + pan");

    let panned = capture_scene(&mut harness, &camera, &world, "panned");

    // Release D and advance: with no key held the controller emits no mail, so
    // the camera pose is frozen and the view stops moving.
    harness
        .execute(vec![
            (
                "release_d",
                HarnessOp::send_mail(controller.as_str(), &KeyRelease { window: TEST_WINDOW_ID, code: KEY_D }),
            ),
            ("idle", HarnessOp::advance(48)),
        ])
        .expect("release D + idle");

    let idle = capture_scene(&mut harness, &camera, &world, "idle");

    let seeded_img = decode_png(&seeded).expect("decode seeded png");
    let panned_img = decode_png(&panned).expect("decode panned png");
    let idle_img = decode_png(&idle).expect("decode idle png");

    // Both moving-phase frames rendered a real scene: the painted ground fills
    // a healthy fraction of the frame, far from an empty (clear-color) capture.
    let seeded_cov = coverage(&seeded_img, background_top_left(&seeded_img), 5);
    let panned_cov = coverage(&panned_img, background_top_left(&panned_img), 5);
    eprintln!("scene coverage: seeded={seeded_cov:.3} panned={panned_cov:.3}");
    assert!(
        seeded_cov > 0.1 && panned_cov > 0.1,
        "both captures should render the painted ground (coverage > 0.1); \
         seeded={seeded_cov:.3} panned={panned_cov:.3} — the controller did not seed / \
         drive the camera over the scene",
    );

    // The camera moved: panning the orbit target scrolled the world-anchored
    // ground, so the pan frame diverges from the seeded frame. The tolerance
    // band absorbs GPU nondeterminism at the low end and rules out a
    // garbage/all-different frame at the high end.
    let pan_mae = mean_absolute_error(&panned_img, &seeded_img).expect("same-size frames");
    eprintln!("frame mean-absolute-error across the pan: {pan_mae:.3}");
    assert!(
        (0.01..0.95).contains(&pan_mae),
        "holding D should scroll the painted world under the camera; the frame \
         mean-absolute-error was {pan_mae:.3} (expected 0.01..0.95) — the key did not drive \
         the camera",
    );

    // Idle produces no drift: with the key released the pose is frozen, so the
    // idle frame matches the pan frame. Only GPU nondeterminism separates two
    // renders of the same pose.
    let idle_mae = mean_absolute_error(&idle_img, &panned_img).expect("same-size frames");
    eprintln!("frame mean-absolute-error after release: {idle_mae:.3}");
    assert!(
        idle_mae < 0.02,
        "releasing the key should freeze the camera (zero mail while idle); the frame \
         mean-absolute-error after release was {idle_mae:.3} (expected < 0.02) — the camera \
         kept moving with no key held",
    );
}
