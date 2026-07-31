use std::collections::{BTreeSet, VecDeque};
use std::mem;
use std::sync::Arc;
use std::time::Instant;

use aether_substrate::MailboxWakeSlot;
use aether_substrate::actor::native::PumpedSlot;
use aether_substrate::chassis::settlement::WaitOutcome;
use aether_substrate::mail::MailId;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::{Window, WindowId as WinitWindowId};

use crate::{WindowId, WindowMode, WindowSpec};

use super::{
    DesktopWindowCapabilityState, DesktopWindowLifecycle, WindowHostAction, WindowHostEffect, resolve_fullscreen,
};
use crate::DesktopWindowCapability;

/// Semantic seam between the window application and chassis-owned render,
/// settlement, and process-lifecycle integration.
pub trait DesktopWindowIntegration {
    fn attach_window(&mut self, id: WindowId, window: Arc<Window>) -> Result<(), String>;

    fn detach_window(&mut self, id: WindowId);

    fn windows_dirty(&mut self, windows: &[WindowId]);

    fn window_occluded(&mut self, _id: WindowId, _occluded: bool) {}

    fn request_shutdown(&mut self);

    fn drain_available(&mut self);

    fn capture_deadline(&self) -> Option<Instant>;

    fn should_exit(&self) -> bool;

    fn pump_while_settling(&mut self, settlement: MailId) -> WaitOutcome;
}

/// User events understood by [`DesktopWindowApplication`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DesktopWindowUserEvent {
    /// One of the application-thread pumped mailboxes accepted mail.
    WindowMail,
    /// A signal or host request should enter graceful engine shutdown.
    Quit,
}

/// Winit application owned by `aether-window`.
///
/// Construction and `run_app` remain chassis responsibilities; this value
/// neither spawns nor transfers the application thread.
pub struct DesktopWindowApplication<I> {
    window_slot: PumpedSlot<DesktopWindowCapability>,
    integration: I,
    pending_dirty: BTreeSet<WindowId>,
    shutdown_requested: bool,
}

impl<I: DesktopWindowIntegration> DesktopWindowApplication<I> {
    pub fn new(
        mut window_slot: PumpedSlot<DesktopWindowCapability>,
        integration: I,
        initial_window: WindowSpec,
    ) -> Self {
        let _ = window_slot.host_turn(|state, _ctx| {
            if state.queue_initial_window(initial_window).is_err() {
                state.pending_host_effects.push(WindowHostEffect::LastWindowClosed);
            }
        });
        Self { window_slot, integration, pending_dirty: BTreeSet::new(), shutdown_requested: false }
    }

    /// Install the event-loop wake for one pumped mailbox.
    pub fn install_wake(proxy: EventLoopProxy<DesktopWindowUserEvent>, wake: &MailboxWakeSlot) {
        wake.set(Arc::new(move || {
            let _ = proxy.send_event(DesktopWindowUserEvent::WindowMail);
        }));
    }

    #[must_use]
    pub fn integration(&self) -> &I {
        &self.integration
    }

    pub fn integration_mut(&mut self) -> &mut I {
        &mut self.integration
    }

    /// Run the pumped actor's closed path after `run_app` returns.
    pub fn shutdown(&mut self) {
        self.window_slot.shutdown();
    }

    fn apply_work(
        &mut self,
        event_loop: &ActiveEventLoop,
        actions: Vec<WindowHostAction>,
        effects: Vec<WindowHostEffect>,
    ) -> (BTreeSet<WindowId>, bool) {
        let mut dirty = BTreeSet::new();
        let mut should_shutdown = false;
        self.apply_effects(effects, &mut dirty, &mut should_shutdown);

        for action in actions {
            match action {
                WindowHostAction::Create { id, .. } => match action.realize(event_loop) {
                    Ok(Some(window)) => {
                        let staged = self.window_slot.host_turn(|state, _ctx| state.stage_created_window(id, window));
                        match staged {
                            Some(Ok(created)) => {
                                self.apply_effects(vec![created], &mut dirty, &mut should_shutdown);
                            }
                            Some(Err(error)) => {
                                let effects = self
                                    .window_slot
                                    .host_turn(|state, _ctx| state.fail_window_creation(id, error))
                                    .unwrap_or_default();
                                self.apply_effects(effects, &mut dirty, &mut should_shutdown);
                            }
                            None => {}
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let effects = self
                            .window_slot
                            .host_turn(|state, _ctx| state.fail_window_creation(id, error))
                            .unwrap_or_default();
                        self.apply_effects(effects, &mut dirty, &mut should_shutdown);
                    }
                },
                WindowHostAction::Close { id } => {
                    should_shutdown |=
                        apply_simple_effect(&mut self.integration, WindowHostEffect::Closing { id }, &mut dirty);
                    let effects =
                        self.window_slot.host_turn(|state, ctx| state.finish_window_close(id, ctx)).unwrap_or_default();
                    self.apply_effects(effects, &mut dirty, &mut should_shutdown);
                }
            }
        }

        (dirty, should_shutdown)
    }

    fn apply_effects(
        &mut self,
        effects: Vec<WindowHostEffect>,
        dirty: &mut BTreeSet<WindowId>,
        should_shutdown: &mut bool,
    ) {
        let mut effects = VecDeque::from(effects);
        while let Some(effect) = effects.pop_front() {
            match effect {
                WindowHostEffect::Created { id, window } => {
                    let attachment = self.integration.attach_window(id, Arc::clone(&window));
                    let follow_up = self
                        .window_slot
                        .host_turn(|state, ctx| state.finish_window_attachment(id, attachment, ctx))
                        .unwrap_or_default();
                    effects.extend(follow_up);
                }
                simple => {
                    *should_shutdown |= apply_simple_effect(&mut self.integration, simple, dirty);
                }
            }
        }
    }

    fn drain_and_take_work(
        &mut self,
        host_turn: impl FnOnce(
            &mut DesktopWindowCapabilityState,
            &mut aether_substrate::NativeCtx<'_, aether_actor::Single, DesktopWindowCapability>,
        ),
    ) -> (Vec<WindowHostAction>, Vec<WindowHostEffect>) {
        self.window_slot.drain_available();
        self.window_slot
            .host_turn(|state, ctx| {
                host_turn(state, ctx);
                state.take_host_work()
            })
            .unwrap_or_default()
    }

    fn turn(
        &mut self,
        event_loop: &ActiveEventLoop,
        request_shutdown: bool,
        flush_frame: bool,
        host_turn: impl FnOnce(
            &mut DesktopWindowCapabilityState,
            &mut aether_substrate::NativeCtx<'_, aether_actor::Single, DesktopWindowCapability>,
        ),
    ) {
        self.integration.drain_available();
        let (actions, effects) = self.drain_and_take_work(host_turn);
        let (dirty, last_window_closed) = self.apply_work(event_loop, actions, effects);
        self.pending_dirty.extend(dirty);

        if request_shutdown || last_window_closed {
            request_shutdown_once(&mut self.integration, &mut self.shutdown_requested);
        }

        let snapshot = self.window_slot.host_turn(|state, _ctx| state.application_snapshot()).unwrap_or_default();
        if flush_frame {
            let now = Instant::now();
            let capture_expired = self.integration.capture_deadline().is_some_and(|deadline| deadline <= now);
            let dirty = mem::take(&mut self.pending_dirty);
            let frame_windows = snapshot.frame_windows(&dirty, self.shutdown_requested || capture_expired);
            if !frame_windows.is_empty() || self.shutdown_requested || capture_expired {
                self.integration.windows_dirty(&frame_windows);
            }
        }

        let disposition = loop_disposition(
            self.integration.should_exit(),
            self.shutdown_requested,
            !snapshot.visible.is_empty(),
            self.integration.capture_deadline(),
            Instant::now(),
        );
        match disposition {
            LoopDisposition::Exit => event_loop.exit(),
            LoopDisposition::Poll => event_loop.set_control_flow(ControlFlow::Poll),
            LoopDisposition::Wait => event_loop.set_control_flow(ControlFlow::Wait),
            LoopDisposition::WaitUntil(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
        }

        if disposition != LoopDisposition::Exit {
            for (_, window) in snapshot.visible {
                window.request_redraw();
            }
        }
    }
}

impl<I: DesktopWindowIntegration> ApplicationHandler<DesktopWindowUserEvent> for DesktopWindowApplication<I> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.turn(event_loop, false, false, |_, _| {});
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: DesktopWindowUserEvent) {
        self.turn(event_loop, event == DesktopWindowUserEvent::Quit, false, |_, _| {});
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WinitWindowId, event: WindowEvent) {
        self.turn(event_loop, false, false, |state, ctx| state.window_event(window_id, event, ctx));
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.turn(event_loop, false, true, |_, _| {});
    }
}

#[derive(Default)]
struct WindowSnapshot {
    live: Vec<WindowId>,
    visible: Vec<(WindowId, Arc<Window>)>,
}

impl WindowSnapshot {
    fn frame_windows(&self, dirty: &BTreeSet<WindowId>, force: bool) -> Vec<WindowId> {
        if force {
            return self.live.clone();
        }
        self.visible.iter().map(|(id, _)| *id).filter(|id| dirty.contains(id)).collect()
    }
}

impl DesktopWindowCapabilityState {
    fn application_snapshot(&self) -> WindowSnapshot {
        let mut snapshot = WindowSnapshot::default();
        for (id, state) in &self.windows {
            if state.lifecycle != DesktopWindowLifecycle::Live {
                continue;
            }
            snapshot.live.push(*id);
            if !state.occluded
                && let Some(window) = self.native_windows.get(id)
            {
                snapshot.visible.push((*id, Arc::clone(window)));
            }
        }
        snapshot
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum LoopDisposition {
    Exit,
    Poll,
    Wait,
    WaitUntil(Instant),
}

fn loop_disposition(
    should_exit: bool,
    shutdown_requested: bool,
    has_visible_windows: bool,
    capture_deadline: Option<Instant>,
    now: Instant,
) -> LoopDisposition {
    if should_exit {
        LoopDisposition::Exit
    } else if shutdown_requested || has_visible_windows {
        LoopDisposition::Poll
    } else if let Some(deadline) = capture_deadline
        && deadline > now
    {
        LoopDisposition::WaitUntil(deadline)
    } else {
        LoopDisposition::Wait
    }
}

fn request_shutdown_once<I: DesktopWindowIntegration>(integration: &mut I, shutdown_requested: &mut bool) {
    if !*shutdown_requested {
        *shutdown_requested = true;
        integration.request_shutdown();
    }
}

impl WindowHostAction {
    /// Realize this action against the callback-scoped event loop.
    ///
    /// Close has no direct winit call: detachment happens through the
    /// integration and dropping the manager's final `Arc<Window>` completes
    /// native closure.
    pub fn realize(&self, event_loop: &ActiveEventLoop) -> Result<Option<Arc<Window>>, String> {
        let Self::Create { spec, .. } = self else {
            return Ok(None);
        };
        let mut attributes = Window::default_attributes().with_title(&spec.title);
        if matches!(&spec.mode, WindowMode::Windowed)
            && let Some(size) = spec.size
        {
            attributes = attributes.with_inner_size(PhysicalSize::new(size.width, size.height));
        }
        attributes = attributes.with_fullscreen(resolve_fullscreen(&spec.mode, event_loop.primary_monitor().as_ref())?);
        let window = Arc::new(event_loop.create_window(attributes).map_err(|error| error.to_string())?);
        window.set_ime_allowed(true);
        window.request_redraw();
        Ok(Some(window))
    }
}

fn apply_simple_effect<I: DesktopWindowIntegration>(
    integration: &mut I,
    effect: WindowHostEffect,
    dirty: &mut BTreeSet<WindowId>,
) -> bool {
    match effect {
        WindowHostEffect::Created { .. } => unreachable!("created effects require actor completion"),
        WindowHostEffect::Closing { id } => integration.detach_window(id),
        WindowHostEffect::Dirty { id } => {
            dirty.insert(id);
        }
        WindowHostEffect::Occluded { id, occluded } => integration.window_occluded(id, occluded),
        WindowHostEffect::LastWindowClosed => return true,
    }
    false
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[derive(Default)]
    struct SpyIntegration {
        calls: Vec<String>,
    }

    impl DesktopWindowIntegration for SpyIntegration {
        fn attach_window(&mut self, id: WindowId, _window: Arc<Window>) -> Result<(), String> {
            self.calls.push(format!("attach:{}", id.0));
            Ok(())
        }

        fn detach_window(&mut self, id: WindowId) {
            self.calls.push(format!("detach:{}", id.0));
        }

        fn windows_dirty(&mut self, windows: &[WindowId]) {
            self.calls.push(format!(
                "dirty:{}",
                windows.iter().map(|window| window.0.to_string()).collect::<Vec<_>>().join(","),
            ));
        }

        fn window_occluded(&mut self, id: WindowId, occluded: bool) {
            self.calls.push(format!("occluded:{}:{occluded}", id.0));
        }

        fn request_shutdown(&mut self) {
            self.calls.push("shutdown".to_owned());
        }

        fn drain_available(&mut self) {}

        fn capture_deadline(&self) -> Option<Instant> {
            None
        }

        fn should_exit(&self) -> bool {
            false
        }

        fn pump_while_settling(&mut self, _settlement: MailId) -> WaitOutcome {
            panic!("the simple-effect test never enters settlement")
        }
    }

    #[test]
    fn detach_happens_before_last_window_shutdown() {
        let mut integration = SpyIntegration::default();
        let mut dirty = BTreeSet::new();

        apply_simple_effect(&mut integration, WindowHostEffect::Closing { id: WindowId(4) }, &mut dirty);
        let should_shutdown = apply_simple_effect(&mut integration, WindowHostEffect::LastWindowClosed, &mut dirty);
        if should_shutdown {
            let mut shutdown_requested = false;
            request_shutdown_once(&mut integration, &mut shutdown_requested);
        }

        assert_eq!(integration.calls, ["detach:4", "shutdown"]);
    }

    #[test]
    fn dirty_windows_coalesce_in_identity_order() {
        let mut integration = SpyIntegration::default();
        let mut dirty = BTreeSet::new();
        for id in [WindowId(8), WindowId(2), WindowId(8)] {
            apply_simple_effect(&mut integration, WindowHostEffect::Dirty { id }, &mut dirty);
        }

        integration.windows_dirty(&dirty.into_iter().collect::<Vec<_>>());

        assert_eq!(integration.calls, ["dirty:2,8"]);
    }

    #[test]
    fn occlusion_is_semantic_not_a_raw_winit_event() {
        let mut integration = SpyIntegration::default();
        let mut dirty = BTreeSet::new();

        apply_simple_effect(
            &mut integration,
            WindowHostEffect::Occluded { id: WindowId(3), occluded: true },
            &mut dirty,
        );

        assert_eq!(integration.calls, ["occluded:3:true"]);
    }

    #[test]
    fn shutdown_request_is_idempotent() {
        let mut integration = SpyIntegration::default();
        let mut shutdown_requested = false;

        request_shutdown_once(&mut integration, &mut shutdown_requested);
        request_shutdown_once(&mut integration, &mut shutdown_requested);

        assert_eq!(integration.calls, ["shutdown"]);
    }

    #[test]
    fn terminal_disposition_exits_without_a_native_window() {
        let now = Instant::now();

        assert_eq!(loop_disposition(true, true, false, None, now), LoopDisposition::Exit);
        assert_eq!(loop_disposition(false, true, false, None, now), LoopDisposition::Poll);
        assert_eq!(loop_disposition(false, false, false, None, now), LoopDisposition::Wait);
        assert_eq!(
            loop_disposition(false, false, false, Some(now + Duration::from_secs(1)), now),
            LoopDisposition::WaitUntil(now + Duration::from_secs(1)),
        );
    }
}
