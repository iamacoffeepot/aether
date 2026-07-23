use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use aether_substrate::MailboxWakeSlot;
use aether_substrate::actor::native::PumpedSlot;
use aether_substrate::chassis::settlement::WaitOutcome;
use aether_substrate::mail::MailId;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::{Window, WindowId as WinitWindowId};

use crate::{WindowId, WindowMode, WindowSpec};

use super::{
    DesktopWindowCapability, DesktopWindowCapabilityState, WindowHostAction, WindowHostEffect, resolve_fullscreen,
};

/// Semantic seam between the window application and chassis-owned render,
/// settlement, and process-lifecycle integration.
pub trait DesktopWindowIntegration {
    fn attach_window(&mut self, id: WindowId, window: Arc<Window>) -> Result<(), String>;

    fn detach_window(&mut self, id: WindowId);

    fn windows_dirty(&mut self, windows: &[WindowId]);

    fn window_occluded(&mut self, _id: WindowId, _occluded: bool) {}

    fn request_shutdown(&mut self);

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
        Self { window_slot, integration }
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
    ) {
        let mut dirty = BTreeSet::new();
        self.apply_effects(effects, &mut dirty);

        for action in actions {
            match action {
                WindowHostAction::Create { id, .. } => match action.realize(event_loop) {
                    Ok(Some(window)) => {
                        let staged = self.window_slot.host_turn(|state, _ctx| state.stage_created_window(id, window));
                        match staged {
                            Some(Ok(created)) => self.apply_effects(vec![created], &mut dirty),
                            Some(Err(error)) => {
                                let effects = self
                                    .window_slot
                                    .host_turn(|state, _ctx| state.fail_window_creation(id, error))
                                    .unwrap_or_default();
                                self.apply_effects(effects, &mut dirty);
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
                        self.apply_effects(effects, &mut dirty);
                    }
                },
                WindowHostAction::Close { id } => {
                    apply_simple_effect(&mut self.integration, WindowHostEffect::Closing { id }, &mut dirty);
                    let effects =
                        self.window_slot.host_turn(|state, ctx| state.finish_window_close(id, ctx)).unwrap_or_default();
                    self.apply_effects(effects, &mut dirty);
                }
            }
        }

        if !dirty.is_empty() {
            self.integration.windows_dirty(&dirty.into_iter().collect::<Vec<_>>());
        }
    }

    fn apply_effects(&mut self, effects: Vec<WindowHostEffect>, dirty: &mut BTreeSet<WindowId>) {
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
                    apply_simple_effect(&mut self.integration, simple, dirty);
                }
            }
        }
    }

    fn drain_and_take_work(
        &mut self,
        host_turn: impl FnOnce(&mut DesktopWindowCapabilityState, &mut aether_substrate::NativeCtx<'_>),
    ) -> (Vec<WindowHostAction>, Vec<WindowHostEffect>) {
        self.window_slot.drain_available();
        self.window_slot
            .host_turn(|state, ctx| {
                host_turn(state, ctx);
                state.take_host_work()
            })
            .unwrap_or_default()
    }
}

impl<I: DesktopWindowIntegration> ApplicationHandler<DesktopWindowUserEvent> for DesktopWindowApplication<I> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let (actions, effects) = self.drain_and_take_work(|_, _| {});
        self.apply_work(event_loop, actions, effects);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: DesktopWindowUserEvent) {
        let (actions, effects) = self.drain_and_take_work(|_, _| {});
        self.apply_work(event_loop, actions, effects);
        if event == DesktopWindowUserEvent::Quit {
            self.integration.request_shutdown();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WinitWindowId, event: WindowEvent) {
        let (actions, effects) = self.drain_and_take_work(|state, ctx| state.window_event(window_id, event, ctx));
        self.apply_work(event_loop, actions, effects);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let (actions, effects) = self.drain_and_take_work(|_, _| {});
        self.apply_work(event_loop, actions, effects);
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
) {
    match effect {
        WindowHostEffect::Created { .. } => unreachable!("created effects require actor completion"),
        WindowHostEffect::Closing { id } => integration.detach_window(id),
        WindowHostEffect::Dirty { id } => {
            dirty.insert(id);
        }
        WindowHostEffect::Occluded { id, occluded } => integration.window_occluded(id, occluded),
        WindowHostEffect::LastWindowClosed => integration.request_shutdown(),
    }
}

#[cfg(test)]
mod tests {
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

        fn pump_while_settling(&mut self, _settlement: MailId) -> WaitOutcome {
            panic!("the simple-effect test never enters settlement")
        }
    }

    #[test]
    fn detach_happens_before_last_window_shutdown() {
        let mut integration = SpyIntegration::default();
        let mut dirty = BTreeSet::new();

        apply_simple_effect(&mut integration, WindowHostEffect::Closing { id: WindowId(4) }, &mut dirty);
        apply_simple_effect(&mut integration, WindowHostEffect::LastWindowClosed, &mut dirty);

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
}
