//! The view source the renderer follows (`aether.render.view_from`).
//!
//! The renderer is a subscriber like any other viewer: told which
//! [`ViewSource`] to follow, it sends that source [`ViewSubscribe`] and takes
//! each [`ViewProjection`](crate::ViewProjection) the source sends back
//! through its ordinary handler. What it keeps is the source's proven
//! reference and the monitor that reports the source's close, so the
//! subscription is released on both ways a source stops being followed: a
//! later `view_from` sends the old source [`ViewUnsubscribe`], and a source
//! that closes clears the hold through its `MonitorNotice`. Monitoring never
//! fails, so the renderer never follows a source whose close it cannot hear.

use aether_actor::{ErasedActorRef, PathRefused, ProtocolPath, ProtocolRef, ReplyMode};
use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::actor::native::NativeCtx;

use super::RenderCapabilityState;
use crate::{ViewFromResult, ViewSource, ViewSubscribe, ViewUnsubscribe};

/// The [`ViewSource`] the renderer is subscribed to.
pub(super) struct FollowedView {
    source: ProtocolRef<ViewSource>,
    /// Reports the source's close; dropping it deregisters. Every followed
    /// source has one, so every hold has a release.
    _monitor: MonitorHandle,
}

impl FollowedView {
    fn is(&self, actor: ErasedActorRef) -> bool {
        self.source.erase() == actor
    }
}

impl RenderCapabilityState {
    /// Whether `actor` is the view source the renderer follows.
    fn follows_view_of(&self, actor: ErasedActorRef) -> bool {
        self.view_source.as_ref().is_some_and(|followed| followed.is(actor))
    }

    /// Follow the view source at `path`: unsubscribe from the source followed
    /// before, subscribe to this one, and monitor it.
    ///
    /// The path is proven live before anything changes, so a refused request
    /// leaves the renderer following the source it had. A source that closed
    /// between that proof and the monitor is followed like any other and
    /// released by its notice, which arrives after this handler returns, the
    /// same as a source that closes a moment later. A
    /// request naming the source already followed keeps the hold
    /// and subscribes again, which a source answers with its current view;
    /// that is how a viewer rejoins a source that lost its subscribers.
    pub(super) fn follow_view<A, S, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, S, M>,
        path: &ProtocolPath<ViewSource>,
    ) -> ViewFromResult {
        let source = match ctx.resolve(path) {
            Ok(source) => source,
            Err(error) => return PathRefused::from(error).into(),
        };

        if !self.follows_view_of(source.erase()) {
            let monitor = ctx.monitor(source);
            let previous = self.view_source.replace(FollowedView { source, _monitor: monitor });

            if let Some(previous) = previous {
                ctx.send_to(previous.source, &ViewUnsubscribe);
            }
        }
        ctx.send_to(source, &ViewSubscribe);

        ViewFromResult::Ok
    }

    /// Stop following `departed` if it is the followed source. The renderer
    /// keeps the last view it was sent, as it does between any two views.
    pub(super) fn release_view_of(&mut self, departed: ErasedActorRef) {
        if self.follows_view_of(departed) {
            self.view_source = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use aether_actor::{ActorPath, ActorRef, PathRefusal, Subscriber};
    use aether_data::{LoadName, SessionToken, Uuid};
    use aether_kinds::Quit;
    use aether_math::{Mat4, Vec3};
    use aether_substrate::actor::native::{NativeActor, NativeInitCtx};
    use aether_substrate::mail::outbound::EgressEvent;
    use aether_substrate::testing::{
        PumpedDriver, boot_bare_test_chassis, decode_session_reply, fresh_substrate_and_rx,
    };
    use aether_substrate::{BootError, ReplyTarget, Subname};

    use super::super::{DEFAULT_CLEAR_COLOR, RenderParams, RenderTuningConfig};
    use super::*;
    use crate::{RenderCapability, ViewFrom, ViewProjection, ViewportExtent};

    /// What a [`Source`] was asked, forwarded to the test.
    #[derive(Debug, PartialEq)]
    enum Asked {
        Subscribe,
        Unsubscribe,
    }

    /// What a [`Source`] publishes and where it reports what it was asked.
    #[derive(Clone)]
    struct SourceConfig {
        view: ViewProjection,
        asked: mpsc::Sender<Asked>,
    }

    /// A keyed view source: it answers `ViewSubscribe` with its one view,
    /// sent through the sender cast to a `ViewProjection` subscriber, as a
    /// camera does. `Quit` closes it, so the runtime posts a `MonitorNotice`
    /// to the renderer following it.
    struct Source {
        config: SourceConfig,
    }

    #[aether_actor::actor(instanced, root)]
    impl NativeActor for Source {
        const NAMESPACE: &'static str = "test.render.view_source";
        type Config = SourceConfig;

        fn init(config: SourceConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { config })
        }

        #[handler::tell]
        fn on_subscribe(&mut self, ctx: &mut NativeCtx<'_>, _mail: ViewSubscribe) {
            let viewer = ctx
                .sender()
                .and_then(|sender| ctx.cast::<Subscriber<ViewProjection>>(sender))
                .expect("the subscribing sender takes a view silently");

            ctx.send_to(viewer, &self.config.view);
            let _ = self.config.asked.send(Asked::Subscribe);
        }

        #[handler::tell]
        fn on_unsubscribe(&mut self, _ctx: &mut NativeCtx<'_>, _mail: ViewUnsubscribe) {
            let _ = self.config.asked.send(Asked::Unsubscribe);
        }

        #[handler::tell]
        fn on_quit(&mut self, ctx: &mut NativeCtx<'_>, _quit: Quit) {
            let _ = self;
            ctx.shutdown();
        }
    }

    fn source_path(key: &str) -> ActorPath<Source> {
        ActorPath::instance(&LoadName::new(key).expect("a valid key"))
    }

    /// A view from `eye` toward the origin. The view and the projection do
    /// not commute, so only `projection * view` matches [`applied`].
    fn view_from_eye(eye: Vec3) -> ViewProjection {
        ViewProjection {
            view: Mat4::look_at_rh(eye, Vec3::ZERO, Vec3::Y),
            projection: Mat4::perspective_rh(1.0, 4.0 / 3.0, 0.1, 100.0),
            eye,
            near: 0.1,
            far: 100.0,
            extent: ViewportExtent { width: 128, height: 96 },
        }
    }

    /// The matrix the renderer applies for `view`.
    fn applied(view: &ViewProjection) -> [f32; 16] {
        (view.projection * view.view).to_cols_array()
    }

    fn session() -> ReplyTarget {
        ReplyTarget::Session { session: SessionToken(Uuid::from_u128(0x7477)), correlation: 1 }
    }

    /// A booted `aether.render` on a pumped slot with no GPU, beside the
    /// egress its session replies leave through.
    struct Booted {
        driver: PumpedDriver<RenderCapability>,
        egress: mpsc::Receiver<EgressEvent>,
    }

    impl Booted {
        fn boot() -> Self {
            let (registry, mailer, egress) = fresh_substrate_and_rx();
            let tuning = RenderTuningConfig {
                vertex_buffer_bytes: 1024,
                clear_color: DEFAULT_CLEAR_COLOR.to_owned(),
                pass_timings: false,
                upload_pieces_per_frame: 32,
            };
            let driver =
                PumpedDriver::boot(boot_bare_test_chassis(&registry, &mailer), tuning, RenderParams::default());

            Self { driver, egress }
        }

        /// Spawn a [`Source`] at `key` publishing `view`, and the channel it
        /// reports what it was asked on.
        fn spawn_source(&self, key: &str, view: ViewProjection) -> (ActorRef<Source>, mpsc::Receiver<Asked>) {
            let (asked, heard) = mpsc::channel();
            let source = self
                .driver
                .chassis()
                .spawn_actor_for_test::<Source>(Subname::Named(key), SourceConfig { view, asked }, ())
                .finish()
                .expect("the source spawns");

            (source, heard)
        }

        /// Close `source` through its own `Quit` handler and wait until its
        /// route stops answering live.
        fn close(&self, source: ActorRef<Source>) {
            self.driver.chassis().send_for_reply(source, &Quit, session());
            self.driver.chassis().await_closed(source.erase());
        }

        /// Tell the renderer to follow the source at `key` and answer its
        /// reply. The source's view rides the request's chain, so it has
        /// been applied when this returns.
        fn follow(&mut self, key: &str) -> ViewFromResult {
            let render = self.driver.chassis().actor_ref::<RenderCapability>();
            let request = ViewFrom { source: source_path(key).narrow() };

            self.driver.send_and_settle(render, &request, Some(session()));
            decode_session_reply(&self.egress)
        }

        fn applied_view(&self) -> [f32; 16] {
            self.driver.read_state(|state| state.camera_state).expect("the render cap is live")
        }
    }

    /// Following a source subscribes the renderer to it, and the view the
    /// source sends back is applied as `projection * view`. A renderer that
    /// answered `Ok` without subscribing would keep identity; one that
    /// multiplied the other way round would apply a different matrix.
    #[test]
    fn a_followed_source_s_view_is_applied_as_projection_times_view() {
        let mut booted = Booted::boot();
        let view = view_from_eye(Vec3::new(1.8, 1.5, 2.2));
        let (_source, heard) = booted.spawn_source("main", view.clone());

        assert_eq!(booted.follow("main"), ViewFromResult::Ok);

        assert_eq!(heard.try_iter().collect::<Vec<_>>(), [Asked::Subscribe]);
        assert_eq!(booted.applied_view(), applied(&view));
    }

    /// Following a second source unsubscribes the first, so one mail switches
    /// the active camera. A renderer that only subscribed to the new source
    /// would keep taking views from both.
    #[test]
    fn following_a_second_source_unsubscribes_the_first() {
        let mut booted = Booted::boot();
        let second_view = view_from_eye(Vec3::new(-3.0, 0.5, 1.0));
        let (_first, first_heard) = booted.spawn_source("first", view_from_eye(Vec3::new(1.8, 1.5, 2.2)));
        let (_second, second_heard) = booted.spawn_source("second", second_view.clone());

        assert_eq!(booted.follow("first"), ViewFromResult::Ok);
        assert_eq!(booted.follow("second"), ViewFromResult::Ok);

        assert_eq!(first_heard.try_iter().collect::<Vec<_>>(), [Asked::Subscribe, Asked::Unsubscribe]);
        assert_eq!(second_heard.try_iter().collect::<Vec<_>>(), [Asked::Subscribe]);
        assert_eq!(booted.applied_view(), applied(&second_view));
    }

    /// A followed source that closes is released through its `MonitorNotice`,
    /// and the renderer keeps the last view it was sent. A renderer that
    /// ignored the notice would hold a reference to a closed actor and send
    /// it an unsubscribe on the next switch.
    #[test]
    fn a_followed_source_that_closes_is_released() {
        let mut booted = Booted::boot();
        let view = view_from_eye(Vec3::new(1.8, 1.5, 2.2));
        let (source, _heard) = booted.spawn_source("main", view.clone());
        assert_eq!(booted.follow("main"), ViewFromResult::Ok);

        booted.close(source);
        booted.driver.pump_until("the closed source's release", |state| state.view_source.is_none());

        assert_eq!(booted.applied_view(), applied(&view));
    }

    /// A request naming a source that has closed is answered with the path
    /// and why, and changes nothing: the source followed before is neither
    /// unsubscribed nor replaced.
    #[test]
    fn a_refused_request_keeps_the_source_already_followed() {
        let mut booted = Booted::boot();
        let (kept, kept_heard) = booted.spawn_source("kept", view_from_eye(Vec3::new(1.8, 1.5, 2.2)));
        let (gone, _gone_heard) = booted.spawn_source("gone", view_from_eye(Vec3::new(-3.0, 0.5, 1.0)));
        booted.close(gone);
        assert_eq!(booted.follow("kept"), ViewFromResult::Ok);

        let refused = booted.follow("gone");

        let expected = PathRefused { path: source_path("gone").as_erased().clone(), reason: PathRefusal::NotLive };
        assert_eq!(refused, ViewFromResult::Err(expected));
        assert_eq!(kept_heard.try_iter().collect::<Vec<_>>(), [Asked::Subscribe]);
        assert!(booted.driver.read_state(|state| state.follows_view_of(kept.erase())).expect("the render cap is live"));
    }
}
