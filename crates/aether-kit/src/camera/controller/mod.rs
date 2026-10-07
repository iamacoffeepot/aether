// `#[handler]` methods take the decoded mail by value per the ADR-0033
// dispatch ABI; the macro-generated trampoline owns the payload.
#![allow(clippy::needless_pass_by_value)]

//! [`CameraController`] — a mouse and keyboard driver for one
//! [`camera`](crate::camera) instance, at `aether.kit.camera-controller:<key>`.
//!
//! Turns the input of one window into poses and mails them to the camera its
//! config names. It sends only [`Pose`], the message any script or agent
//! sends, so the camera stays a pose and a lens and all input policy lives
//! here.
//!
//! # Controls
//!
//! - **Left-drag** orbits: the yaw and the pitch follow the cursor, and the
//!   pitch stops where the camera looks straight down or up.
//! - **The wheel** zooms: each step multiplies the distance, so a step is
//!   the same visual change at any scale, within the config's `nearest` and
//!   `farthest`.
//! - **Right-drag or middle-drag** pans: the point of the scene under the
//!   cursor at the press stays under the cursor.
//! - **W / A / S / D and the arrows** pan the target across the ground,
//!   relative to the camera's yaw, at a rate that scales with the camera's
//!   distance.
//! - **Q / E** turn the camera about its target.
//!
//! # Design
//!
//! The handlers only record input. Once a tick the controller turns what
//! was recorded into the next pose and sends it if it differs, so the camera gets at most one [`Pose`] a tick and none while
//! nothing is held. Rates use the tick's elapsed time.
//!
//! **It reads before it writes.** A gesture starts when input arrives while
//! nothing is held. The controller then asks the camera [`Where`], and the
//! answer is the pose the gesture steps from. When the last key and button
//! are released the gesture ends and the controller forgets the pose. So a
//! [`Pose`], a `Frame` or a `Glide` sent from elsewhere between gestures
//! stands, and the next gesture continues from wherever the camera is.
//!
//! **It is a viewer of its camera.** It subscribes to the camera's view
//! (`aether.render.view_subscribe`) and keeps the last one. A drag pan casts
//! its rays through the view the drag began in, which keeps the grabbed
//! point under the cursor for either lens with no request per mouse move.
//! Until the camera has published a view, a camera following a window whose
//! size has not arrived, a drag pan waits.
//!
//! **One window.** Every input event names its window, and the controller
//! ignores any that is not the config's. When that window loses focus the
//! controller drops every held key and button and ends the gesture: the
//! releases went to another window and will never arrive.
//!
//! A controller whose camera does not prove at `wire` fails its birth, so
//! load the camera first. `unwire` unsubscribes from the view. A republish
//! does not run `wire`, so `on_rehydrate` proves the camera and subscribes
//! again; the replacement starts with nothing held.
//!
//! # Config
//!
//! [`ControllerConfig`] (init-config, ADR-0090) names the camera and the
//! window and sets the rates and the zoom range.

mod gesture;
mod kinds;
pub use kinds::*;

use aether_actor::{ActorInitError, ActorRef, PriorState, ReplyMode, ResolveError, WasmActor, WasmCtx, WasmDropCtx};
use aether_actor::{WasmInitCtx, actor};
use aether_data::{ErasedActorPath, Kind};
use aether_kinds::{Key, KeyRelease, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, Tick};
use aether_lifecycle::LifecycleCapability;
use aether_math::Vec2;
use aether_render::{ViewProjection, ViewSubscribe, ViewUnsubscribe};
use aether_window::{WindowCapability, WindowFocus};

use crate::camera::{CameraComponent, Pose, Where};
use gesture::Input;

/// The controller's hold on the camera it drives.
#[derive(Debug, Clone)]
enum Link {
    /// Not wired yet, or a republish found the camera gone; input moves
    /// nothing.
    Unlinked,
    /// The camera is proven and subscribed to, and has published no view
    /// yet.
    Subscribed(ActorRef<CameraComponent>),
    /// The camera's last published view.
    Viewing { camera: ActorRef<CameraComponent>, view: ViewProjection },
}

impl Link {
    const fn camera(&self) -> Option<ActorRef<CameraComponent>> {
        match self {
            Self::Unlinked => None,
            Self::Subscribed(camera) | Self::Viewing { camera, .. } => Some(*camera),
        }
    }

    const fn view(&self) -> Option<&ViewProjection> {
        match self {
            Self::Unlinked | Self::Subscribed(_) => None,
            Self::Viewing { view, .. } => Some(view),
        }
    }
}

/// Whether the controller is moving the camera.
#[derive(Debug, Clone, Copy)]
enum Gesture {
    /// Nothing is held. The camera's pose is whoever set it last, and the
    /// controller keeps no copy of it.
    Idle,
    /// Input arrived while nothing was held, and the camera has been asked
    /// where it is. It stays here until the camera answers, so one question
    /// is out at a time and each answer is to the gesture that asked.
    Asked,
    /// The camera answered: `pose` is what the input steps, and what the
    /// camera was last sent.
    Driving { pose: Pose },
}

/// What a controller carries across a republish: nothing of its own, since
/// the camera holds the pose. Saving it is what makes the replacement's
/// `on_rehydrate` run.
#[aether_data::kind(name = "aether.kit.camera-controller.state", copy, eq, no_serde)]
struct ControllerState;

/// Mouse and keyboard driver for one camera instance.
pub struct CameraController {
    config: ControllerConfig,
    input: Input,
    link: Link,
    gesture: Gesture,
}

/// One camera's mouse and keyboard controls.
///
/// # Agent
/// Spawn one per camera with `load_component` / `spawn`, `namespace:
/// "aether.kit.camera-controller"`, a `key` and a [`ControllerConfig`]
/// naming the camera and the window; the camera must be live first. It moves
/// the camera only while a key or a button is held in that window, and it
/// starts each gesture from the camera's own pose, so a pose you send the
/// camera between gestures stands. To move the camera yourself send it
/// `aether.kit.camera.pose`; nothing here needs to be mailed.
#[actor(instanced, root, depends(WindowCapability, LifecycleCapability))]
impl WasmActor for CameraController {
    type Config = ControllerConfig;
    const NAMESPACE: &'static str = "aether.kit.camera-controller";

    fn init(config: ControllerConfig, _ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        if config.nearest.get() > config.farthest.get() {
            return Err(ActorInitError::new(format!("{} has its nearest beyond its farthest", ControllerConfig::NAME)));
        }

        Ok(Self { config, input: Input::default(), link: Link::Unlinked, gesture: Gesture::Idle })
    }

    /// Subscribe the window's input streams and the tick stage, then prove
    /// the camera and subscribe to its view. `init`'s ctx can't mail.
    ///
    /// # Errors
    ///
    /// When the config's camera path does not prove: a controller with no
    /// camera would take input and move nothing, so its birth fails and the
    /// load that asked is told which path.
    fn wire(&mut self, ctx: &mut aether_actor::WireCtx<'_, '_>) -> Result<(), ActorInitError> {
        ctx.subscribe::<WindowCapability, Key>();
        ctx.subscribe::<WindowCapability, KeyRelease>();
        ctx.subscribe::<WindowCapability, MouseButton>();
        ctx.subscribe::<WindowCapability, MouseButtonRelease>();
        ctx.subscribe::<WindowCapability, MouseMove>();
        ctx.subscribe::<WindowCapability, MouseWheel>();
        ctx.subscribe::<WindowCapability, WindowFocus>();
        ctx.subscribe::<LifecycleCapability, Tick>();

        self.follow_camera(ctx).map_err(|error| ActorInitError::new(format!("the controller's camera: {error}")))
    }

    /// Stop taking the camera's view.
    fn unwire(&mut self, ctx: &mut WasmCtx<'_>) {
        if let Some(camera) = self.link.camera() {
            ctx.send_to(camera, &ViewUnsubscribe);
        }
        self.link = Link::Unlinked;
    }

    fn on_dehydrate(&mut self, ctx: &mut WasmDropCtx<'_>) -> Result<(), ActorInitError> {
        ctx.save_state_kind(0, &ControllerState)
    }

    /// Prove the camera and subscribe to its view again: a republish does
    /// not run `wire`, the instance it replaced unsubscribed in `unwire`,
    /// and a reference does not outlive the instance that proved it. The
    /// input subscriptions are the mailbox's and carry over. A camera that no
    /// longer proves is logged and input moves nothing: returning the error
    /// would close an instance reinstated after an aborted republish.
    fn on_rehydrate(&mut self, ctx: &mut WasmCtx<'_>, _prior: PriorState<'_>) -> Result<(), ActorInitError> {
        if let Err(error) = self.follow_camera(ctx) {
            tracing::error!(target: "aether_kit", %error, "the controller's camera does not prove; input moves nothing");
        }
        Ok(())
    }

    /// A bound key went down: it is held until its release or a loss of
    /// focus.
    ///
    /// # Agent
    /// Published by the window manager; not useful to send manually.
    #[handler::event]
    fn on_key(&mut self, ctx: &mut WasmCtx<'_>, key: Key) {
        if self.reads(&key.window) {
            self.input.key(key.code, true);
            self.settle(ctx);
        }
    }

    /// A key came up.
    ///
    /// # Agent
    /// Published by the window manager; not useful to send manually.
    #[handler::event]
    fn on_key_release(&mut self, ctx: &mut WasmCtx<'_>, key: KeyRelease) {
        if self.reads(&key.window) {
            self.input.key(key.code, false);
            self.settle(ctx);
        }
    }

    /// A press begins a drag when none is in progress: the left button an
    /// orbit, the right or the middle a pan.
    ///
    /// # Agent
    /// Published by the window manager; not useful to send manually.
    #[handler::event]
    fn on_mouse_button(&mut self, ctx: &mut WasmCtx<'_>, press: MouseButton) {
        if self.reads(&press.window) {
            self.input.press(press.button, Vec2::new(press.x, press.y));
            self.settle(ctx);
        }
    }

    /// A release ends the drag its button began.
    ///
    /// # Agent
    /// Published by the window manager; not useful to send manually.
    #[handler::event]
    fn on_mouse_button_release(&mut self, ctx: &mut WasmCtx<'_>, release: MouseButtonRelease) {
        if self.reads(&release.window) {
            self.input.release(release.button);
            self.settle(ctx);
        }
    }

    /// The cursor moves the drag in progress. With no drag it moves nothing,
    /// so it never starts a gesture.
    ///
    /// # Agent
    /// Published by the window manager; not useful to send manually.
    #[handler::event]
    fn on_mouse_move(&mut self, _ctx: &mut WasmCtx<'_>, moved: MouseMove) {
        if self.reads(&moved.window) {
            self.input.cursor(Vec2::new(moved.x, moved.y));
        }
    }

    /// The wheel's vertical travel zooms; away from the user zooms in.
    ///
    /// # Agent
    /// Published by the window manager; not useful to send manually.
    #[handler::event]
    fn on_mouse_wheel(&mut self, ctx: &mut WasmCtx<'_>, wheel: MouseWheel) {
        if self.reads(&wheel.window) {
            self.input.wheel(wheel.delta_y);
            self.settle(ctx);
        }
    }

    /// The window lost focus: drop everything held and end the gesture. The
    /// releases went to another window, so without this a held key would
    /// keep moving the camera until it was pressed and released again.
    ///
    /// # Agent
    /// Published by the window manager; not useful to send manually.
    #[handler::event]
    fn on_window_focus(&mut self, ctx: &mut WasmCtx<'_>, focus: WindowFocus) {
        let lost = !focus.focused && self.reads(&focus.window);
        if lost {
            self.input = Input::default();
            self.settle(ctx);
        }
    }

    /// Step the gesture's pose by what was recorded and, if that moved it,
    /// send the camera the pose. No gesture, or one whose camera has not
    /// answered yet, sends nothing and keeps what was recorded.
    ///
    /// # Agent
    /// Lifecycle-driven; not useful to send manually.
    #[handler::event]
    fn on_tick(&mut self, ctx: &mut WasmCtx<'_>, tick: Tick) {
        let Gesture::Driving { pose } = self.gesture else {
            return;
        };
        let Some(camera) = self.link.camera() else {
            return;
        };

        let next = self.input.step(pose, self.link.view(), tick.delta_seconds(), &self.config);
        if next != pose {
            ctx.send_to(camera, &next);
        }
        self.gesture = Gesture::Driving { pose: next };
        self.settle(ctx);
    }

    /// The camera's answer to [`Where`]: the pose the gesture that asked
    /// steps from. Input that was released before the answer came leaves no
    /// gesture to start.
    #[handler::response]
    fn on_pose(&mut self, _ctx: &mut WasmCtx<'_>, pose: Pose) {
        let released = self.input.at_rest();
        self.gesture = match self.gesture {
            Gesture::Asked if released => Gesture::Idle,
            Gesture::Asked => Gesture::Driving { pose },
            Gesture::Idle | Gesture::Driving { .. } => self.gesture,
        };
    }

    /// Keep the view the camera sent; a drag pan casts its rays through it.
    ///
    /// # Agent
    /// Sent by the camera this controller subscribed to; not useful to send
    /// manually.
    #[handler::event]
    fn on_view(&mut self, _ctx: &mut WasmCtx<'_>, view: ViewProjection) {
        if let Some(camera) = self.link.camera() {
            self.link = Link::Viewing { camera, view };
        }
    }
}

impl CameraController {
    /// Prove the config's camera path and subscribe to its view. A path
    /// that does not prove leaves the controller unlinked.
    fn follow_camera(&mut self, ctx: &mut WasmCtx<'_, Self>) -> Result<(), ResolveError> {
        self.link = Link::Unlinked;
        let camera = ctx.resolve(&self.config.camera)?;
        ctx.send_to(camera, &ViewSubscribe);
        self.link = Link::Subscribed(camera);

        Ok(())
    }

    /// Whether `window` is the window whose input this controller reads.
    fn reads(&self, window: &ErasedActorPath) -> bool {
        self.config.window.as_erased() == window
    }

    /// Bring the gesture in line with what is held, after any change to the
    /// input: start one when input arrived while idle, end one when the
    /// last of it was released. A gesture that has asked waits for its
    /// answer either way.
    fn settle<S, M: ReplyMode>(&mut self, ctx: &mut WasmCtx<'_, Self, S, M>) {
        let at_rest = self.input.at_rest();
        match self.gesture {
            Gesture::Idle if !at_rest => self.ask(ctx),
            Gesture::Driving { .. } if at_rest => self.gesture = Gesture::Idle,
            Gesture::Idle | Gesture::Asked | Gesture::Driving { .. } => {}
        }
    }

    /// Ask the camera where it is, to start a gesture from its answer. With
    /// no camera there is nothing to ask and no gesture starts.
    fn ask<S, M: ReplyMode>(&mut self, ctx: &mut WasmCtx<'_, Self, S, M>) {
        if let Some(camera) = self.link.camera() {
            ctx.send_to(camera, &Where);
            self.gesture = Gesture::Asked;
        }
    }
}
