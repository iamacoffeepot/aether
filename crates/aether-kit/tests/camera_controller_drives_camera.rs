//! Acceptance: window input drives the camera controller, which steers the
//! camera instance its config names, which scrolls the rendered view (issues
//! 2820 and 7479).
//!
//! Each scenario loads two `aether-kit` actors: a `CameraComponent` instance
//! at `aether.kit.camera:main` and a `CameraController` whose default config
//! names it and the `main` window. Input is the window's own event kinds,
//! mailed to the controller as the window manager would publish them, and
//! the camera's answer to `aether.kit.camera.where` says where that left it.
//!
//! The first scenario also renders: it tells the renderer to follow the
//! camera, draws a high-contrast world-anchored striped ground straight to
//! `aether.render` at each capture, and captures three frames: before any
//! key, after a held `D` pans the camera's target across the ground, and
//! after the key is released. The controller owns no pixels; the honest
//! rendered signal that the whole
//! `key → controller → aether.kit.camera.pose → camera → view → renderer`
//! chain composed is that the pan frame differs from the first frame, while
//! the released frame matches the pan frame (the zero-mail-idle invariant, end
//! to end). The controller's input-to-pose maths is pinned by its own unit
//! tests; these are the composition proofs the harness split routes to
//! `SubstrateHarness`.
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
use aether_kinds::{Key, KeyRelease, LoadComponent, MouseButton, MouseMove, MouseWheel, NamedMail, mouse_button};
use aether_kit::camera::controller::{CameraController, ControllerConfig};
use aether_kit::camera::{CameraComponent, CameraConfig, Distance, Lens, Pitch, Pixels, Pose, Viewport, Where, Yaw};
use aether_math::{Rgb, Vec3};
use aether_render::{DrawTriangle, RenderCapability, Vertex, ViewFrom, ViewSource};
use aether_window::WindowFocus;

/// Capture surface — a 4:3 frame, which the camera's fixed viewport matches.
const WINDOW_WIDTH: u32 = 128;
const WINDOW_HEIGHT: u32 = 96;

/// The window the default controller config reads.
fn test_window() -> ErasedActorPath {
    window_named("main")
}

fn window_named(name: &str) -> ErasedActorPath {
    aether_window::window_path(&aether_data::LoadName::new(name).expect("a valid window name"))
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

/// A booted harness with the overhead camera and a default controller for it
/// loaded, in that order: the controller proves the camera's path and
/// subscribes to its view when it wires.
struct Scene {
    harness: SubstrateHarness,
    camera: ActorRef<CameraComponent>,
    controller: ActorRef<CameraController>,
}

impl Scene {
    /// `None` when the runtime gate skips the scenario.
    fn boot() -> Option<Self> {
        let kit_wasm = fs::read(require_runtime("aether_kit")?).expect("read kit wasm");
        // Composition: GPU captures + wasm loads; every input event is mailed
        // straight to the controller's mailbox, so no input fan-out cap.
        let mut harness = SubstrateHarness::builder()
            .size(WINDOW_WIDTH, WINDOW_HEIGHT)
            .with_render()
            .with_component_host()
            .build()
            .expect("boot");

        let camera = load_main::<CameraComponent>(&mut harness, &kit_wasm, overhead_camera().encode_into_bytes());
        let controller =
            load_main::<CameraController>(&mut harness, &kit_wasm, ControllerConfig::default().encode_into_bytes());

        Some(Self { harness, camera, controller })
    }

    /// Run `steps` in order, each to settlement. An input event sent to the
    /// controller settles with the controller's question to the camera and
    /// the camera's answer, which ride its chain.
    fn run(&mut self, steps: Vec<(&'static str, HarnessOp)>) {
        self.harness.execute(steps).expect("the steps run");
    }

    fn pose(&mut self) -> Pose {
        pose_of(&mut self.harness, self.camera)
    }
}

/// **A held key, end to end.** A held `D` pans the camera's target east
/// across the striped ground; the world-anchored ground scrolls under the
/// camera, so the pan frame differs from the first frame, and the camera
/// reports its target 0.8 seconds of pan to the east at its distance of 12.
/// Releasing the key freezes the pose, so the next frame matches the pan
/// frame — the zero-mail-idle invariant proven through the full render chain.
/// Proves the controller proves the camera its config names, starts the
/// gesture from the pose the camera reports, and that input reaches the
/// rendered view without the controller ever touching the render sink itself.
/// A controller that never heard the camera's answer to `where` would send no
/// pose at all.
#[test]
#[allow(clippy::cast_precision_loss)]
fn held_key_pans_the_camera_over_the_painted_world() {
    let Some(mut scene) = Scene::boot() else {
        return;
    };

    // The renderer takes its view from the camera; the view the camera sends
    // back rides the request's chain.
    let follow = ViewFrom { source: CameraComponent::main_path().narrow::<ViewSource>() };
    let render = scene.harness.actor_ref::<RenderCapability>();
    scene.run(vec![("follow", HarnessOp::send_and_settle(&render, &follow)), ("settle", HarnessOp::advance(2))]);

    let seeded = capture_scene(&mut scene.harness, "seeded");

    // Hold D (no release): each tick the controller pans the target east
    // across the stripes and mails the pose to the camera. 48 ticks of
    // 16,667 microseconds at the default one camera distance per second walk
    // the target 0.8 of the camera's 12 units: 9.6, over two stripe widths.
    let press = Key { window: test_window(), code: KEY_D };
    scene
        .run(vec![("press_d", HarnessOp::send_and_settle(&scene.controller, &press)), ("pan", HarnessOp::advance(48))]);

    let panned = capture_scene(&mut scene.harness, "panned");
    let panned_pose = scene.pose();
    assert!(
        (panned_pose.target.x - 9.6).abs() < 0.01,
        "0.8 seconds of D from 12 back move the target 9.6 east; the camera reports {:?}",
        panned_pose.target,
    );

    // Release D and advance: with no key held the controller emits no mail, so
    // the camera pose is frozen and the view stops moving.
    let release = KeyRelease { window: test_window(), code: KEY_D };
    scene.run(vec![
        ("release_d", HarnessOp::send_and_settle(&scene.controller, &release)),
        ("idle", HarnessOp::advance(48)),
    ]);

    let idle = capture_scene(&mut scene.harness, "idle");
    assert_eq!(scene.pose(), panned_pose, "no key held, so the camera keeps its pose");

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

/// A left-drag turns the camera by the cursor's travel, and a held button
/// with a still mouse turns it no further. The controller is a viewer of the
/// camera and asks it where it is on the press; one that never took the
/// answer, or that did not subscribe the mouse kinds, leaves the yaw where
/// it was.
#[test]
fn a_left_drag_turns_the_camera() {
    let Some(mut scene) = Scene::boot() else {
        return;
    };
    let before = scene.pose();

    // 40 pixels to the right at the default 0.005 radians per pixel.
    let press = MouseButton { window: test_window(), button: mouse_button::LEFT, x: 44.0, y: 48.0 };
    let drag = MouseMove { window: test_window(), x: 84.0, y: 48.0 };
    scene.run(vec![
        ("press", HarnessOp::send_and_settle(&scene.controller, &press)),
        ("drag", HarnessOp::send_and_settle(&scene.controller, &drag)),
        ("turn", HarnessOp::advance(1)),
    ]);

    let turned = scene.pose();
    let yaw = turned.yaw.get();
    assert!((yaw + 0.2).abs() < 1e-4, "40 pixels to the right turn the yaw to -0.2; the camera reports {yaw}");
    assert_eq!((turned.target, turned.pitch, turned.distance), (before.target, before.pitch, before.distance));

    scene.run(vec![("hold", HarnessOp::advance(8))]);
    assert_eq!(scene.pose(), turned, "the button is held and the mouse is still");
}

/// A pose sent to the camera between two gestures is the pose the second
/// gesture starts from. A controller that kept its own copy of the pose
/// would send the camera that copy on the second gesture's first tick and
/// snap an agent's pose back to where the keys left it.
#[test]
fn a_pose_set_between_gestures_is_where_the_next_gesture_starts() {
    let Some(mut scene) = Scene::boot() else {
        return;
    };

    let press = Key { window: test_window(), code: KEY_D };
    let release = KeyRelease { window: test_window(), code: KEY_D };
    scene.run(vec![
        ("press_d", HarnessOp::send_and_settle(&scene.controller, &press)),
        ("pan", HarnessOp::advance(4)),
        ("release_d", HarnessOp::send_and_settle(&scene.controller, &release)),
    ]);
    assert!(scene.pose().target.x > 0.5, "the first gesture moved the camera");

    // No tick separates the release from the pose from the next gesture: the
    // release itself ends the first one.
    let set = Pose {
        target: Vec3::new(5.0, 0.0, -3.0),
        yaw: Yaw::new(1.0).expect("a finite yaw"),
        pitch: Pitch::new(-0.5).expect("a pitch above the ground"),
        distance: Distance::new(20.0).expect("a positive distance"),
    };
    let one_step_in = MouseWheel { window: test_window(), delta_x: 0.0, delta_y: 40.0, x: 64.0, y: 48.0 };
    scene.run(vec![
        ("set", HarnessOp::send_and_settle(&scene.camera, &set)),
        ("wheel", HarnessOp::send_and_settle(&scene.controller, &one_step_in)),
        ("zoom", HarnessOp::advance(1)),
    ]);

    let zoomed = scene.pose();
    assert_eq!((zoomed.target, zoomed.yaw, zoomed.pitch), (set.target, set.yaw, set.pitch));
    let distance = zoomed.distance.get();
    assert!((distance - 18.0).abs() < 1e-3, "one wheel step in from 20 is 18; the camera reports {distance}");
}

/// The controller reads its own window's input, and drops what is held when
/// that window loses focus. A controller that took every window's keys would
/// move this camera from another window's typing; one that kept a held key
/// across a loss of focus would pan forever, because the release goes to the
/// window that took the focus.
#[test]
fn input_is_read_from_the_controller_s_window_until_it_loses_focus() {
    let Some(mut scene) = Scene::boot() else {
        return;
    };
    let before = scene.pose();

    let elsewhere = Key { window: window_named("other"), code: KEY_D };
    scene.run(vec![
        ("press_elsewhere", HarnessOp::send_and_settle(&scene.controller, &elsewhere)),
        ("idle", HarnessOp::advance(4)),
    ]);
    assert_eq!(scene.pose(), before, "a key pressed in another window moves nothing");

    // Losing another window's focus releases nothing here.
    let press = Key { window: test_window(), code: KEY_D };
    let other_blurred = WindowFocus { window: window_named("other"), focused: false };
    scene.run(vec![
        ("press_d", HarnessOp::send_and_settle(&scene.controller, &press)),
        ("pan", HarnessOp::advance(4)),
        ("blur_other", HarnessOp::send_and_settle(&scene.controller, &other_blurred)),
    ]);
    let panning = scene.pose();
    assert!(panning.target.x > 0.5, "the held key pans the camera");

    scene.run(vec![("pan_on", HarnessOp::advance(4))]);
    let panned = scene.pose();
    assert!(panned.target.x > panning.target.x + 0.5, "the key is still held after another window lost focus");

    // No release is sent: the loss of focus is all the controller hears.
    let blurred = WindowFocus { window: test_window(), focused: false };
    scene
        .run(vec![("blur", HarnessOp::send_and_settle(&scene.controller, &blurred)), ("after", HarnessOp::advance(8))]);
    assert_eq!(scene.pose(), panned, "the held key was dropped with the window's focus");
}

/// A controller whose config names a camera that is not live fails its load,
/// and the refusal names the camera's path. A `wire` that logged the failure
/// and carried on would stand up a controller that takes keys and moves
/// nothing.
#[test]
fn a_controller_whose_camera_is_not_live_is_refused_at_load() {
    let Some(kit_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(&kit_path).expect("read kit wasm");
    let mut harness = SubstrateHarness::builder()
        .size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .with_render()
        .with_component_host()
        .build()
        .expect("boot");

    let refused = harness
        .load::<CameraController>(LoadComponent {
            wasm,
            name: Some("main".to_owned()),
            config: Vec::new(),
            export: None,
        })
        .expect_err("a controller with no camera to drive does not load");

    let reason = refused.to_string();
    assert!(reason.contains("aether.kit.camera:main"), "the refusal names the camera's path: {reason}");
}
