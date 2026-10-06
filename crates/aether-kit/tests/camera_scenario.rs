//! Camera scenario tests. Each test boots a rendering `SubstrateHarness`,
//! loads `aether-kit`'s wasm artifact (built separately for
//! `wasm32-unknown-unknown`), spawns camera instances from the
//! `aether.kit.camera` export, and drives them and the renderer through mail:
//! `aether.render.view_from` to pick the camera the renderer follows, the
//! `aether.kit.camera.*` kinds to move one, and a captured frame or a reply to
//! see what came of it.
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The component's wasm hasn't been built — tests read
//!   `target/wasm32-unknown-unknown/{debug,release}/aether_kit.wasm`
//!   and skip with an `eprintln!` when both paths are absent. CI
//!   builds the wasm before invoking `cargo test`.
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate) live in `aether_harness_substrate_capture::test_helpers`
//! (issues 460 + 821).

use std::fs;
use std::time::Duration;

use aether_actor::{ActorPath, ActorRef};
use aether_data::{Kind, LoadName};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, require_runtime};
use aether_harness_substrate_capture::visual::{
    Image, background_top_left, bounding_box, coverage, decode_png, mean_absolute_error,
};
use aether_kinds::LoadComponent;
use aether_kit::camera::{
    CameraComponent, CameraConfig, CameraRay, CameraRayResult, Distance, Glide, Lens, Pitch, Pixels, Pose, Viewport,
    Where, Yaw,
};
use aether_math::{Rgb, Vec2, Vec3};
use aether_render::{DrawTriangle, RenderCapability, Vertex, ViewFrom, ViewSource};
use aether_window::{CreateWindow, WindowCapability, WindowMode, WindowSizeRequest, WindowSpec};

// Force linkage of `aether-kit`'s `inventory::submit!` `KindDescriptor`
// entries into this test binary. Cargo treats integration tests as
// separate crates that link against the test target's host rlib, but
// the linker strips inventory submits for kinds the test code doesn't
// statically reference.
#[allow(unused_imports)]
use aether_kit as _;

fn key(name: &str) -> LoadName {
    LoadName::new(name).expect("a valid key")
}

/// A level pose looking down `-Z` at `target` from `distance` back.
fn facing(target: Vec3, distance: f32) -> Pose {
    Pose {
        target,
        yaw: Yaw::new(0.0).expect("a finite yaw"),
        pitch: Pitch::new(0.0).expect("a level pitch"),
        distance: Distance::new(distance).expect("a positive distance"),
    }
}

/// A 60° perspective camera config over a fixed `width` by `height` viewport.
fn fixed(width: u32, height: u32, pose: Pose) -> CameraConfig {
    let viewport = Viewport::Fixed {
        width: Pixels::new(width).expect("a width that is not zero"),
        height: Pixels::new(height).expect("a height that is not zero"),
    };

    CameraConfig { lens: Lens::BOOT, viewport, pose: Some(pose) }
}

/// Spawn a camera instance under `name` from the kit's `aether.kit.camera`
/// export.
fn load_camera(
    harness: &mut SubstrateHarness,
    wasm: &[u8],
    name: &str,
    config: &CameraConfig,
) -> ActorRef<CameraComponent> {
    let camera = harness
        .load::<CameraComponent>(LoadComponent {
            wasm: wasm.to_vec(),
            name: Some(name.to_owned()),
            config: config.encode_into_bytes(),
            export: None,
        })
        .unwrap_or_else(|error| panic!("load camera {name}: {error}"));
    assert_eq!(harness.actor_path(&camera).to_string(), format!("aether.kit.camera:{name}"));

    camera
}

/// Tell the renderer to take its view from the camera under `name`. The
/// subscription it sends the camera and the view the camera sends back ride
/// the request's chain, so the view is applied when this returns.
fn follow(harness: &mut SubstrateHarness, name: &str) {
    let source = ActorPath::<CameraComponent>::instance(&key(name)).narrow::<ViewSource>();

    harness
        .execute(vec![(
            "follow",
            HarnessOp::send_and_settle(&harness.actor_ref::<RenderCapability>(), &ViewFrom { source }),
        )])
        .expect("the renderer follows the camera");
}

/// Capture one frame drawing a world-space triangle centred on the origin,
/// one unit wide and one unit tall, facing `+Z`.
fn capture_triangle(harness: &mut SubstrateHarness, label: &'static str) -> Image {
    let color = Rgb { r: 0.9, g: 0.3, b: 0.2 };
    let corner = |x: f32, y: f32| Vertex { x, y, z: 0.0, color };
    let triangle = DrawTriangle { verts: [corner(-0.5, -0.5), corner(0.5, -0.5), corner(0.0, 0.5)] };
    let captured = harness
        .execute(vec![(label, HarnessOp::capture_with_mails(vec![envelope("aether.render", &triangle)], Vec::new()))])
        .expect("capture-with-mails");

    decode_png(captured.captured(label).expect("capture step ran")).expect("decode capture png")
}

/// The fraction of `image` the triangle covers against the clear color.
fn covered(image: &Image) -> f32 {
    coverage(image, background_top_left(image), 5)
}

/// One `view_from` mail switches the active camera. Two cameras look at the
/// same triangle from 2 and from 5 units back; through a 60° lens on a 4:3
/// frame it covers about 7% and about 1.1%. The renderer follows the near
/// one, then the far one, and a pose sent to the near one afterwards changes
/// nothing on screen. A renderer that kept its first subscription, or a
/// camera that kept sending a viewer that unsubscribed, would redraw the
/// frame from the near camera's new pose.
#[test]
fn one_view_from_mail_switches_the_camera_the_renderer_follows() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    let mut harness =
        SubstrateHarness::builder().size(128, 96).with_render().with_component_host().build().expect("boot");
    let near = load_camera(&mut harness, &wasm, "near", &fixed(128, 96, facing(Vec3::ZERO, 2.0)));
    load_camera(&mut harness, &wasm, "far", &fixed(128, 96, facing(Vec3::ZERO, 5.0)));

    follow(&mut harness, "near");
    let through_near = covered(&capture_triangle(&mut harness, "near"));
    assert!((0.05..0.09).contains(&through_near), "from 2 units the triangle covers ~7%; got {through_near:.4}");

    follow(&mut harness, "far");
    let far_frame = capture_triangle(&mut harness, "far");
    let through_far = covered(&far_frame);
    assert!((0.007..0.016).contains(&through_far), "from 5 units the triangle covers ~1.1%; got {through_far:.4}");

    harness
        .execute(vec![("move_near", HarnessOp::send_and_settle(&near, &facing(Vec3::ZERO, 1.2)))])
        .expect("the near camera takes a pose");
    let after_frame = capture_triangle(&mut harness, "after");
    let drift = mean_absolute_error(&after_frame, &far_frame).expect("same-size frames");
    assert!(
        drift < 0.01,
        "a pose sent to the camera the renderer stopped following must not reach the frame; it moved by {drift:.4} \
         and the triangle covers {:.4}",
        covered(&after_frame),
    );
}

/// A `Fixed` viewport's aspect is the one the camera projects for, whatever
/// the frame it is drawn into. Drawn into a 4:3 frame, a view projected for a
/// 3:4 viewport stretches the unit triangle to `(4/3) / (3/4)`, about 1.78
/// times as wide as tall, and one projected for 4:3 leaves it as wide as
/// tall. A camera that still took its aspect from a window would draw both
/// the same.
#[test]
fn a_fixed_viewport_sets_the_aspect_the_camera_projects_for() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    let mut harness =
        SubstrateHarness::builder().size(256, 192).with_render().with_component_host().build().expect("boot");
    load_camera(&mut harness, &wasm, "matched", &fixed(256, 192, facing(Vec3::ZERO, 3.0)));
    load_camera(&mut harness, &wasm, "upright", &fixed(480, 640, facing(Vec3::ZERO, 3.0)));

    let width_over_height = |image: &Image| {
        let bounds = bounding_box(image, background_top_left(image), 5).expect("the triangle is drawn");
        f64::from(bounds.max_x - bounds.min_x + 1) / f64::from(bounds.max_y - bounds.min_y + 1)
    };

    follow(&mut harness, "matched");
    let matched = width_over_height(&capture_triangle(&mut harness, "matched"));
    assert!((0.85..1.18).contains(&matched), "projected for the frame's own aspect; got {matched:.3}");

    follow(&mut harness, "upright");
    let upright = width_over_height(&capture_triangle(&mut harness, "upright"));
    assert!((1.55..2.0).contains(&upright), "projected for a 3:4 viewport, drawn into 4:3; got {upright:.3}");
}

/// A camera that follows a window publishes nothing until it has learned the
/// window's size, and starts when the window opens. Before: the renderer
/// follows the camera but is sent no view, so the triangle draws under the
/// identity projection (an area of 0.5 in a clip square of 4, 12.5% of the
/// frame), and a pixel has no ray. After the window opens, the camera
/// projects through its boot pose (3 units back, a 60° lens: about 2-3% of
/// the frame) and the centre pixel's ray runs from the eye toward the target.
/// A camera that never heard of a window opened after it wired would stay at
/// the identity footprint; one that guessed an extent would project before
/// the window exists.
#[test]
fn a_window_camera_publishes_once_its_window_has_a_size() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    let mut harness =
        SubstrateHarness::builder().size(128, 96).with_render().with_component_host().build().expect("boot");
    let camera = load_camera(&mut harness, &wasm, "main", &CameraConfig::default());
    follow(&mut harness, "main");
    let centre = CameraRay { pixel: Vec2::new(64.0, 48.0) };

    let waiting = covered(&capture_triangle(&mut harness, "waiting"));
    assert!((0.08..0.17).contains(&waiting), "no view before the window has a size; got {waiting:.4}");
    let blind = harness
        .execute(vec![("ray", HarnessOp::send_and_await_reply(&camera, &centre))])
        .expect("the camera answers a ray")
        .reply::<CameraRayResult>("ray")
        .expect("decode CameraRayResult");
    assert_eq!(blind, CameraRayResult::NoViewport);

    let spec = WindowSpec {
        name: "main".to_owned(),
        title: "main".to_owned(),
        mode: WindowMode::Windowed,
        size: Some(WindowSizeRequest { width: 128, height: 96 }),
    };
    harness
        .execute(vec![(
            "open",
            HarnessOp::send_and_settle(&harness.actor_ref::<WindowCapability>(), &CreateWindow { spec }),
        )])
        .expect("the window opens");

    let projected = covered(&capture_triangle(&mut harness, "projected"));
    assert!(projected > 0.005, "the projected triangle stays visible; got {projected:.4}");
    assert!(projected < waiting * 0.5, "the camera's view reaches the renderer; got {projected:.4} of {waiting:.4}");

    let seen = harness
        .execute(vec![("ray", HarnessOp::send_and_await_reply(&camera, &centre))])
        .expect("the camera answers a ray")
        .reply::<CameraRayResult>("ray")
        .expect("decode CameraRayResult");
    let CameraRayResult::Ok(ray) = seen else {
        panic!("a camera with a viewport answers a ray; got {seen:?}");
    };
    // The boot pose pitches the eye 0.3 radians below the origin, 3 back.
    let eye = Vec3::new(0.0, -3.0 * 0.3_f32.sin(), 3.0 * 0.3_f32.cos());
    let toward_target = (Vec3::ZERO - eye).normalize();
    assert!((ray.direction - toward_target).length() < 1e-3, "the centre ray runs {:?}", ray.direction);
}

/// A glide steps on `Tick` by the tick's elapsed time and ends on its
/// destination. Halfway through a 100 ms glide the target has moved half the
/// way and the distance is at the geometric mean, 4, of 2 and 8; once the
/// time has run, `Where` answers the destination. A camera that did not
/// subscribe `Tick` for the glide would stay where it started, and one that
/// counted ticks instead of their elapsed time would be elsewhere at the
/// halfway mark.
#[test]
fn a_glide_steps_by_elapsed_time_and_ends_on_its_destination() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    let mut harness =
        SubstrateHarness::builder().size(64, 48).with_render().with_component_host().build().expect("boot");
    let camera = load_camera(&mut harness, &wasm, "main", &fixed(64, 48, facing(Vec3::ZERO, 2.0)));
    let destination = facing(Vec3::new(4.0, 0.0, 0.0), 8.0);

    let report = harness
        .execute(vec![
            ("glide", HarnessOp::send_and_settle(&camera, &Glide { to: destination, over_millis: 100 })),
            ("half", HarnessOp::advance_by(1, Duration::from_millis(50))),
            ("halfway", HarnessOp::send_and_await_reply(&camera, &Where)),
            ("rest", HarnessOp::advance_by(2, Duration::from_millis(50))),
            ("arrived", HarnessOp::send_and_await_reply(&camera, &Where)),
        ])
        .expect("glide, advance, ask");

    let halfway = report.reply::<Pose>("halfway").expect("decode the halfway pose");
    assert!((halfway.target.x - 2.0).abs() < 1e-3, "halfway target = {:?}", halfway.target);
    assert!((halfway.distance.get() - 4.0).abs() < 1e-3, "halfway distance = {}", halfway.distance.get());
    assert_eq!(report.reply::<Pose>("arrived").expect("decode the final pose"), destination);
}
