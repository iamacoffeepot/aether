use std::collections::{BTreeSet, VecDeque};
use std::mem;
use std::sync::Arc;
use std::time::Instant;

use aether_substrate::MailboxWakeSlot;
use aether_substrate::chassis::settlement::WaitOutcome;
use aether_substrate::mail::MailId;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::{Window, WindowId as WinitWindowId};

use aether_data::ErasedActorPath;

use crate::{WindowMode, WindowPresentation, WindowSpec};

use super::pacing::{FramePacing, FrameSchedule};
use super::{
    DesktopWindowLifecycle, DesktopWindowSlot, DesktopWindows, WindowHostAction, WindowHostEffect, menu,
    resolve_fullscreen,
};
use crate::WindowCapability;

/// Semantic seam between the window application and chassis-owned render,
/// settlement, and process-lifecycle integration.
pub trait DesktopWindowIntegration {
    /// Attach `window` as a render target whose surface presents as
    /// `presentation` asks. An `Err` fails the window's creation.
    fn attach_window(
        &mut self,
        path: ErasedActorPath,
        window: Arc<Window>,
        presentation: WindowPresentation,
    ) -> Result<(), String>;

    /// Reconfigure the attached window's surface for `presentation`. An
    /// `Err` names why the surface cannot serve it, and leaves the surface
    /// as it was.
    fn set_presentation(&mut self, path: &ErasedActorPath, presentation: WindowPresentation) -> Result<(), String>;

    fn detach_window(&mut self, path: &ErasedActorPath);

    fn windows_dirty(&mut self, windows: &[ErasedActorPath]);

    fn window_occluded(&mut self, _path: &ErasedActorPath, _occluded: bool) {}

    fn request_shutdown(&mut self);

    /// The boot window can be mailed. The chassis makes the engine reachable
    /// here. An `Err` says why it could not, and fails the run.
    fn boot_window_live(&mut self) -> Result<(), String>;

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
    /// Declared before `window_slot` so it drops first: the integration owns
    /// the pumped render slot, which holds each window's surface, so render
    /// closes before the window actor that owns the windows.
    integration: I,
    /// The pumped window actor. It closes when this application drops, which
    /// the chassis does after its passives (ADR-0160 §3).
    window_slot: DesktopWindowSlot,
    pending_dirty: BTreeSet<ErasedActorPath>,
    /// The instant each capped window's next frame is due.
    pacing: FramePacing,
    shutdown_requested: bool,
    /// Whether the engine became reachable. The chassis reads it after the
    /// loop exits and fails a run that did not.
    boot: BootState,
}

/// How far the engine got toward being reachable. The boot window settles
/// once, so the state leaves `Pending` once and then stays.
enum BootState {
    /// The boot window has not settled: nothing can reach the engine yet.
    Pending,
    /// The boot window is live and the chassis made the engine reachable.
    Reachable,
    /// The engine will never be reachable, for the reason carried: the boot
    /// window failed, or the chassis could not make the engine reachable.
    Failed(String),
}

impl BootState {
    /// Record the boot window's settled outcome. The first one stands.
    fn settle(&mut self, outcome: Result<(), String>) {
        if !matches!(self, Self::Pending) {
            return;
        }
        *self = match outcome {
            Ok(()) => Self::Reachable,
            Err(reason) => Self::Failed(reason),
        };
    }
}

impl<I: DesktopWindowIntegration> DesktopWindowApplication<I> {
    pub fn new(mut window_slot: DesktopWindowSlot, integration: I, initial_window: WindowSpec) -> Self {
        let name = initial_window.name.clone();
        let _ = window_slot.host_turn(|state, _ctx| {
            if let Err(reason) = state.queue_initial_window(initial_window) {
                state.fail_initial_window(&name, &reason);
            }
        });
        Self {
            window_slot,
            integration,
            pending_dirty: BTreeSet::new(),
            pacing: FramePacing::default(),
            shutdown_requested: false,
            boot: BootState::Pending,
        }
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

    /// Whether the engine became reachable. `None` means one thing: the boot
    /// window has not settled, so nothing has been able to reach the engine.
    /// `Some(Ok(()))` is a live boot window the chassis made reachable, and
    /// `Some(Err(reason))` is the boot window's failure or the chassis's own
    /// from [`DesktopWindowIntegration::boot_window_live`], for which the
    /// application has already requested shutdown.
    #[must_use]
    pub fn boot_outcome(&self) -> Option<Result<(), &str>> {
        match &self.boot {
            BootState::Pending => None,
            BootState::Reachable => Some(Ok(())),
            BootState::Failed(reason) => Some(Err(reason)),
        }
    }

    fn apply_work(
        &mut self,
        event_loop: &ActiveEventLoop,
        actions: Vec<WindowHostAction>,
        effects: Vec<WindowHostEffect>,
    ) -> (BTreeSet<ErasedActorPath>, WorkOutcome) {
        let mut dirty = BTreeSet::new();
        let mut outcome = WorkOutcome::default();
        self.apply_effects(effects, &mut dirty, &mut outcome);

        for action in actions {
            match action {
                WindowHostAction::Create { ref path, .. } => match action.realize(event_loop) {
                    Ok(Some(window)) => {
                        let staged =
                            self.window_slot.host_turn(|state, _ctx| state.stage_created_window(path.clone(), window));
                        match staged {
                            Some(Ok(created)) => {
                                self.apply_effects(vec![created], &mut dirty, &mut outcome);
                            }
                            Some(Err(error)) => {
                                let effects = self
                                    .window_slot
                                    .host_turn(|state, ctx| state.fail_window_creation(ctx, path, error))
                                    .unwrap_or_default();
                                self.apply_effects(effects, &mut dirty, &mut outcome);
                            }
                            None => {}
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let effects = self
                            .window_slot
                            .host_turn(|state, ctx| state.fail_window_creation(ctx, path, error))
                            .unwrap_or_default();
                        self.apply_effects(effects, &mut dirty, &mut outcome);
                    }
                },
                WindowHostAction::Close { path } => {
                    outcome.record(apply_simple_effect(
                        &mut self.integration,
                        WindowHostEffect::Closing { path: path.clone() },
                        &mut dirty,
                    ));
                    let effects = self
                        .window_slot
                        .host_turn(|state, ctx| state.finish_window_close(&path, ctx))
                        .unwrap_or_default();
                    self.apply_effects(effects, &mut dirty, &mut outcome);
                }
                WindowHostAction::SetPresentation { path, presentation } => {
                    let outcome = self.integration.set_presentation(&path, presentation);
                    let _ = self
                        .window_slot
                        .host_turn(|state, ctx| state.finish_window_presentation(&path, presentation, outcome, ctx));
                }
            }
        }

        (dirty, outcome)
    }

    fn apply_effects(
        &mut self,
        effects: Vec<WindowHostEffect>,
        dirty: &mut BTreeSet<ErasedActorPath>,
        outcome: &mut WorkOutcome,
    ) {
        let mut effects = VecDeque::from(effects);
        while let Some(effect) = effects.pop_front() {
            match effect {
                WindowHostEffect::Created { path, window, presentation } => {
                    let attachment = self.integration.attach_window(path.clone(), Arc::clone(&window), presentation);
                    let follow_up = self
                        .window_slot
                        .host_turn(|state, ctx| state.finish_window_attachment(&path, attachment, ctx))
                        .unwrap_or_default();
                    effects.extend(follow_up);
                }
                simple => outcome.record(apply_simple_effect(&mut self.integration, simple, dirty)),
            }
        }
    }

    fn drain_and_take_work(
        &mut self,
        host_turn: impl FnOnce(
            &mut DesktopWindows,
            &mut aether_substrate::NativeCtx<'_, WindowCapability, aether_actor::Anyone, aether_actor::Single>,
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
            &mut DesktopWindows,
            &mut aether_substrate::NativeCtx<'_, WindowCapability, aether_actor::Anyone, aether_actor::Single>,
        ),
    ) {
        self.integration.drain_available();
        // muda delivers menu clicks on its own process-wide channel rather
        // than through winit's event loop, so every turn drains it here — one
        // non-blocking `try_iter` — and folds the activations into the same
        // host turn the callback's own event rides, ahead of it so a click and
        // the key event that may accompany it stay in arrival order.
        let activations = menu::drain_menu_activations();
        let (actions, effects) = self.drain_and_take_work(|state, ctx| {
            for raw in &activations {
                state.menu_activated(raw, ctx);
            }
            host_turn(state, ctx);
        });
        let (dirty, outcome) = self.apply_work(event_loop, actions, effects);
        self.pending_dirty.extend(dirty);

        let boot_failed = matches!(outcome.boot_settled, Some(Err(_)));
        let shuts_down = request_shutdown || outcome.last_window_closed || boot_failed;
        if let Some(settled) = outcome.boot_settled {
            self.boot.settle(settled);
        }
        if shuts_down {
            request_shutdown_once(&mut self.integration, &mut self.shutdown_requested);
        }

        let snapshot = self.window_slot.host_turn(|state, _ctx| state.application_snapshot()).unwrap_or_default();
        let visible = snapshot.visible_presentations();
        if flush_frame {
            let now = Instant::now();
            let capture_expired = self.integration.capture_deadline().is_some_and(|deadline| deadline <= now);
            let dirty = mem::take(&mut self.pending_dirty);
            let force = self.shutdown_requested || capture_expired;
            let frame_windows = snapshot.frame_windows(&dirty, &self.pacing.schedule(&visible, now).due, force);
            if !frame_windows.is_empty() || force {
                self.integration.windows_dirty(&frame_windows);
                self.pacing.frame_drawn(&snapshot.live, &frame_windows, now);
            }
        }

        // Read again after the frame: a display-paced present has just
        // waited, so a capped window may have come due meanwhile.
        let now = Instant::now();
        let schedule = self.pacing.schedule(&visible, now);
        let disposition = loop_disposition(
            self.integration.should_exit(),
            self.shutdown_requested,
            &schedule,
            self.integration.capture_deadline(),
            now,
        );
        match disposition {
            LoopDisposition::Exit => event_loop.exit(),
            LoopDisposition::Poll => event_loop.set_control_flow(ControlFlow::Poll),
            LoopDisposition::Wait => event_loop.set_control_flow(ControlFlow::Wait),
            LoopDisposition::WaitUntil(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
        }

        if disposition != LoopDisposition::Exit {
            for shown in snapshot.visible {
                if schedule.due.contains(&shown.path) {
                    shown.window.request_redraw();
                }
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

    fn window_event(&mut self, event_loop: &ActiveEventLoop, winit_id: WinitWindowId, event: WindowEvent) {
        self.turn(event_loop, false, false, |state, ctx| state.window_event(winit_id, event, ctx));
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.turn(event_loop, false, true, |_, _| {});
    }
}

/// One live, unoccluded window: the handle the loop asks to redraw and the
/// presentation that paces it.
struct VisibleWindow {
    path: ErasedActorPath,
    window: Arc<Window>,
    presentation: WindowPresentation,
}

#[derive(Default)]
struct WindowSnapshot {
    live: Vec<(ErasedActorPath, WindowPresentation)>,
    visible: Vec<VisibleWindow>,
}

impl WindowSnapshot {
    /// The visible windows as the pacing schedule reads them.
    fn visible_presentations(&self) -> Vec<(ErasedActorPath, WindowPresentation)> {
        self.visible.iter().map(|visible| (visible.path.clone(), visible.presentation)).collect()
    }

    /// The windows one frame draws: every visible window that asked for a
    /// redraw and is `due` one, or every live window when the frame is
    /// forced.
    fn frame_windows(
        &self,
        dirty: &BTreeSet<ErasedActorPath>,
        due: &BTreeSet<ErasedActorPath>,
        force: bool,
    ) -> Vec<ErasedActorPath> {
        if force {
            return self.live.iter().map(|(path, _)| path.clone()).collect();
        }
        self.visible
            .iter()
            .map(|visible| &visible.path)
            .filter(|path| dirty.contains(path))
            .filter(|path| due.contains(path))
            .cloned()
            .collect()
    }
}

impl DesktopWindows {
    fn application_snapshot(&self) -> WindowSnapshot {
        let mut snapshot = WindowSnapshot::default();
        for (path, state) in &self.windows {
            if state.lifecycle != DesktopWindowLifecycle::Live {
                continue;
            }
            snapshot.live.push((path.clone(), state.presentation));
            if !state.occluded
                && let Some(window) = self.native_windows.get(path)
            {
                snapshot.visible.push(VisibleWindow {
                    path: path.clone(),
                    window: Arc::clone(window),
                    presentation: state.presentation,
                });
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

/// How the event loop waits for its next turn. It polls while a frame is
/// due: any visible display-paced or uncapped window, or a capped one whose
/// instant has passed. Otherwise it sleeps until the earlier of the next
/// capped window's due instant and a pending capture's deadline, or until an
/// event when there is neither.
fn loop_disposition(
    should_exit: bool,
    shutdown_requested: bool,
    schedule: &FrameSchedule,
    capture_deadline: Option<Instant>,
    now: Instant,
) -> LoopDisposition {
    let frame_due = !schedule.due.is_empty();
    let capture_pending = capture_deadline.filter(|deadline| *deadline > now);
    let wake = [schedule.wake, capture_pending].into_iter().flatten().min();

    if should_exit {
        LoopDisposition::Exit
    } else if shutdown_requested || frame_due {
        LoopDisposition::Poll
    } else if let Some(wake) = wake {
        LoopDisposition::WaitUntil(wake)
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

/// What the application does next about one applied effect.
#[derive(Debug, PartialEq, Eq)]
enum Applied {
    Nothing,
    LastWindowClosed,
    /// The boot window settled: the engine is reachable, or never will be
    /// for the reason carried.
    BootSettled(Result<(), String>),
}

/// What one turn's host work asks of the application.
#[derive(Default)]
struct WorkOutcome {
    last_window_closed: bool,
    /// The boot window's outcome, in the one turn that settles it: the
    /// first the turn's effects reported.
    boot_settled: Option<Result<(), String>>,
}

impl WorkOutcome {
    fn record(&mut self, applied: Applied) {
        match applied {
            Applied::Nothing => {}
            Applied::LastWindowClosed => self.last_window_closed = true,
            Applied::BootSettled(outcome) => {
                self.boot_settled.get_or_insert(outcome);
            }
        }
    }
}

fn apply_simple_effect<I: DesktopWindowIntegration>(
    integration: &mut I,
    effect: WindowHostEffect,
    dirty: &mut BTreeSet<ErasedActorPath>,
) -> Applied {
    match effect {
        WindowHostEffect::Created { .. } => unreachable!("created effects require actor completion"),
        WindowHostEffect::Closing { path } => integration.detach_window(&path),
        WindowHostEffect::Dirty { path } => {
            dirty.insert(path);
        }
        WindowHostEffect::Occluded { path, occluded } => integration.window_occluded(&path, occluded),
        WindowHostEffect::LastWindowClosed => return Applied::LastWindowClosed,
        // A live boot window is the chassis's cue to make the engine
        // reachable; a failed one is never offered to it.
        WindowHostEffect::BootWindowSettled { outcome } => {
            return Applied::BootSettled(outcome.and_then(|()| integration.boot_window_live()));
        }
    }
    Applied::Nothing
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aether_data::LoadName;

    use super::super::tests::insert_window;
    use super::*;
    use crate::runtime::subscribers::fixture::Rig;
    use crate::{
        CreateWindow, ListWindows, ListWindowsResult, SetWindowPresentation, SetWindowPresentationResult,
        WindowInstance,
    };

    fn window(name: &str) -> ErasedActorPath {
        crate::window_path(&LoadName::new(name).expect("fixture window name"))
    }

    /// The window name a spy records: the key after the path's last `:`.
    fn name(path: &ErasedActorPath) -> &str {
        path.as_str().rsplit_once(':').map_or(path.as_str(), |(_, name)| name)
    }

    #[derive(Default)]
    struct SpyIntegration {
        calls: Vec<String>,
        /// The error every `set_presentation` is refused with, when set.
        refuses_presentation: Option<String>,
        /// The error `boot_window_live` answers, when set.
        refuses_boot: Option<String>,
    }

    impl DesktopWindowIntegration for SpyIntegration {
        fn attach_window(
            &mut self,
            path: ErasedActorPath,
            _window: Arc<Window>,
            _presentation: WindowPresentation,
        ) -> Result<(), String> {
            self.calls.push(format!("attach:{}", name(&path)));
            Ok(())
        }

        fn set_presentation(&mut self, path: &ErasedActorPath, presentation: WindowPresentation) -> Result<(), String> {
            self.calls.push(format!("present:{}:{presentation:?}", name(path)));
            self.refuses_presentation.clone().map_or(Ok(()), Err)
        }

        fn detach_window(&mut self, path: &ErasedActorPath) {
            self.calls.push(format!("detach:{}", name(path)));
        }

        fn windows_dirty(&mut self, windows: &[ErasedActorPath]) {
            self.calls.push(format!("dirty:{}", windows.iter().map(name).collect::<Vec<_>>().join(",")));
        }

        fn window_occluded(&mut self, path: &ErasedActorPath, occluded: bool) {
            self.calls.push(format!("occluded:{}:{occluded}", name(path)));
        }

        fn request_shutdown(&mut self) {
            self.calls.push("shutdown".to_owned());
        }

        fn boot_window_live(&mut self) -> Result<(), String> {
            self.calls.push("boot-live".to_owned());
            self.refuses_boot.clone().map_or(Ok(()), Err)
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

    /// A presentation the surface refuses is answered `Err` with the
    /// surface's own message, and the window still lists the presentation it
    /// had. The window's child is born through the manager's own staged
    /// birth, so the request travels the forward a live window's does. Fails
    /// if the manager records the asked value before the integration has
    /// answered, or answers `Ok` whatever the surface said.
    #[test]
    fn a_refused_presentation_answers_err_and_the_window_keeps_its_own() {
        let mut rig = Rig::desktop();
        let game = window("game");
        let spec = WindowSpec {
            name: "game".to_owned(),
            title: "Game".to_owned(),
            mode: WindowMode::Windowed,
            size: None,
            presentation: WindowPresentation::Display,
        };
        rig.push(&CreateWindow { spec });
        rig.pump_desktop_until("the create's reservation", |state| state.pending_creates.contains_key(&game));
        rig.desktop_turn(|state, ctx| {
            let _ = state.take_host_work();
            let staged = insert_window(state, "game", false);
            state.windows.get_mut(&staged).expect("the staged window").lifecycle = DesktopWindowLifecycle::Attaching;
            state.finish_window_attachment(&staged, Ok(()), ctx)
        })
        .expect("the desktop manager is live");
        rig.pump_desktop_until("the window child's birth", |state| state.children.contains_key(&game));
        let child = rig
            .chassis()
            .child::<WindowCapability, WindowInstance>(rig.manager(), LoadName::new("game").expect("fixture name"))
            .expect("the window child is live");

        let request = rig.push_to(child, &SetWindowPresentation { presentation: WindowPresentation::Uncapped });
        rig.pump_desktop_until("the queued presentation change", |state| !state.presentation_helds.is_empty());
        let mut integration =
            SpyIntegration { refuses_presentation: Some("the surface offers [Fifo]".to_owned()), ..Default::default() };
        rig.desktop_turn(|state, ctx| {
            let (actions, _) = state.take_host_work();
            for action in actions {
                let WindowHostAction::SetPresentation { path, presentation } = action else {
                    panic!("the only queued host action is the presentation change, got {action:?}");
                };
                let outcome = integration.set_presentation(&path, presentation);
                state.finish_window_presentation(&path, presentation, outcome, ctx);
            }
        })
        .expect("the desktop manager is live");
        rig.driver.settle(&[request]);

        assert_eq!(integration.calls, ["present:game:Uncapped"], "the surface was asked once");
        let SetWindowPresentationResult::Err { error } = rig.reply() else {
            panic!("a presentation the surface refuses is answered Err");
        };
        assert!(error.contains("the surface offers [Fifo]"), "the reply carries the surface's message: {error}");
        rig.send(&ListWindows);
        let ListWindowsResult::Ok { windows } = rig.reply() else {
            panic!("desktop manager list succeeds");
        };
        assert_eq!(
            windows.into_iter().map(|listed| listed.presentation).collect::<Vec<_>>(),
            [WindowPresentation::Display],
            "the refused value is not what the window reports",
        );
    }

    #[test]
    fn detach_happens_before_last_window_shutdown() {
        let mut integration = SpyIntegration::default();
        let mut dirty = BTreeSet::new();

        apply_simple_effect(&mut integration, WindowHostEffect::Closing { path: window("d") }, &mut dirty);
        let applied = apply_simple_effect(&mut integration, WindowHostEffect::LastWindowClosed, &mut dirty);
        if applied == Applied::LastWindowClosed {
            let mut shutdown_requested = false;
            request_shutdown_once(&mut integration, &mut shutdown_requested);
        }

        assert_eq!(integration.calls, ["detach:d", "shutdown"]);
    }

    /// A live boot window asks the chassis once to make the engine reachable.
    /// Fails if the settled window never reaches the chassis, which leaves
    /// the RPC port unbound and the engine running unreachable.
    #[test]
    fn a_live_boot_window_asks_the_chassis_to_make_the_engine_reachable() {
        let mut integration = SpyIntegration::default();
        let mut dirty = BTreeSet::new();

        let applied =
            apply_simple_effect(&mut integration, WindowHostEffect::BootWindowSettled { outcome: Ok(()) }, &mut dirty);

        assert_eq!(applied, Applied::BootSettled(Ok(())));
        assert_eq!(integration.calls, ["boot-live"]);
    }

    /// Fails if a chassis that cannot bind its port has the error swallowed:
    /// the engine would keep running with a window and no way to reach it.
    #[test]
    fn a_chassis_that_cannot_make_the_engine_reachable_fails_the_boot() {
        let mut integration =
            SpyIntegration { refuses_boot: Some("port 8901 is taken".to_owned()), ..Default::default() };
        let mut dirty = BTreeSet::new();

        let applied =
            apply_simple_effect(&mut integration, WindowHostEffect::BootWindowSettled { outcome: Ok(()) }, &mut dirty);

        assert_eq!(applied, Applied::BootSettled(Err("port 8901 is taken".to_owned())));
        assert_eq!(integration.calls, ["boot-live"]);
    }

    /// Fails if a boot window that failed still has the chassis bind its
    /// port, reporting ready an engine whose main window cannot be mailed, or
    /// if its reason is dropped on the way to the run's error.
    #[test]
    fn a_failed_boot_window_fails_the_boot_without_asking_the_chassis() {
        let mut integration = SpyIntegration::default();
        let mut dirty = BTreeSet::new();
        let failed = WindowHostEffect::BootWindowSettled { outcome: Err("render attach failed".to_owned()) };

        let applied = apply_simple_effect(&mut integration, failed, &mut dirty);

        assert_eq!(applied, Applied::BootSettled(Err("render attach failed".to_owned())));
        assert!(integration.calls.is_empty(), "the chassis was asked about a failed window: {:?}", integration.calls);
    }

    /// A turn keeps the first boot failure its effects report. Fails if a
    /// later effect's reason replaces the one that names the cause, or if a
    /// last-window shutdown in the same turn drops it.
    #[test]
    fn a_turn_keeps_its_first_boot_failure_beside_a_last_window_shutdown() {
        let mut outcome = WorkOutcome::default();

        outcome.record(Applied::BootSettled(Err("first".to_owned())));
        outcome.record(Applied::LastWindowClosed);
        outcome.record(Applied::BootSettled(Err("second".to_owned())));

        assert!(outcome.last_window_closed);
        assert_eq!(outcome.boot_settled, Some(Err("first".to_owned())));
    }

    /// The boot state leaves `Pending` once. Fails if a later outcome turns
    /// a failed boot reachable, which would have the run report success for
    /// an engine nothing could reach, or replaces the reason that names the
    /// cause.
    #[test]
    fn the_boot_state_keeps_its_first_settled_outcome() {
        let mut failed = BootState::Pending;
        failed.settle(Err("first".to_owned()));
        failed.settle(Ok(()));
        failed.settle(Err("second".to_owned()));
        assert!(matches!(&failed, BootState::Failed(reason) if reason == "first"));

        let mut reachable = BootState::Pending;
        reachable.settle(Ok(()));
        reachable.settle(Err("late".to_owned()));
        assert!(matches!(reachable, BootState::Reachable));
    }

    #[test]
    fn dirty_windows_coalesce_in_path_order() {
        let mut integration = SpyIntegration::default();
        let mut dirty = BTreeSet::new();
        for name in ["h", "b", "h"] {
            apply_simple_effect(&mut integration, WindowHostEffect::Dirty { path: window(name) }, &mut dirty);
        }

        integration.windows_dirty(&dirty.into_iter().collect::<Vec<_>>());

        assert_eq!(integration.calls, ["dirty:b,h"]);
    }

    #[test]
    fn occlusion_is_semantic_not_a_raw_winit_event() {
        let mut integration = SpyIntegration::default();
        let mut dirty = BTreeSet::new();

        apply_simple_effect(
            &mut integration,
            WindowHostEffect::Occluded { path: window("c"), occluded: true },
            &mut dirty,
        );

        assert_eq!(integration.calls, ["occluded:c:true"]);
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

        let idle = FrameSchedule::default();

        assert_eq!(loop_disposition(true, true, &idle, None, now), LoopDisposition::Exit);
        assert_eq!(loop_disposition(false, true, &idle, None, now), LoopDisposition::Poll);
        assert_eq!(loop_disposition(false, false, &idle, None, now), LoopDisposition::Wait);
        assert_eq!(
            loop_disposition(false, false, &idle, Some(now + Duration::from_secs(1)), now),
            LoopDisposition::WaitUntil(now + Duration::from_secs(1)),
        );
    }

    /// A loop whose only visible window is capped and waiting sleeps until
    /// that window's instant, or a capture deadline that comes sooner, and an
    /// uncapped window beside it keeps the loop polling. Fails if a capped
    /// window alone is polled at full speed, or if its wait parks a window
    /// that is not capped.
    #[test]
    fn a_capped_only_loop_waits_until_the_next_due_instant() {
        let start = Instant::now();
        let capped = WindowPresentation::Capped {
            frames_per_second: crate::FrameRate::new(10).expect("a rate inside the range"),
        };
        let due = start + Duration::from_millis(100);
        let now = start + Duration::from_millis(1);
        let mut pacing = FramePacing::default();
        pacing.frame_drawn(&[(window("game"), capped)], &[window("game")], start);

        let capped_only = pacing.schedule(&[(window("game"), capped)], now);
        assert_eq!(loop_disposition(false, false, &capped_only, None, now), LoopDisposition::WaitUntil(due));
        let capture = start + Duration::from_millis(40);
        assert_eq!(
            loop_disposition(false, false, &capped_only, Some(capture), now),
            LoopDisposition::WaitUntil(capture),
            "a capture deadline before the due instant wakes the loop first",
        );

        let with_uncapped =
            pacing.schedule(&[(window("game"), capped), (window("tools"), WindowPresentation::Uncapped)], now);
        assert_eq!(loop_disposition(false, false, &with_uncapped, None, now), LoopDisposition::Poll);
    }
}
