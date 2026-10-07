//! Camera scenario tests. Each test boots a rendering `SubstrateHarness`,
//! loads `aether-kit`'s wasm artifact (built separately for
//! `wasm32-unknown-unknown`), spawns camera instances from the
//! `aether.kit.camera` export, and drives them and the renderer through mail:
//! `aether.render.view_from` to pick the camera the renderer follows, the
//! `aether.kit.camera.*` kinds to move one, and a captured frame or a reply to
//! see what came of it.
//!
//! The viewer scenarios compose three native actors beside the component
//! host. [`Viewer`] subscribes to a camera, unsubscribes, and shuts itself
//! down, each when told, and never unsubscribes on its own, so it is a viewer
//! that closes without saying so. [`Bystander`] subscribes and stays.
//! [`Watcher`] monitors the viewer the way a capability does. What a camera
//! sends for one pose is read from the camera's own trace ring
//! ([`views_sent_for`]).
//!
//! No viewer scenario waits on a clock. A departure's notices are posted to
//! the watchers in the order they registered, and every scenario registers
//! the [`Watcher`] after the camera's watch. So once the [`Watcher`] has
//! handled its own notice, the camera's notice is already in the camera's
//! inbox, and a pose sent after that is handled behind it.
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

use aether_actor::{ActorPath, ActorRef, HeldReply, ProtocolPath, actor};
use aether_data::{Kind, LoadName};
use aether_harness_substrate::{HarnessOp, SendTarget, SubstrateHarness};
use aether_harness_substrate_capture::RenderHarnessBuilderExt;
use aether_harness_substrate_capture::test_helpers::{envelope, require_runtime};
use aether_harness_substrate_capture::visual::{
    Image, background_top_left, bounding_box, coverage, decode_png, mean_absolute_error,
};
use aether_kinds::trace::{TraceEvent, TraceTail, TraceTailResult};
use aether_kinds::{LoadComponent, MonitorNotice};
use aether_kit::camera::{
    CameraComponent, CameraConfig, CameraRay, CameraRayResult, Distance, Glide, Lens, Pitch, Pixels, Pose, Viewport,
    Where, Yaw,
};
use aether_math::{Rgb, Vec2, Vec3};
use aether_render::{
    DrawTriangle, RenderCapability, Vertex, ViewFrom, ViewProjection, ViewSource, ViewSubscribe, ViewUnsubscribe,
};
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::{BootError, MonitorHandle};
use aether_window::{CreateWindow, WindowCapability, WindowMode, WindowPresentation, WindowSizeRequest, WindowSpec};

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
    let source = view_source(name);

    harness
        .execute(vec![(
            "follow",
            HarnessOp::send_and_settle(&harness.actor_ref::<RenderCapability>(), &ViewFrom { source }),
        )])
        .expect("the renderer follows the camera");
}

/// Where the camera under `name` takes its viewers.
fn view_source(name: &str) -> ProtocolPath<ViewSource> {
    ActorPath::<CameraComponent>::instance(&key(name)).narrow()
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
        presentation: WindowPresentation::Display,
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

/// Subscribe the receiver to the camera at `camera`. The receiver sends the
/// camera `ViewSubscribe` itself, so the camera sees it as the sender.
#[aether_data::kind(name = "test.kit_camera.subscribe", no_serde)]
struct Subscribe {
    camera: ProtocolPath<ViewSource>,
}

/// Unsubscribe the receiver from the camera at `camera`, the same way.
#[aether_data::kind(name = "test.kit_camera.unsubscribe", no_serde)]
struct Unsubscribe {
    camera: ProtocolPath<ViewSource>,
}

/// Tells the viewer to shut itself down.
#[aether_data::kind(name = "test.kit_camera.shut_down", copy, no_serde)]
struct ShutDown;

/// A native viewer: it takes a camera's view silently, subscribes and
/// unsubscribes when told, and closes when told. Closing unsubscribes from
/// nothing.
struct Viewer;

#[actor(singleton, root)]
impl NativeActor for Viewer {
    const NAMESPACE: &'static str = "test.kit_camera.viewer";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::event]
    fn on_view(&mut self, _ctx: &mut NativeCtx<'_>, _view: ViewProjection) {}

    #[handler::tell]
    fn on_subscribe(&mut self, ctx: &mut NativeCtx<'_>, subscribe: Subscribe) {
        let camera = ctx.resolve(&subscribe.camera).expect("the camera is live");
        ctx.send_to(camera, &ViewSubscribe);
    }

    #[handler::tell]
    fn on_unsubscribe(&mut self, ctx: &mut NativeCtx<'_>, unsubscribe: Unsubscribe) {
        let camera = ctx.resolve(&unsubscribe.camera).expect("the camera is live");
        ctx.send_to(camera, &ViewUnsubscribe);
    }

    #[handler::tell]
    fn on_shut_down(&mut self, ctx: &mut NativeCtx<'_>, _shut_down: ShutDown) {
        ctx.shutdown();
    }
}

/// A second native viewer, which subscribes when told and stays.
struct Bystander;

#[actor(singleton, root)]
impl NativeActor for Bystander {
    const NAMESPACE: &'static str = "test.kit_camera.bystander";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    #[handler::event]
    fn on_view(&mut self, _ctx: &mut NativeCtx<'_>, _view: ViewProjection) {}

    #[handler::tell]
    fn on_subscribe(&mut self, ctx: &mut NativeCtx<'_>, subscribe: Subscribe) {
        let camera = ctx.resolve(&subscribe.camera).expect("the camera is live");
        ctx.send_to(camera, &ViewSubscribe);
    }
}

/// Monitor the [`Viewer`]; the reply confirms the watch stands.
#[aether_data::kind(name = "test.kit_camera.watch", copy, no_serde)]
struct Watch;

#[aether_data::kind(name = "test.kit_camera.watching", copy, no_serde)]
struct Watching;

/// Answered once the viewer's `MonitorNotice` has arrived.
#[aether_data::kind(name = "test.kit_camera.await_departure", copy, no_serde)]
struct AwaitDeparture;

#[aether_data::kind(name = "test.kit_camera.noticed", copy, partial_eq, no_serde)]
struct Noticed {
    notified: bool,
}

impl HeldReply for Noticed {
    fn unanswered() -> Self {
        Self { notified: false }
    }
}

/// Watches the [`Viewer`] the way a capability watches a registrant, and
/// holds an [`AwaitDeparture`] until the viewer's notice arrives.
struct Watcher {
    watch: Option<MonitorHandle>,
    departed: bool,
    waiting: Option<Held<Noticed>>,
}

#[actor(singleton, root)]
impl NativeActor for Watcher {
    const NAMESPACE: &'static str = "test.kit_camera.watcher";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { watch: None, departed: false, waiting: None })
    }

    #[handler::request]
    fn on_watch(&mut self, ctx: &mut NativeCtx<'_>, _watch: Watch) -> Watching {
        let viewer = ctx.resolve_path(ActorPath::<Viewer>::root().as_erased()).expect("the viewer is live");
        self.watch = Some(ctx.monitor(viewer));

        Watching
    }

    #[handler::request]
    fn on_await_departure(&mut self, ctx: &mut NativeCtx<'_>, _await: AwaitDeparture) -> Pending<Noticed> {
        let (pending, held) = ctx.hold::<Noticed>();
        if self.departed {
            held.answer(ctx, &Noticed { notified: true });
        } else {
            self.waiting = Some(held);
        }

        pending
    }

    #[handler::event]
    fn on_monitor_notice(&mut self, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        drop(self.watch.take());
        self.departed = true;
        if let Some(held) = self.waiting.take() {
            held.answer(ctx, &Noticed { notified: true });
        }
    }
}

/// A rendering harness with the component host and the three native test
/// actors.
fn with_viewers() -> SubstrateHarness {
    SubstrateHarness::builder()
        .size(64, 48)
        .with_render()
        .with_component_host()
        .with_actor::<Viewer>(())
        .with_actor::<Bystander>(())
        .with_actor::<Watcher>(())
        .build()
        .expect("boot")
}

fn tell<K: Kind + Clone + 'static, I>(harness: &mut SubstrateHarness, to: impl SendTarget<K, I>, mail: &K) {
    harness.execute(vec![("tell", HarnessOp::send_and_settle(to, mail))]).expect("the tell settles");
}

/// The viewer subscribes to the camera under `name`. The camera's answer,
/// its current view, rides the tell's chain.
fn subscribe_viewer(harness: &mut SubstrateHarness, name: &str) {
    let viewer = harness.actor_ref::<Viewer>();

    tell(harness, &viewer, &Subscribe { camera: view_source(name) });
}

/// Register the native watcher on the viewer, close the viewer, and wait for
/// the watcher's notice. Called after the camera's watch, so the camera's
/// notice is posted before the watcher's.
fn close_viewer(harness: &mut SubstrateHarness) {
    let watcher = harness.actor_ref::<Watcher>();
    let viewer = harness.actor_ref::<Viewer>();
    let report = harness
        .execute(vec![
            ("watch", HarnessOp::send_and_await_reply(&watcher, &Watch)),
            ("shut_down", HarnessOp::send_and_settle(&viewer, &ShutDown)),
            ("noticed", HarnessOp::send_and_await_reply(&watcher, &AwaitDeparture)),
        ])
        .expect("the viewer closes and the watcher is noticed");

    let noticed = report.reply::<Noticed>("noticed").expect("decode Noticed");
    assert_eq!(noticed, Noticed { notified: true }, "the native watcher was noticed");
}

/// Whether `event` is the camera sending a view.
fn sends_a_view(event: &TraceEvent) -> bool {
    matches!(event, TraceEvent::Sent { kind, .. } if *kind == ViewProjection::ID)
}

/// How many `ViewProjection`s `camera` sends for `pose`, read from the
/// camera's own trace ring. The pose is pushed as a tracked root and the
/// tail asks for that root's events; the camera handles the pose before the
/// tail, so the count is settled when the reply arrives.
fn views_sent_for(harness: &mut SubstrateHarness, camera: &ActorRef<CameraComponent>, pose: Pose) -> usize {
    let root = harness.send_tracked(camera, &pose).expect("the pose is pushed");
    let tail = TraceTail { max: 0, since: None, root: Some(root) };
    let reply = harness
        .execute(vec![("tail", HarnessOp::send_and_await_reply(camera, &tail))])
        .expect("the camera's trace ring answers")
        .reply::<TraceTailResult>("tail")
        .expect("decode TraceTailResult");
    let TraceTailResult::Ok { entries, .. } = reply else {
        panic!("the camera's trace ring answers its tail; got {reply:?}");
    };

    entries.iter().filter(|entry| sends_a_view(&entry.event)).count()
}

/// A viewer that closes without unsubscribing is removed, and the camera
/// sends it nothing afterwards. While the viewer is live a pose sends one
/// view, which also shows the trace read sees a guest's sends; after the
/// viewer has closed, a pose sends none. A camera that never watched its
/// viewer, one that watched through a type its departure handler does not
/// serve, or a handler that removed another key would still send one.
#[test]
fn a_viewer_that_closed_without_unsubscribing_is_sent_no_further_view() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    let mut harness = with_viewers();
    let camera = load_camera(&mut harness, &wasm, "main", &fixed(64, 48, facing(Vec3::ZERO, 2.0)));
    subscribe_viewer(&mut harness, "main");
    assert_eq!(views_sent_for(&mut harness, &camera, facing(Vec3::ZERO, 3.0)), 1, "a live viewer is sent the view");

    close_viewer(&mut harness);

    assert_eq!(views_sent_for(&mut harness, &camera, facing(Vec3::ZERO, 4.0)), 0, "a closed viewer is sent nothing");
}

/// A viewer that unsubscribed and later closes changes nothing for the
/// viewers still held. The viewer subscribes and unsubscribes, a bystander
/// subscribes and stays, and the viewer closes: a pose still sends exactly
/// one view, the bystander's. An unsubscribe that left its row behind would
/// send two, and a departure handler that cleared more than the departed
/// viewer's row would send none.
#[test]
fn an_unsubscribed_viewer_that_later_closes_changes_nothing() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    let mut harness = with_viewers();
    let camera = load_camera(&mut harness, &wasm, "main", &fixed(64, 48, facing(Vec3::ZERO, 2.0)));
    let viewer = harness.actor_ref::<Viewer>();
    let bystander = harness.actor_ref::<Bystander>();
    subscribe_viewer(&mut harness, "main");
    tell(&mut harness, &viewer, &Unsubscribe { camera: view_source("main") });
    tell(&mut harness, &bystander, &Subscribe { camera: view_source("main") });

    close_viewer(&mut harness);

    assert_eq!(
        views_sent_for(&mut harness, &camera, facing(Vec3::ZERO, 3.0)),
        1,
        "only the bystander is sent the view"
    );
}

/// A viewer that subscribes twice is held once and removed by its one
/// departure. After two subscribes a pose sends one view; after the viewer
/// has closed, a pose sends none. A second subscribe that made a second row,
/// or one whose row its departure does not remove, would send a view.
#[test]
fn a_viewer_that_subscribes_twice_is_removed_by_one_departure() {
    let Some(wasm_path) = require_runtime("aether_kit") else {
        return;
    };
    let wasm = fs::read(wasm_path).expect("read kit wasm");
    let mut harness = with_viewers();
    let camera = load_camera(&mut harness, &wasm, "main", &fixed(64, 48, facing(Vec3::ZERO, 2.0)));
    subscribe_viewer(&mut harness, "main");
    subscribe_viewer(&mut harness, "main");
    assert_eq!(views_sent_for(&mut harness, &camera, facing(Vec3::ZERO, 3.0)), 1, "a viewer is held once");

    close_viewer(&mut harness);

    assert_eq!(views_sent_for(&mut harness, &camera, facing(Vec3::ZERO, 4.0)), 0, "a closed viewer is sent nothing");
}
