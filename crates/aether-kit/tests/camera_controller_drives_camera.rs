//! Acceptance: a held key drives the keyboard camera controller, which steers
//! the camera instance its config names, which scrolls the rendered view
//! (issue 2820).
//!
//! Loads two `aether-kit` actors — a `CameraComponent` instance at
//! `aether.kit.camera:main` and a `CameraController` whose default config
//! names it — tells the renderer to follow the camera, draws a high-contrast
//! world-anchored striped ground straight to `aether.render` at each capture,
//! and captures three frames: before any key, after a held `D` pans the
//! camera's target across the ground, and after the key is released. The
//! controller owns no pixels; the honest rendered signal that the whole
//! `key → controller → aether.kit.camera.pose → camera → view → renderer`
//! chain composed is that the pan frame differs from the first frame, while
//! the released frame matches the pan frame (the zero-mail-idle invariant, end
//! to end). The camera's own answer to `aether.kit.camera.where` says how far
//! the target moved. The controller's per-tick integration math is pinned by
//! its own unit tests; this is the composition-and-motion proof the harness
//! split routes to `SubstrateHarness`.
//!
//! Skipped when no wgpu adapter is available or the `aether_kit` wasm has not
//! been pre-built (the shared `require_runtime` gate). CI sets
//! `AETHER_REQUIRE_RUNTIME=1` to turn either skip into a hard failure.

// Integration-test skip diagnostic: emit via stderr so `cargo test` surfaces
// "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]

use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use std::fs;

use aether_actor::{ActorRef, Addressable};
use aether_data::{ErasedActorPath, Kind};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::test_helpers::{envelope, require_runtime};
use aether_harness_substrate_capture::visual::{background_top_left, coverage, decode_png, mean_absolute_error};
use aether_kinds::keycode::KEY_D;
use aether_kinds::{Key, KeyRelease, LoadComponent, NamedMail};
use aether_kit::camera::controller::{CameraController, ControllerConfig};
use aether_kit::camera::{CameraComponent, CameraConfig, Distance, Lens, Pitch, Pixels, Pose, Viewport, Where, Yaw};
use aether_math::{Rgb, Vec3};
use aether_render::{DrawTriangle, RenderCapability, Vertex, ViewFrom, ViewSource};

/// Capture surface — a 4:3 frame, which the camera's fixed viewport matches.
const WINDOW_WIDTH: u32 = 128;
const WINDOW_HEIGHT: u32 = 96;
fn test_window() -> ErasedActorPath {
    aether_window::window_path(&aether_data::LoadName::new("main").expect("a valid window name"))
}

/// Spawn the `aether_kit` export `R` as the instance `main` with init-config
/// bytes, blocking on `LoadResult` so the component is instantiated and wired
/// before the next op.
fn load_main<R: Addressable>(harness: &mut SubstrateHarness, wasm: &[u8], config: Vec<u8>) -> ActorRef<R> {
    let namespace = R::NAMESPACE;
    let actor = harness
        .load::<R>(LoadComponent { wasm: wasm.to_vec(), name: Some("main".to_owned()), config, export: None })
        .unwrap_or_else(|error| panic!("load {namespace}: {error}"));
    assert_eq!(harness.actor_path(&actor).to_string(), format!("{namespace}:main"));

    actor
}

/// A three-quarter overhead look at the world origin from 12 units back, over
/// a fixed viewport of the capture's size.
fn overhead_camera() -> CameraConfig {
    let pose = Pose {
        target: Vec3::ZERO,
        yaw: Yaw::new(0.0).expect("a finite yaw"),
        pitch: Pitch::new(-1.1).expect("a pitch above the ground"),
        distance: Distance::new(12.0).expect("a positive distance"),
    };
    let viewport = Viewport::Fixed {
        width: Pixels::new(WINDOW_WIDTH).expect("a width that is not zero"),
        height: Pixels::new(WINDOW_HEIGHT).expect("a height that is not zero"),
    };

    CameraConfig { lens: Lens::BOOT, viewport, pose: Some(pose) }
}

/// A world-anchored ground plane at `y = 0` striped along `x` — green and gray
/// bands 4 m wide from `x = -16` to `x = 24`, spanning `z` in `-16..16` — so
/// sharp, world-fixed color boundaries cross the whole pan path. Those stripe
/// edges (plus the plane's rim against the clear color) are the high-contrast
/// features the camera scrolls over as the target pans east.
fn ground_stripes() -> Vec<NamedMail> {
    const STRIPE_WIDTH: f32 = 4.0;
    const HALF_DEPTH: f32 = 16.0;
    let green = Rgb { r: 0.2, g: 0.8, b: 0.3 };
    let gray = Rgb { r: 0.5, g: 0.5, b: 0.55 };
    (0u8..10)
        .flat_map(|stripe| {
            let west = STRIPE_WIDTH.mul_add(f32::from(stripe), -16.0);
            let east = west + STRIPE_WIDTH;
            let color = if stripe % 2 == 0 {
                green
            } else {
                gray
            };
            let corner = |x: f32, z: f32| Vertex { x, y: 0.0, z, color };
            [
                DrawTriangle {
                    verts: [corner(west, -HALF_DEPTH), corner(east, -HALF_DEPTH), corner(east, HALF_DEPTH)],
                },
                DrawTriangle { verts: [corner(west, -HALF_DEPTH), corner(east, HALF_DEPTH), corner(west, HALF_DEPTH)] },
            ]
        })
        .map(|triangle| envelope("aether.render", &triangle))
        .collect()
}

/// Capture one frame of the striped ground under the view the renderer was
/// last sent: the camera publishes when its pose changes, so the capture
/// stages only the ground.
fn capture_scene(harness: &mut SubstrateHarness, label: &'static str) -> Vec<u8> {
    let captured = harness
        .execute(vec![(label, HarnessOp::capture_with_mails(ground_stripes(), Vec::new()))])
        .expect("capture-with-mails");
    captured.captured(label).expect("capture step ran").to_vec()
}

/// The camera's answer to `aether.kit.camera.where`.
fn pose_of(harness: &mut SubstrateHarness, camera: ActorRef<CameraComponent>) -> Pose {
    harness
        .execute(vec![("where", HarnessOp::send_and_await_reply(&camera, &Where))])
        .expect("the camera answers where it is")
        .reply::<Pose>("where")
        .expect("decode the pose")
}

/// **The keyboard camera controller, end to end.** A held `D` pans the
/// camera's target east across the striped ground; the world-anchored ground
/// scrolls under the camera, so the pan frame differs from the first frame,
/// and the camera reports its target 48 ticks of pan to the east. Releasing
/// the key freezes the pose, so the next frame matches the pan frame — the
/// zero-mail-idle invariant proven through the full render chain. Proves the
/// controller proves the camera its config names, starts from the pose the
/// camera reports, and that input reaches the rendered view without the
/// controller ever touching the render sink itself. A controller that never
/// heard the camera's answer to `where` would send no pose at all.
#[test]
#[allow(clippy::cast_precision_loss)]
fn held_key_pans_the_camera_over_the_painted_world() {
    let Some(kit_path) = require_runtime("aether_kit") else {
        return;
    };
    let kit_wasm = fs::read(&kit_path).expect("read kit wasm");
    // Composition: GPU captures + wasm loads; every Key / WindowSize is
    // mailed straight to a component mailbox, so no input fan-out cap.
    let mut harness = SubstrateHarness::builder()
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .with_render()
        .with_component_host()
        .build()
        .expect("boot");

    // The camera first: the controller proves its path when it wires and asks
    // it where it is, so the keys step from the camera's own pose.
    let camera = load_main::<CameraComponent>(&mut harness, &kit_wasm, overhead_camera().encode_into_bytes());
    let controller =
        load_main::<CameraController>(&mut harness, &kit_wasm, ControllerConfig::default().encode_into_bytes());

    // The renderer takes its view from the camera; the view the camera sends
    // back rides the request's chain.
    let follow = ViewFrom { source: CameraComponent::main_path().narrow::<ViewSource>() };
    harness
        .execute(vec![
            ("follow", HarnessOp::send_and_settle(&harness.actor_ref::<RenderCapability>(), &follow)),
            ("settle", HarnessOp::advance(2)),
        ])
        .expect("follow + settle");

    let seeded = capture_scene(&mut harness, "seeded");

    // Hold D (no release): each tick the controller pans the target east
    // across the stripes and mails the pose to the camera. 48 ticks at the
    // default 0.15 m/tick pan walks the target 7.2 m — nearly two stripe widths.
    harness
        .execute(vec![
            ("press_d", HarnessOp::send_and_settle(&controller, &Key { window: test_window(), code: KEY_D })),
            ("pan", HarnessOp::advance(48)),
        ])
        .expect("hold D + pan");

    let panned = capture_scene(&mut harness, "panned");
    let panned_pose = pose_of(&mut harness, camera);
    assert!(
        (panned_pose.target.x - 7.2).abs() < 0.01,
        "48 ticks of D at 0.15 per tick move the target 7.2 east; the camera reports {:?}",
        panned_pose.target,
    );

    // Release D and advance: with no key held the controller emits no mail, so
    // the camera pose is frozen and the view stops moving.
    harness
        .execute(vec![
            ("release_d", HarnessOp::send_and_settle(&controller, &KeyRelease { window: test_window(), code: KEY_D })),
            ("idle", HarnessOp::advance(48)),
        ])
        .expect("release D + idle");

    let idle = capture_scene(&mut harness, "idle");
    assert_eq!(pose_of(&mut harness, camera), panned_pose, "no key held, so the camera keeps its pose");

    let seeded_img = decode_png(&seeded).expect("decode seeded png");
    let panned_img = decode_png(&panned).expect("decode panned png");
    let idle_img = decode_png(&idle).expect("decode idle png");

    // Both moving-phase frames rendered a real scene: the striped ground fills
    // a healthy fraction of the frame, far from an empty (clear-color) capture.
    let seeded_cov = coverage(&seeded_img, background_top_left(&seeded_img), 5);
    let panned_cov = coverage(&panned_img, background_top_left(&panned_img), 5);
    eprintln!("scene coverage: seeded={seeded_cov:.3} panned={panned_cov:.3}");
    assert!(
        seeded_cov > 0.1 && panned_cov > 0.1,
        "both captures should render the striped ground (coverage > 0.1); \
         seeded={seeded_cov:.3} panned={panned_cov:.3} — the camera's view did not reach \
         the renderer",
    );

    // The camera moved: panning the orbit target scrolled the world-anchored
    // ground, so the pan frame diverges from the seeded frame. The tolerance
    // band absorbs GPU nondeterminism at the low end and rules out a
    // garbage/all-different frame at the high end.
    let pan_mae = mean_absolute_error(&panned_img, &seeded_img).expect("same-size frames");
    eprintln!("frame mean-absolute-error across the pan: {pan_mae:.3}");
    assert!(
        (0.01..0.95).contains(&pan_mae),
        "holding D should scroll the striped ground under the camera; the frame \
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
