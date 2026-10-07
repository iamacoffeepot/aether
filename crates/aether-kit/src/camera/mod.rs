// Camera math: pixel extents and glide times cast to f32 for a ratio are
// domain-correct.
#![allow(clippy::cast_precision_loss)]
// `#[handler]` methods take the decoded mail by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded payload and hands it off, so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

//! The camera: one instance per camera, at `aether.kit.camera:<key>`.
//!
//! A camera is a [`Pose`] and a [`Lens`] over a [`Viewport`], all set by its
//! [`CameraConfig`] at spawn. It is a view source
//! ([`aether_render::ViewSource`]): whoever sends it
//! `aether.render.view_subscribe` is sent its
//! [`ViewProjection`] at once and again each
//! time the view changes, and at no other time, so an idle camera sends no
//! mail. The renderer becomes such a viewer when it is sent
//! `aether.render.view_from` naming this camera; the active camera is the one
//! the renderer follows, and nothing in the camera says so. A component that
//! draws through its own render program subscribes the same way.
//!
//! # Mail surface
//!
//! - [`Pose`] sets the pose.
//! - [`Frame`] `{ bounds }` looks at a box from far enough back to see it all.
//! - [`Glide`] `{ to, over_millis }` eases to a pose over time.
//! - [`Where`] is answered with the current [`Pose`].
//! - [`CameraRay`] `{ pixel }` is answered with the world-space ray through a
//!   pixel of the viewport ([`CameraRayResult`]).
//! - `aether.render.view_subscribe` / `view_unsubscribe` add and remove the
//!   sender as a viewer. A viewer is also removed when it closes, by any
//!   exit: the camera watches each viewer it adds (ADR-0079 §8).
//!
//! # The viewport
//!
//! A `Fixed` viewport is known from the config. A `Window` viewport is
//! learned: at `wire` the camera subscribes to the window manager's
//! `WindowSize` and `WindowOpened` and asks `aether.window.list`, and takes
//! its window's size from whichever names its window first. Until then it has
//! no extent: it holds its viewers, publishes nothing, refuses a [`Frame`],
//! and answers [`CameraRay`] with `NoViewport`.
//!
//! # Across a republish
//!
//! `on_dehydrate` saves the pose, any glide, and the viewport with the
//! extent learned for it; `on_rehydrate` restores them. The lens and the
//! viewport come from the instance's config, which the host hands the
//! replacement's `init` again. The viewers cannot be saved (a proven
//! reference has no codec), so a republish of this module drops them and each
//! must subscribe again; the camera says how many at warn. The renderer does
//! so when it is sent `aether.render.view_from` again.
//!
//! The watches on those viewers stand: the host moves each with the mailbox.
//! A viewer that subscribes again is watched through the standing watch, so
//! its new row holds the right id. One that never does leaves a watch with no
//! row, which sends the viewer nothing and which the host ends when the
//! viewer closes; the replacement's departure handler then removes nothing.

pub mod controller;

mod kinds;
mod pose;
mod viewers;

pub use kinds::*;

use aether_actor::{ActorInitError, ActorPath, Departed, NoContext, PriorState, ReplyMode, Sends, Subscriber};
use aether_actor::{WasmActor, WasmCtx, WasmDropCtx, WasmInitCtx, actor};
use aether_data::{ErasedActorPath, Kind, LoadName};
use aether_kinds::{Tick, WindowSize};
use aether_lifecycle::{LifecycleCapability, LifecycleSubscribeResult};
use aether_render::{ViewProjection, ViewSubscribe, ViewUnsubscribe, ViewportExtent};
use aether_window::{ListWindows, ListWindowsResult, WindowCapability, WindowOpened};

use pose::Gliding;
use viewers::Viewers;

/// What a camera knows of its viewport's size.
#[derive(aether_data::Schema, Debug, Clone, Copy, PartialEq, Eq)]
enum Extent {
    /// A window viewport whose size has not arrived yet.
    Awaited,
    Known(ViewportExtent),
}

/// What a camera carries across a republish.
#[aether_data::kind(name = "aether.kit.camera.state", no_serde)]
struct CameraState {
    pose: Pose,
    glide: Option<Gliding>,
    /// The viewport `extent` was learned for. The replacement keeps `extent`
    /// only when its own config names the same viewport.
    viewport: Viewport,
    extent: Extent,
    /// How many viewers the instance held, so the replacement can say how
    /// many it lost.
    viewers: u32,
}

pub struct CameraComponent {
    pose: Pose,
    lens: Lens,
    viewport: Viewport,
    extent: Extent,
    glide: Option<Gliding>,
    viewers: Viewers,
}

/// One camera. Publishes its view to its viewers when the view changes.
///
/// # Agent
/// Spawn one with `load_component` / `spawn`, `namespace: "aether.kit.camera"`,
/// a `key` (the demo uses `main`) and a [`CameraConfig`]; it answers at
/// `aether.kit.camera:<key>`. Make it the renderer's camera by sending
/// `aether.render.view_from { source: "aether.kit.camera:<key>" }` to
/// `aether.render`; send that again with another camera's path to switch.
/// Then drive it with `aether.kit.camera.pose`, `.frame` or `.glide`, and
/// `capture_frame` to see the result. Drop the instance to remove the camera.
#[actor(instanced, root, depends(WindowCapability, LifecycleCapability))]
impl WasmActor for CameraComponent {
    type Config = CameraConfig;
    const NAMESPACE: &'static str = "aether.kit.camera";

    fn init(config: CameraConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        let pose = config.pose.unwrap_or(Pose::BOOT);
        if !pose::is_finite(pose.target) {
            return Err(ActorInitError::new(format!("{} has a pose whose target is not finite", CameraConfig::NAME)));
        }
        let extent = match &config.viewport {
            Viewport::Fixed { width, height } => {
                Extent::Known(ViewportExtent { width: width.get(), height: height.get() })
            }
            Viewport::Window(_) => Extent::Awaited,
        };

        Ok(Self {
            pose,
            lens: config.lens,
            viewport: config.viewport,
            extent,
            glide: None,
            viewers: Viewers::default(),
        })
    }

    /// Start following the window a `Window` viewport names. `init`'s ctx has
    /// no send surface, so the subscriptions and the list request go here.
    ///
    /// Nothing here can fail: the window is named by a path the camera
    /// compares and never proves, so a window that is not open yet is waited
    /// for.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        let follows_window = matches!(self.viewport, Viewport::Window(_));
        if follows_window {
            Self::follow_window(ctx);
        }
        Ok(())
    }

    /// Save what a republish would otherwise reset (see the module docs).
    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) {
        let state = CameraState {
            pose: self.pose,
            glide: self.glide,
            viewport: self.viewport.clone(),
            extent: self.extent,
            viewers: u32::try_from(self.viewers.len()).unwrap_or(u32::MAX),
        };

        ctx.save_state_kind(0, &state);
    }

    /// Take back what `on_dehydrate` saved. A replacement holds no viewers
    /// and says how many it lost; an instance reinstated after an aborted
    /// republish still holds its own.
    fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_>, prior: PriorState<'_>) {
        let Some(saved) = prior.decode_kind::<CameraState>() else {
            tracing::warn!(target: "aether_kit", "the saved camera state does not decode; starting from the config");
            return;
        };
        self.pose = saved.pose;
        self.glide = saved.glide;

        let same_viewport = saved.viewport == self.viewport;
        if same_viewport {
            self.extent = saved.extent;
        }
        // A republish does not run `wire`, so a replacement whose config
        // names another window asks for its size here.
        if self.extent == Extent::Awaited {
            Self::follow_window(ctx);
        }

        let held = u32::try_from(self.viewers.len()).unwrap_or(u32::MAX);
        let lost = saved.viewers.saturating_sub(held);
        if lost > 0 {
            tracing::warn!(
                target: "aether_kit",
                lost,
                "a republish dropped this camera's viewers; each must send aether.render.view_subscribe again",
            );
        }
    }

    /// Set the pose, ending any glide in progress.
    ///
    /// # Agent
    /// `{"target": {"x": 0, "y": 0, "z": 0}, "yaw": 0.6, "pitch": -0.4,
    /// "distance": 5}`. A negative pitch looks down on the target;
    /// `-1.5707964` looks straight down. A value out of range does not
    /// decode, and a target that is not finite is refused with a warn.
    #[handler::tell]
    fn on_pose(&mut self, ctx: &mut WasmCtx<'_>, pose: Pose) {
        if pose::is_finite(pose.target) {
            self.rest_at(ctx, pose);
        } else {
            tracing::warn!(target: "aether_kit", "pose refused: its target is not finite");
        }
    }

    /// Look at the centre of a box from far enough back to see all of it,
    /// keeping yaw, pitch and lens, and ending any glide in progress.
    ///
    /// # Agent
    /// `{"bounds": {"min": {"x": -1, "y": 0, "z": -1}, "max": {"x": 1, "y": 2,
    /// "z": 1}}}`. Refused with a warn for an empty, zero-size or non-finite
    /// box, and while a window viewport's size has not arrived.
    #[handler::tell]
    fn on_frame(&mut self, ctx: &mut WasmCtx<'_>, frame: Frame) {
        let Extent::Known(extent) = self.extent else {
            tracing::warn!(target: "aether_kit", "frame refused: the viewport's size is not known yet");
            return;
        };

        if let Some(framed) = pose::framed(self.pose, self.lens, extent, frame.bounds) {
            self.rest_at(ctx, framed);
        } else {
            tracing::warn!(target: "aether_kit", bounds = ?frame.bounds, "frame refused: nothing to frame");
        }
    }

    /// Ease from the current pose to `to` over `over_millis`. The camera
    /// subscribes `Tick` for the glide's duration and steps on each one.
    ///
    /// # Agent
    /// `{"to": <pose>, "over_millis": 800}`. A later `pose`, `frame` or
    /// `glide` ends this one where it stands.
    #[handler::tell]
    fn on_glide(&mut self, ctx: &mut WasmCtx<'_>, glide: Glide) {
        if !pose::is_finite(glide.to.target) {
            tracing::warn!(target: "aether_kit", "glide refused: its target is not finite");
            return;
        }
        if glide.over_millis == 0 {
            self.rest_at(ctx, glide.to);
            return;
        }

        let was_gliding = self.glide.replace(Gliding::start(self.pose, glide.to, glide.over_millis)).is_some();
        if !was_gliding {
            ctx.subscribe::<LifecycleCapability, Tick>();
        }
    }

    /// Step the glide in progress by the tick's elapsed time.
    ///
    /// # Agent
    /// Lifecycle-driven while a glide runs; not useful to send manually.
    #[handler::event]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, tick: Tick) {
        // No glide to step: a tick already on its way when one ended, or a
        // subscription that outlived a glide a republish did not carry over.
        let Some(glide) = &mut self.glide else {
            ctx.unsubscribe::<LifecycleCapability, Tick>();
            return;
        };
        glide.advance(tick.delta_micros);
        let glide = *glide;

        if glide.finished() {
            self.rest_at(ctx, glide.destination());
        } else {
            self.pose = glide.pose();
            self.publish(&mut ctx.sends());
        }
    }

    /// The lifecycle's answer to a glide's `Tick` subscription.
    #[handler::response]
    fn on_tick_subscription(&mut self, _ctx: &mut WasmCtx<'_>, result: LifecycleSubscribeResult) {
        let _ = self;
        if let LifecycleSubscribeResult::Err(error) = result {
            tracing::error!(target: "aether_kit", ?error, "the lifecycle refused the camera's tick subscription");
        }
    }

    /// The current pose; during a glide, the pose reached so far.
    #[handler::request]
    fn on_where(&mut self, _ctx: &mut WasmCtx<'_>, _where: Where) -> Pose {
        self.pose
    }

    /// The world-space ray through a pixel of the viewport.
    ///
    /// # Agent
    /// `{"pixel": {"x": 640, "y": 360}}`, physical pixels from the top-left
    /// corner. Intersect the ray with your scene; `aether-math`'s
    /// `Ray::plane_hit` does a ground plane.
    #[handler::request]
    fn on_ray(&mut self, _ctx: &mut WasmCtx<'_>, ray: CameraRay) -> CameraRayResult {
        let Extent::Known(extent) = self.extent else {
            return CameraRayResult::NoViewport;
        };
        let view = pose::view_projection(self.pose, self.lens, extent);

        pose::pixel_ray(&view, ray.pixel).map_or(CameraRayResult::NoRay, CameraRayResult::Ok)
    }

    /// Add the sender as a viewer and send it the current view. The camera
    /// watches the viewer, so one that closes without unsubscribing is
    /// removed by [`Self::on_viewer_gone`].
    ///
    /// # Agent
    /// Sent by an actor that wants this camera's view; the renderer sends it
    /// when `aether.render.view_from` names this camera. A sender that does
    /// not take `aether.view_projection` silently is refused with a warn.
    #[handler::tell]
    fn on_view_subscribe(&mut self, ctx: &mut WasmCtx<'_>, _subscribe: ViewSubscribe) {
        let Some(sender) = ctx.sender() else {
            tracing::warn!(target: "aether_kit", "view subscribe arrived with no sender; ignoring");
            return;
        };
        let Some(viewer) = ctx.cast::<Subscriber<ViewProjection>>(sender) else {
            tracing::warn!(target: "aether_kit", "view subscribe sender does not take a view projection; ignoring");
            return;
        };

        let watch = ctx.watch(viewer, NoContext);
        self.viewers.add(viewer, watch);
        if let Extent::Known(extent) = self.extent {
            ctx.send_to(viewer, &pose::view_projection(self.pose, self.lens, extent));
        }
    }

    /// Remove the sender as a viewer and end the watch on it. A sender that
    /// never subscribed changes nothing.
    #[handler::tell]
    fn on_view_unsubscribe(&mut self, ctx: &mut WasmCtx<'_>, _unsubscribe: ViewUnsubscribe) {
        let Some(sender) = ctx.sender() else {
            return;
        };

        if let Some(watch) = self.viewers.remove(sender) {
            ctx.unwatch(watch);
        }
    }

    /// Remove a viewer that closed. The watch ended at this notice, so there
    /// is none to end.
    ///
    /// # Agent
    /// The engine's notice that a watched viewer closed; never sent by hand.
    #[handler::event]
    fn on_viewer_gone(&mut self, _ctx: &mut WasmCtx<'_>, event: Departed<Subscriber<ViewProjection>>) {
        self.viewers.remove(event.actor.erase());
    }

    /// Follow the window's size.
    ///
    /// # Agent
    /// Published by the window manager; not useful to send manually.
    #[handler::event]
    fn on_window_size(&mut self, ctx: &mut WasmCtx<'_>, size: WindowSize) {
        self.resize(&mut ctx.sends(), &size.window, size.width, size.height);
    }

    /// Take the size of a window that opened after this camera wired.
    #[handler::event]
    fn on_window_opened(&mut self, ctx: &mut WasmCtx<'_>, opened: WindowOpened) {
        let window = opened.window;

        self.resize(&mut ctx.sends(), &window.path, window.width, window.height);
    }

    /// The window manager's answer to the list `wire` asked for: take the
    /// size of this camera's window, if it is open yet.
    #[handler::response]
    fn on_windows(&mut self, ctx: &mut WasmCtx<'_>, result: ListWindowsResult) {
        match result {
            ListWindowsResult::Ok { windows } => {
                for window in windows {
                    self.resize(&mut ctx.sends(), &window.path, window.width, window.height);
                }
            }
            ListWindowsResult::Err { error } => {
                tracing::error!(target: "aether_kit", %error, "the window manager did not list its windows");
            }
        }
    }
}

impl CameraComponent {
    /// The key the kit's defaults give a scene's one camera.
    pub const MAIN_KEY: &'static str = "main";

    /// `aether.kit.camera:main`: the camera a controller or a mesh viewer
    /// with no config names.
    ///
    /// # Panics
    ///
    /// Never: [`Self::MAIN_KEY`] is a valid segment.
    #[must_use]
    pub fn main_path() -> ActorPath<Self> {
        ActorPath::instance(&LoadName::new(Self::MAIN_KEY).expect("the main camera's key is a valid segment"))
    }

    /// Subscribe to window sizes and openings, then ask for the windows open
    /// now: the manager sends a new subscriber nothing until the next change.
    fn follow_window(ctx: &mut WasmCtx<'_, Self>) {
        ctx.subscribe::<WindowCapability, WindowSize>();
        ctx.subscribe::<WindowCapability, WindowOpened>();
        ctx.send::<WindowCapability>(&ListWindows);
    }

    /// End any glide in progress, take `pose`, and publish.
    fn rest_at<S, M: ReplyMode>(&mut self, ctx: &mut WasmCtx<'_, Self, S, M>, pose: Pose) {
        if self.glide.take().is_some() {
            ctx.unsubscribe::<LifecycleCapability, Tick>();
        }
        self.pose = pose;

        self.publish(&mut ctx.sends());
    }

    /// Take `width` by `height` as the viewport's size when `window` is the
    /// one this camera follows, and publish if that changed the view. A zero
    /// size, a minimised window, keeps the last one.
    fn resize(&mut self, sends: &mut Sends<'_, Self>, window: &ErasedActorPath, width: u32, height: u32) {
        let Viewport::Window(followed) = &self.viewport else {
            return;
        };
        let Some(extent) = pose::extent_of(width, height) else {
            return;
        };

        let ours = followed.as_erased() == window;
        let changed = self.extent != Extent::Known(extent);
        if ours && changed {
            self.extent = Extent::Known(extent);
            self.publish(sends);
        }
    }

    /// Send every viewer the current view. Every change of pose or extent
    /// ends here; a camera with no extent yet has no view to send.
    fn publish(&self, sends: &mut Sends<'_, Self>) {
        if let Extent::Known(extent) = self.extent {
            self.viewers.send(sends, &pose::view_projection(self.pose, self.lens, extent));
        }
    }
}
