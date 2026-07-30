//! Desktop `aether.window` manager and native winit integration.
//!
//! The chassis owns the application thread and pumps this actor on that
//! thread. The actor owns every engine/native window identity, window-local
//! state, native-event translation, and selector-aware subscription. Work
//! requiring winit's callback-scoped [`ActiveEventLoop`](winit::event_loop::ActiveEventLoop)
//! crosses the boundary as a [`WindowHostAction`] and returns through a second
//! host turn as a [`WindowHostEffect`].

mod application;
mod input;
mod instance;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use aether_actor::{Addressable, Manual, runtime};
use aether_data::{Kind, MailboxId};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MonitorNotice, MouseButton, MouseButtonRelease, MouseMove, MouseWheel,
    TextInput, WindowMode, WindowSize,
};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SpawnApplied, SpawnError, TaskDone};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{InboundMail, MonitorHandle as ActorMonitorHandle, Subname};
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Ime, WindowEvent};
use winit::keyboard::PhysicalKey;
use winit::monitor::{MonitorHandle as WinitMonitorHandle, VideoModeHandle};
use winit::window::{Fullscreen, Window, WindowId as WinitWindowId};

use self::input::{TextSource, ime_cursor_span, map_mouse_button, map_winit_keycode, normalize_wheel, text_input_gate};
use super::subscribers::{WindowSubscribers, validate_subscriber_mailbox};
use crate::{
    ApplyWindowCommand, ApplyWindowCommandResult, CloseWindowResult, CreateWindow, CreateWindowResult,
    DesktopWindowCapability, DesktopWindowInstance, FocusWindowResult, ListWindows, ListWindowsResult,
    RequestWindowRedrawResult, RetireWindow, SetWindowModeResult, SetWindowTitleResult, SubscribeWindow,
    SubscribeWindowResult, SubscribeWindowSelf, UnsubscribeAllWindows, UnsubscribeWindow, UnsubscribeWindowSelf,
    WindowCapability, WindowClosed, WindowCommand, WindowId, WindowInfo, WindowInstance, WindowOpened, WindowSpec,
};

pub use application::{DesktopWindowApplication, DesktopWindowIntegration, DesktopWindowUserEvent};

/// Construction input for the application-scoped desktop window manager.
///
/// The manager needs no native handle or initial identity at boot. Its
/// registry comes from [`NativeInitCtx`], and the application host reserves
/// the boot window before winit begins dispatching callbacks.
#[derive(Default)]
pub struct DesktopWindowParams;

/// Host-only work that must be realized while a winit callback supplies an
/// `ActiveEventLoop`.
#[derive(Clone, Debug)]
pub enum WindowHostAction {
    Create { id: WindowId, spec: WindowSpec },
    Close { id: WindowId },
}

/// Owned semantic changes produced by a window host turn.
#[derive(Clone, Debug)]
pub enum WindowHostEffect {
    Created { id: WindowId, window: Arc<Window> },
    Closing { id: WindowId },
    Dirty { id: WindowId },
    Occluded { id: WindowId, occluded: bool },
    LastWindowClosed,
}

struct PendingCreate {
    spec: WindowSpec,
    reply: Option<Box<InboundMail>>,
    shutdown_on_failure: bool,
    /// The reserved child, once [`DesktopWindowCapabilityState::finish_window_attachment`]
    /// has staged its birth. `None` while the create is still waiting on the
    /// native window and render attachment.
    staged: Option<StagedWindowChild>,
}

/// A window child whose deterministic id is reserved but whose birth the
/// registry owner has not applied yet. The window it belongs to stays
/// `Attaching` — absent from `ListWindows`, the frame set, and every
/// publication — until the authoritative `SpawnApplied` lands.
struct StagedWindowChild {
    child: MailboxId,
    /// A reservation that is already doomed: the deterministic child id
    /// disagreed with the addressable prediction. The completion retires the
    /// applied child and rolls the create back with this error.
    rejection: Option<String>,
}

/// Completion context carried from staging to authoritative application. The
/// reserved [`WindowId`] is the pending-create key.
#[derive(Clone, Copy)]
struct WindowChildContinuation {
    id: WindowId,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DesktopWindowLifecycle {
    Attaching,
    Live,
    Closing,
}

struct DesktopWindowState {
    name: String,
    title: String,
    mode: WindowMode,
    width: u32,
    height: u32,
    cursor: (f32, f32),
    composing: bool,
    modifiers: Modifiers,
    focused: bool,
    occluded: bool,
    lifecycle: DesktopWindowLifecycle,
    close_reply: Option<Box<InboundMail>>,
}

impl DesktopWindowState {
    fn info(&self, id: WindowId) -> WindowInfo {
        WindowInfo {
            id,
            name: self.name.clone(),
            title: self.title.clone(),
            mode: self.mode.clone(),
            width: self.width,
            height: self.height,
            focused: self.focused,
            occluded: self.occluded,
        }
    }
}

/// Application-scoped `aether.window` state.
///
/// Engine identities are the mailbox ids of supervised named children. The
/// `BTreeMap` makes `ListWindows` naturally ordered; the hash maps provide
/// constant-time native lookup without exposing winit identities on the wire.
pub struct DesktopWindowCapabilityState {
    windows: BTreeMap<WindowId, DesktopWindowState>,
    native_windows: HashMap<WindowId, Arc<Window>>,
    winit_windows: HashMap<WinitWindowId, WindowId>,
    child_monitors: HashMap<WindowId, ActorMonitorHandle>,
    subscribers: WindowSubscribers,
    pending_creates: HashMap<WindowId, PendingCreate>,
    pending_host_actions: VecDeque<WindowHostAction>,
    pending_host_effects: Vec<WindowHostEffect>,
    initial_window_reserved: bool,
    shutdown_when_idle: bool,
}

impl DesktopWindowCapabilityState {
    /// Reserve the boot window exactly once. Creation happens when the caller
    /// realizes the returned host action.
    pub fn queue_initial_window(&mut self, spec: WindowSpec) -> Result<(), String> {
        if self.initial_window_reserved {
            return Ok(());
        }
        self.queue_create(spec, None, true).map_err(|(error, _)| error)?;
        self.initial_window_reserved = true;
        Ok(())
    }

    /// Consume work accumulated by mail handlers and the current native
    /// callback. Callers must leave the actor borrow before realizing it.
    pub fn take_host_work(&mut self) -> (Vec<WindowHostAction>, Vec<WindowHostEffect>) {
        (self.pending_host_actions.drain(..).collect(), self.pending_host_effects.drain(..).collect())
    }

    /// Stage a successfully-created native window before render attachment.
    ///
    /// The window is addressable internally but remains absent from
    /// `ListWindows` until [`Self::finish_window_attachment`] succeeds.
    pub fn stage_created_window(&mut self, id: WindowId, window: Arc<Window>) -> Result<WindowHostEffect, String> {
        let pending =
            self.pending_creates.get(&id).ok_or_else(|| format!("window {id:?} has no pending create action"))?;
        if self.winit_windows.contains_key(&window.id()) {
            return Err(format!("native window {:?} is already registered", window.id()));
        }
        let size = window.inner_size();
        self.windows.insert(
            id,
            DesktopWindowState {
                name: pending.spec.name.clone(),
                title: pending.spec.title.clone(),
                mode: pending.spec.mode.clone(),
                width: size.width,
                height: size.height,
                cursor: (0.0, 0.0),
                composing: false,
                modifiers: Modifiers { window: id, ..Modifiers::default() },
                focused: window.has_focus(),
                occluded: size.width == 0 || size.height == 0,
                lifecycle: DesktopWindowLifecycle::Attaching,
                close_reply: None,
            },
        );
        self.winit_windows.insert(window.id(), id);
        self.native_windows.insert(id, Arc::clone(&window));
        Ok(WindowHostEffect::Created { id, window })
    }

    /// Complete render attachment for a staged create by staging the window's
    /// child birth. The window stays `Attaching` and the create stays pending
    /// until [`Self::finish_window_child_spawn`] observes the authoritative
    /// `SpawnApplied`; a render failure still rolls back and replies here.
    pub fn finish_window_attachment(
        &mut self,
        id: WindowId,
        attachment: Result<(), String>,
        ctx: &mut NativeCtx<'_>,
    ) -> Vec<WindowHostEffect> {
        let Some(mut pending) = self.pending_creates.remove(&id) else {
            return Vec::new();
        };
        match attachment {
            Ok(()) => {
                let predicted = MailboxId(id.0);
                let receipt = match ctx
                    .spawn_child::<WindowCapability, DesktopWindowInstance>(Subname::Named(&pending.spec.name), (), ())
                    .stage_with(WindowChildContinuation { id })
                {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        return self.rollback_attached_create(
                            id,
                            &mut pending,
                            format!("failed to spawn window child: {error:?}"),
                        );
                    }
                };
                // The reservation is keyed by the prediction consumers address,
                // so a divergent deterministic id dooms it rather than
                // publishing a window nobody can reach.
                let rejection = (receipt.mailbox_id != predicted).then(|| {
                    format!(
                        "spawned window child {:?} did not match predicted mailbox {predicted:?}",
                        receipt.mailbox_id
                    )
                });
                pending.staged = Some(StagedWindowChild { child: receipt.mailbox_id, rejection });
                self.pending_creates.insert(id, pending);
                Vec::new()
            }
            Err(error) => {
                self.remove_window(id);
                if let Some(reply) = pending.reply.take() {
                    reply.reply(&CreateWindowResult::Err { error });
                }
                self.failed_create_effects(pending.shutdown_on_failure)
            }
        }
    }

    /// Apply the authoritative result of a staged window child. Success
    /// installs the monitor, promotes the window to `Live`, replies, and
    /// publishes; every failure retires the applied child and rolls the create
    /// back. Rollback effects go to the host-effect queue because this runs on
    /// an ordinary mail turn rather than inside a native callback.
    fn finish_window_child_spawn(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        id: WindowId,
        applied: &Result<SpawnApplied, SpawnError>,
    ) {
        let Some(mut pending) = self.pending_creates.remove(&id) else {
            if let Ok(applied) = applied {
                ctx.actor_at::<DesktopWindowInstance>(applied.mailbox_id).send(&RetireWindow);
            }
            return;
        };
        let rejection = pending.staged.as_mut().and_then(|staged| staged.rejection.take());
        let effects = match applied {
            Err(error) => {
                self.rollback_attached_create(id, &mut pending, format!("failed to spawn window child: {error:?}"))
            }
            Ok(applied) => {
                let child = applied.mailbox_id;
                if let Some(error) = rejection {
                    ctx.actor_at::<DesktopWindowInstance>(child).send(&RetireWindow);
                    self.rollback_attached_create(id, &mut pending, error)
                } else {
                    match ctx.monitor(child) {
                        Ok(monitor) => self.promote_attached_window(ctx, id, child, monitor, &mut pending),
                        Err(error) => {
                            ctx.actor_at::<DesktopWindowInstance>(child).send(&RetireWindow);
                            self.rollback_attached_create(
                                id,
                                &mut pending,
                                format!("failed to monitor window child: {error:?}"),
                            )
                        }
                    }
                }
            }
        };
        self.pending_host_effects.extend(effects);
    }

    fn promote_attached_window(
        &mut self,
        ctx: &mut NativeCtx<'_>,
        id: WindowId,
        child: MailboxId,
        monitor: ActorMonitorHandle,
        pending: &mut PendingCreate,
    ) -> Vec<WindowHostEffect> {
        let Some(state) = self.windows.get_mut(&id) else {
            let error = format!("window {id:?} disappeared during attachment");
            ctx.actor_at::<DesktopWindowInstance>(child).send(&RetireWindow);
            return self.rollback_attached_create(id, pending, error);
        };
        self.child_monitors.insert(id, monitor);
        state.lifecycle = DesktopWindowLifecycle::Live;
        self.shutdown_when_idle = false;
        let info = state.info(id);
        if let Some(reply) = pending.reply.take() {
            reply.reply(&CreateWindowResult::Ok { window: info.clone() });
        }
        self.publish(ctx, id, &WindowOpened { window: info.clone() });
        if info.width != 0 && info.height != 0 {
            self.publish(ctx, id, &WindowSize { window: id, width: info.width, height: info.height });
        }
        Vec::new()
    }

    fn rollback_attached_create(
        &mut self,
        id: WindowId,
        pending: &mut PendingCreate,
        error: String,
    ) -> Vec<WindowHostEffect> {
        self.remove_window(id);
        if let Some(reply) = pending.reply.take() {
            reply.reply(&CreateWindowResult::Err { error });
        }
        let mut effects = vec![WindowHostEffect::Closing { id }];
        effects.extend(self.failed_create_effects(pending.shutdown_on_failure));
        effects
    }

    /// Fail a queued create before a native window could be staged.
    pub fn fail_window_creation(&mut self, id: WindowId, error: String) -> Vec<WindowHostEffect> {
        let Some(mut pending) = self.pending_creates.remove(&id) else {
            return Vec::new();
        };
        if let Some(reply) = pending.reply.take() {
            reply.reply(&CreateWindowResult::Err { error });
        }
        self.failed_create_effects(pending.shutdown_on_failure)
    }

    /// Finish a close after the integration detached native resources.
    pub fn finish_window_close(&mut self, id: WindowId, ctx: &mut NativeCtx<'_>) -> Vec<WindowHostEffect> {
        let close_reply = self.windows.get_mut(&id).and_then(|state| state.close_reply.take());
        let existed = self.remove_window(id);
        if !existed {
            if let Some(reply) = close_reply {
                reply.reply(&ApplyWindowCommandResult::Close(CloseWindowResult::Err {
                    error: format!("unknown window {id:?}"),
                }));
            }
            return Vec::new();
        }
        if let Some(reply) = close_reply {
            reply.reply(&ApplyWindowCommandResult::Close(CloseWindowResult::Ok));
        } else {
            ctx.actor_at::<DesktopWindowInstance>(MailboxId(id.0)).send(&RetireWindow);
        }
        self.publish(ctx, id, &WindowClosed { window: id });
        if self.windows.values().any(|window| window.lifecycle != DesktopWindowLifecycle::Attaching) {
            return Vec::new();
        }
        if self.pending_creates.is_empty() {
            self.shutdown_when_idle = false;
            return vec![WindowHostEffect::LastWindowClosed];
        }
        self.shutdown_when_idle = true;
        Vec::new()
    }

    /// Translate one native window event and publish typed input directly to
    /// selector-aware subscribers.
    #[allow(clippy::too_many_lines)]
    pub fn window_event(&mut self, winit_id: WinitWindowId, event: WindowEvent, ctx: &mut NativeCtx<'_>) {
        let Some(id) = self.winit_windows.get(&winit_id).copied() else {
            return;
        };
        if self.windows.get(&id).is_none_or(|state| state.lifecycle != DesktopWindowLifecycle::Live) {
            return;
        }

        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                let _ = self.queue_close(id, None);
            }
            WindowEvent::Resized(size) => {
                let mut occlusion = None;
                if let Some(state) = self.windows.get_mut(&id) {
                    state.width = size.width;
                    state.height = size.height;
                    let next = size.width == 0 || size.height == 0;
                    if state.occluded != next {
                        state.occluded = next;
                        occlusion = Some(next);
                    }
                }
                if let Some(occluded) = occlusion {
                    self.pending_host_effects.push(WindowHostEffect::Occluded { id, occluded });
                }
                if size.width != 0 && size.height != 0 {
                    self.publish(ctx, id, &WindowSize { window: id, width: size.width, height: size.height });
                    if let Some(window) = self.native_windows.get(&id) {
                        window.request_redraw();
                    }
                }
            }
            WindowEvent::Occluded(occluded) => {
                if let Some(state) = self.windows.get_mut(&id)
                    && state.occluded != occluded
                {
                    state.occluded = occluded;
                    self.pending_host_effects.push(WindowHostEffect::Occluded { id, occluded });
                }
            }
            WindowEvent::Focused(focused) => {
                if let Some(state) = self.windows.get_mut(&id) {
                    state.focused = focused;
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(window) = self.native_windows.get(&id) {
                    let size = window.inner_size();
                    if let Some(state) = self.windows.get_mut(&id) {
                        state.width = size.width;
                        state.height = size.height;
                    }
                    if size.width != 0 && size.height != 0 {
                        self.publish(ctx, id, &WindowSize { window: id, width: size.width, height: size.height });
                    }
                }
                self.pending_host_effects.push(WindowHostEffect::Dirty { id });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let committed = if event.state == ElementState::Pressed {
                    event.text.as_ref().and_then(|text| {
                        self.windows.get_mut(&id).and_then(|state| {
                            text_input_gate(&mut state.composing, TextSource::KeyText(text.to_string()))
                        })
                    })
                } else {
                    None
                };
                if let Some(text) = committed {
                    self.publish(ctx, id, &TextInput { window: id, text });
                }
                if !event.repeat
                    && let Some(code) = match event.physical_key {
                        PhysicalKey::Code(code) => map_winit_keycode(code),
                        PhysicalKey::Unidentified(_) => None,
                    }
                {
                    match event.state {
                        ElementState::Pressed => self.publish(ctx, id, &Key { window: id, code }),
                        ElementState::Released => self.publish(ctx, id, &KeyRelease { window: id, code }),
                    }
                }
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Preedit(text, cursor) => {
                    if let Some(state) = self.windows.get_mut(&id) {
                        text_input_gate(&mut state.composing, TextSource::Preedit { active: !text.is_empty() });
                    }
                    let (cursor_begin, cursor_end) = ime_cursor_span(cursor);
                    self.publish(ctx, id, &ImePreedit { window: id, text, cursor_begin, cursor_end });
                }
                Ime::Commit(text) => {
                    let committed = self
                        .windows
                        .get_mut(&id)
                        .and_then(|state| text_input_gate(&mut state.composing, TextSource::Commit(text)));
                    if let Some(text) = committed {
                        self.publish(ctx, id, &TextInput { window: id, text });
                    }
                }
                Ime::Disabled => {
                    if let Some(state) = self.windows.get_mut(&id) {
                        text_input_gate(&mut state.composing, TextSource::Disabled);
                    }
                }
                Ime::Enabled => {}
            },
            WindowEvent::ModifiersChanged(modifiers) => {
                let state = modifiers.state();
                let modifiers = Modifiers {
                    window: id,
                    shift: state.shift_key(),
                    ctrl: state.control_key(),
                    alt: state.alt_key(),
                    meta: state.super_key(),
                };
                if let Some(window) = self.windows.get_mut(&id) {
                    window.modifiers = modifiers;
                }
                self.publish(ctx, id, &modifiers);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = map_mouse_button(button) {
                    let (x, y) = self.windows.get(&id).map_or((0.0, 0.0), |window| window.cursor);
                    match state {
                        ElementState::Pressed => {
                            self.publish(ctx, id, &MouseButton { window: id, button, x, y });
                        }
                        ElementState::Released => {
                            self.publish(ctx, id, &MouseButtonRelease { window: id, button, x, y });
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (delta_x, delta_y) = normalize_wheel(delta);
                let (x, y) = self.windows.get(&id).map_or((0.0, 0.0), |window| window.cursor);
                self.publish(ctx, id, &MouseWheel { window: id, delta_x, delta_y, x, y });
            }
            WindowEvent::CursorMoved { position, .. } => {
                #[allow(clippy::cast_possible_truncation)]
                let (x, y) = (position.x as f32, position.y as f32);
                if let Some(state) = self.windows.get_mut(&id) {
                    state.cursor = (x, y);
                }
                self.publish(ctx, id, &MouseMove { window: id, x, y });
            }
            _ => {}
        }
    }

    fn failed_create_effects(&mut self, shutdown_on_failure: bool) -> Vec<WindowHostEffect> {
        if self.windows.values().any(|window| window.lifecycle != DesktopWindowLifecycle::Attaching) {
            self.shutdown_when_idle = false;
            return Vec::new();
        }
        if !self.pending_creates.is_empty() {
            self.shutdown_when_idle |= shutdown_on_failure;
            return Vec::new();
        }
        let should_shutdown = self.shutdown_when_idle || shutdown_on_failure;
        self.shutdown_when_idle = false;
        should_shutdown.then_some(WindowHostEffect::LastWindowClosed).into_iter().collect()
    }

    fn queue_create(
        &mut self,
        spec: WindowSpec,
        reply: Option<Box<InboundMail>>,
        shutdown_on_failure: bool,
    ) -> Result<WindowId, (String, Option<Box<InboundMail>>)> {
        if let Err(error) = crate::validate_window_name(&spec.name) {
            return Err((error, reply));
        }
        if self.pending_creates.values().any(|pending| pending.spec.name == spec.name)
            || self.windows.values().any(|window| window.name == spec.name)
        {
            return Err((format!("window name `{}` is already in use", spec.name), reply));
        }
        let id = predicted_window_id(&spec.name);
        self.pending_host_actions.push_back(WindowHostAction::Create { id, spec: spec.clone() });
        self.pending_creates.insert(id, PendingCreate { spec, reply, shutdown_on_failure, staged: None });
        Ok(id)
    }

    fn queue_close(
        &mut self,
        id: WindowId,
        reply: Option<Box<InboundMail>>,
    ) -> Result<(), (String, Option<Box<InboundMail>>)> {
        let Some(state) = self.windows.get_mut(&id) else {
            return Err((format!("unknown window {id:?}"), reply));
        };
        if state.lifecycle != DesktopWindowLifecycle::Live {
            return Err((format!("window {id:?} is not live"), reply));
        }
        state.lifecycle = DesktopWindowLifecycle::Closing;
        state.close_reply = reply;
        self.pending_host_actions.push_back(WindowHostAction::Close { id });
        Ok(())
    }

    fn remove_window(&mut self, id: WindowId) -> bool {
        if let Some(window) = self.native_windows.remove(&id) {
            self.winit_windows.remove(&window.id());
        } else {
            self.winit_windows.retain(|_, mapped| *mapped != id);
        }
        self.windows.remove(&id).is_some()
    }

    fn live_window(&self, id: WindowId) -> Result<Arc<Window>, String> {
        match self.windows.get(&id) {
            None => Err(format!("unknown window {id:?}")),
            Some(window) if window.lifecycle != DesktopWindowLifecycle::Live => {
                Err(format!("window {id:?} is not live"))
            }
            Some(_) => {
                self.native_windows.get(&id).cloned().ok_or_else(|| format!("window {id:?} has no native handle"))
            }
        }
    }

    fn publish<K: Kind>(&self, ctx: &mut NativeCtx<'_>, window: WindowId, event: &K) {
        ctx.fanout(self.subscribers.recipients(window, K::ID), event);
    }
}

fn predicted_window_id(name: &str) -> WindowId {
    WindowId(WindowInstance::resolve(WindowCapability::resolve(0, ()).0, name).0)
}

#[runtime]
impl NativeActor for DesktopWindowCapability {
    type State = DesktopWindowCapabilityState;

    type Config = ();
    type Params = DesktopWindowParams;

    const NAMESPACE: &'static str = crate::WINDOW_NAMESPACE;

    fn init(
        (): (),
        _params: DesktopWindowParams,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<DesktopWindowCapabilityState, BootError> {
        Ok(DesktopWindowCapabilityState {
            windows: BTreeMap::new(),
            native_windows: HashMap::new(),
            winit_windows: HashMap::new(),
            child_monitors: HashMap::new(),
            subscribers: WindowSubscribers::new(),
            pending_creates: HashMap::new(),
            pending_host_actions: VecDeque::new(),
            pending_host_effects: Vec::new(),
            initial_window_reserved: false,
            shutdown_when_idle: false,
        })
    }

    fn unwire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        for (_, mut pending) in state.pending_creates.drain() {
            // A staged child may not have been applied yet, so the retirement
            // rides the ordered tail its reserved route already parks.
            if let Some(staged) = &pending.staged {
                ctx.actor_at::<DesktopWindowInstance>(staged.child).send(&RetireWindow);
            }
            if let Some(reply) = pending.reply.take() {
                reply.reply(&CreateWindowResult::Err { error: "window manager shutting down".to_owned() });
            }
        }
        for (id, window) in &mut state.windows {
            if let Some(reply) = window.close_reply.take() {
                reply.reply(&ApplyWindowCommandResult::Close(CloseWindowResult::Err {
                    error: format!("window manager shutting down before closing {id:?}"),
                }));
            }
        }
    }

    #[handler::single]
    fn on_list(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListWindows) -> ListWindowsResult {
        ListWindowsResult::Ok {
            windows: state
                .windows
                .iter()
                .filter(|(_, window)| window.lifecycle != DesktopWindowLifecycle::Attaching)
                .map(|(id, window)| window.info(*id))
                .collect(),
        }
    }

    #[handler(task)]
    fn on_window_child_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<Result<SpawnApplied, SpawnError>, WindowChildContinuation>,
    ) {
        state.finish_window_child_spawn(ctx, done.context().id, done.output());
        done.release_no_reply();
    }

    #[handler::manual]
    fn on_create(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: CreateWindow) {
        let reply = ctx.take_inbound();
        if let Err((error, reply)) = state.queue_create(mail.spec, Some(Box::new(reply)), false)
            && let Some(reply) = reply
        {
            reply.reply(&CreateWindowResult::Err { error });
        }
    }

    #[handler::manual]
    fn on_apply_command(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: ApplyWindowCommand) {
        let reply = ctx.take_inbound();
        let result = match mail.command {
            WindowCommand::Close => {
                if let Err((error, reply)) = state.queue_close(mail.window, Some(Box::new(reply)))
                    && let Some(reply) = reply
                {
                    reply.reply(&ApplyWindowCommandResult::Close(CloseWindowResult::Err { error }));
                }
                return;
            }
            WindowCommand::SetMode { mode, width, height } => {
                let window = match state.live_window(mail.window) {
                    Ok(window) => window,
                    Err(error) => {
                        reply.reply(&ApplyWindowCommandResult::SetMode(SetWindowModeResult::Err { error }));
                        return;
                    }
                };
                let fullscreen = match resolve_fullscreen(&mode, window.current_monitor().as_ref()) {
                    Ok(fullscreen) => fullscreen,
                    Err(error) => {
                        reply.reply(&ApplyWindowCommandResult::SetMode(SetWindowModeResult::Err { error }));
                        return;
                    }
                };
                window.set_fullscreen(fullscreen);
                if matches!(mode, WindowMode::Windowed)
                    && let (Some(width), Some(height)) = (width, height)
                {
                    let _ = window.request_inner_size(PhysicalSize::new(width, height));
                }
                window.request_redraw();
                let size = window.inner_size();
                if let Some(state) = state.windows.get_mut(&mail.window) {
                    state.mode = mode.clone();
                    state.width = size.width;
                    state.height = size.height;
                }
                ApplyWindowCommandResult::SetMode(SetWindowModeResult::Ok {
                    mode,
                    width: size.width,
                    height: size.height,
                })
            }
            WindowCommand::SetTitle { title } => {
                let window = match state.live_window(mail.window) {
                    Ok(window) => window,
                    Err(error) => {
                        reply.reply(&ApplyWindowCommandResult::SetTitle(SetWindowTitleResult::Err { error }));
                        return;
                    }
                };
                window.set_title(&title);
                if let Some(state) = state.windows.get_mut(&mail.window) {
                    state.title.clone_from(&title);
                }
                ApplyWindowCommandResult::SetTitle(SetWindowTitleResult::Ok { title })
            }
            WindowCommand::Focus => {
                let window = match state.live_window(mail.window) {
                    Ok(window) => window,
                    Err(error) => {
                        reply.reply(&ApplyWindowCommandResult::Focus(FocusWindowResult::Err { error }));
                        return;
                    }
                };
                window.set_minimized(false);
                window.set_visible(true);
                window.focus_window();
                ApplyWindowCommandResult::Focus(FocusWindowResult::Ok)
            }
            WindowCommand::RequestRedraw => {
                let window = match state.live_window(mail.window) {
                    Ok(window) => window,
                    Err(error) => {
                        reply.reply(&ApplyWindowCommandResult::RequestRedraw(RequestWindowRedrawResult::Err { error }));
                        return;
                    }
                };
                window.request_redraw();
                ApplyWindowCommandResult::RequestRedraw(RequestWindowRedrawResult::Ok)
            }
        };
        reply.reply(&result);
    }

    #[handler::single]
    fn on_subscribe(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SubscribeWindow) -> SubscribeWindowResult {
        if let Err(error) = validate_subscriber_mailbox(ctx, mail.mailbox) {
            return SubscribeWindowResult::Err { error };
        }
        state.subscribers.subscribe(ctx, mail.selector, mail.kind, mail.mailbox);
        SubscribeWindowResult::Ok
    }

    #[handler::single]
    fn on_subscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        match state.subscribers.subscribe_self(ctx, mail.selector, mail.kind) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::Err { error },
        }
    }

    #[handler::single]
    fn on_unsubscribe(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: UnsubscribeWindow,
    ) -> SubscribeWindowResult {
        if let Err(error) = validate_subscriber_mailbox(ctx, mail.mailbox) {
            return SubscribeWindowResult::Err { error };
        }
        state.subscribers.unsubscribe(mail.selector, mail.kind, mail.mailbox);
        SubscribeWindowResult::Ok
    }

    #[handler::single]
    fn on_unsubscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: UnsubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        match state.subscribers.unsubscribe_self(ctx, mail.selector, mail.kind) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::Err { error },
        }
    }

    #[handler::single]
    fn on_unsubscribe_all(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: UnsubscribeAllWindows) {
        state.subscribers.unsubscribe_all(mail.mailbox);
    }

    #[handler::single]
    fn on_monitor_notice(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, notice: MonitorNotice) {
        let id = WindowId(notice.target.0);
        if state.child_monitors.remove(&id).is_some() {
            let _ = state.queue_close(id, None);
        }
        state.subscribers.purge_departed(notice.target);
    }
}

fn find_exclusive_mode(
    monitor: &WinitMonitorHandle,
    width: u32,
    height: u32,
    refresh_mhz: u32,
) -> Option<VideoModeHandle> {
    monitor.video_modes().find(|mode| {
        mode.size().width == width && mode.size().height == height && mode.refresh_rate_millihertz() == refresh_mhz
    })
}

/// Resolve a public window mode to winit's native fullscreen representation.
pub fn resolve_fullscreen(
    mode: &WindowMode,
    monitor_for_exclusive: Option<&WinitMonitorHandle>,
) -> Result<Option<Fullscreen>, String> {
    match mode {
        WindowMode::Windowed => Ok(None),
        WindowMode::FullscreenBorderless => Ok(Some(Fullscreen::Borderless(None))),
        WindowMode::FullscreenExclusive { width, height, refresh_mhz } => {
            let monitor = monitor_for_exclusive
                .ok_or_else(|| "fullscreen-exclusive requested but no monitor available".to_owned())?;
            let handle = find_exclusive_mode(monitor, *width, *height, *refresh_mhz).ok_or_else(|| {
                format!("no video mode matches {width}x{height}@{refresh_mhz}mhz on monitor {:?}", monitor.name())
            })?;
            Ok(Some(Fullscreen::Exclusive(handle)))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::mpsc;

    use aether_substrate::Registry;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::{InboxHandler, MailDispatch, OwnedDispatch};
    use aether_substrate::mail::{MailId, Source, SourceAddr};

    use super::*;

    fn test_state() -> DesktopWindowCapabilityState {
        DesktopWindowCapabilityState {
            windows: BTreeMap::new(),
            native_windows: HashMap::new(),
            winit_windows: HashMap::new(),
            child_monitors: HashMap::new(),
            subscribers: WindowSubscribers::new(),
            pending_creates: HashMap::new(),
            pending_host_actions: VecDeque::new(),
            pending_host_effects: Vec::new(),
            initial_window_reserved: false,
            shutdown_when_idle: false,
        }
    }

    fn test_ctx() -> (Arc<NativeBinding>, Arc<Mailer>) {
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(1)));
        (binding, mailer)
    }

    fn spec(name: &str, title: &str) -> WindowSpec {
        WindowSpec { name: name.to_owned(), title: title.to_owned(), mode: WindowMode::Windowed, size: None }
    }

    fn insert_window(state: &mut DesktopWindowCapabilityState, id: WindowId, name: &str, closing: bool) {
        state.windows.insert(
            id,
            DesktopWindowState {
                name: name.to_owned(),
                title: format!("window-{}", id.0),
                mode: WindowMode::Windowed,
                width: 640,
                height: 480,
                cursor: (0.0, 0.0),
                composing: false,
                modifiers: Modifiers { window: id, ..Modifiers::default() },
                focused: false,
                occluded: false,
                lifecycle: if closing {
                    DesktopWindowLifecycle::Closing
                } else {
                    DesktopWindowLifecycle::Live
                },
                close_reply: None,
            },
        );
    }

    #[test]
    fn explicit_subscriptions_validate_before_mutating_routes() {
        let mut state = test_state();
        let (binding, mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let unknown = MailboxId(0xBAD);

        assert!(matches!(
            DesktopWindowCapability::on_subscribe(
                &mut state,
                &mut ctx,
                SubscribeWindow { selector: crate::WindowSelector::All, kind: Key::ID, mailbox: unknown },
            ),
            SubscribeWindowResult::Err { error } if error == "unknown mailbox id 0x0000000000000bad"
        ));
        assert!(state.subscribers.recipients(WindowId(1), Key::ID).is_empty());

        let dropped =
            mailer.registry().register_inline("test.window.dropped", Arc::new(|_dispatch: MailDispatch<'_>| {}));
        state.subscribers.subscribe(&mut ctx, crate::WindowSelector::All, Key::ID, dropped);
        mailer.registry().drop_mailbox(dropped).expect("drop subscriber mailbox");

        assert!(matches!(
            DesktopWindowCapability::on_unsubscribe(
                &mut state,
                &mut ctx,
                UnsubscribeWindow { selector: crate::WindowSelector::All, kind: Key::ID, mailbox: dropped },
            ),
            SubscribeWindowResult::Err { error } if error == format!("mailbox {dropped:?} already dropped")
        ));
        assert_eq!(state.subscribers.recipients(WindowId(1), Key::ID), BTreeSet::from([dropped]));
    }

    #[test]
    fn window_ids_derive_from_named_child_mailboxes_and_actions_remain_ordered() {
        let mut state = test_state();
        assert!(state.queue_create(spec("first", "First"), None, false).is_ok());
        assert!(state.queue_create(spec("second", "Second"), None, false).is_ok());

        let (actions, _) = state.take_host_work();
        assert!(matches!(actions[0], WindowHostAction::Create { id, .. } if id == predicted_window_id("first")));
        assert!(matches!(actions[1], WindowHostAction::Create { id, .. } if id == predicted_window_id("second")));
        assert_ne!(predicted_window_id("first"), predicted_window_id("second"));
    }

    #[test]
    fn invalid_names_are_rejected_before_initial_reservation() {
        for name in ["", "two words", "bad:name"] {
            let mut state = test_state();

            assert!(state.queue_initial_window(spec(name, "Invalid")).is_err());
            assert!(!state.initial_window_reserved);
            assert!(state.pending_creates.is_empty());
            assert!(state.pending_host_actions.is_empty());
        }
    }

    #[test]
    fn duplicate_pending_and_live_names_are_rejected() {
        let mut pending = test_state();
        assert!(pending.queue_create(spec("tools", "Tools"), None, false).is_ok());
        assert!(pending.queue_create(spec("tools", "Other title"), None, false).is_err());

        let mut live = test_state();
        insert_window(&mut live, WindowId(7), "tools", false);
        assert!(live.queue_create(spec("tools", "Other title"), None, false).is_err());
        assert!(live.pending_host_actions.is_empty());
    }

    #[test]
    fn distinct_valid_names_are_reserved_independently() {
        let mut state = test_state();
        assert!(state.queue_create(spec("main", "Game"), None, false).is_ok());
        assert!(state.queue_create(spec("palette", "Tools"), None, false).is_ok());

        assert_eq!(
            state.pending_creates.values().map(|pending| pending.spec.name.as_str()).collect::<BTreeSet<_>>(),
            BTreeSet::from(["main", "palette"]),
        );
    }

    #[test]
    fn window_name_is_stable_when_title_changes() {
        let mut state = test_state();
        insert_window(&mut state, WindowId(1), "main", false);

        state.windows.get_mut(&WindowId(1)).expect("live window").title = "Renamed".to_owned();
        let info = state.windows[&WindowId(1)].info(WindowId(1));

        assert_eq!(info.name, "main");
        assert_eq!(info.title, "Renamed");
    }

    #[test]
    fn list_windows_is_sorted_by_engine_identity() {
        let mut state = test_state();
        insert_window(&mut state, WindowId(9), "nine", false);
        insert_window(&mut state, WindowId(2), "two", false);
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        let ListWindowsResult::Ok { windows } = DesktopWindowCapability::on_list(&mut state, &mut ctx, ListWindows)
        else {
            panic!("desktop manager list succeeds");
        };

        assert_eq!(windows.iter().map(|window| window.id).collect::<Vec<_>>(), [WindowId(2), WindowId(9)]);
    }

    /// Reducer-only: staging a real child needs a chassis-built binding this
    /// fixture cannot supply, so the reservation is assembled directly. It
    /// pins the two properties the staged path owes — a reserved child is not
    /// enumerable, and an authoritative rejection rolls the window back and
    /// answers the caller exactly once.
    #[test]
    fn a_reserved_window_child_is_not_enumerable_and_rolls_back_when_its_birth_fails() {
        let mut state = test_state();
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let id = predicted_window_id("tools");

        assert!(state.queue_create(spec("tools", "Tools"), None, true).is_ok(), "reserve the create");
        insert_window(&mut state, id, "tools", false);
        state.windows.get_mut(&id).expect("attaching window").lifecycle = DesktopWindowLifecycle::Attaching;
        state.pending_creates.get_mut(&id).expect("pending create").staged =
            Some(StagedWindowChild { child: MailboxId(id.0), rejection: None });

        let ListWindowsResult::Ok { windows } = DesktopWindowCapability::on_list(&mut state, &mut ctx, ListWindows)
        else {
            panic!("desktop manager list succeeds");
        };
        assert!(windows.is_empty(), "a reserved window child is absent from live enumeration");

        state.finish_window_child_spawn(&mut ctx, id, &Err(SpawnError::OwnerClosed));

        assert!(!state.windows.contains_key(&id), "a rejected birth rolls its window back");
        assert!(state.pending_creates.is_empty(), "a rejected birth clears its reservation");
        assert!(
            state
                .pending_host_effects
                .iter()
                .any(|effect| matches!(effect, WindowHostEffect::Closing { id: closing } if *closing == id)),
            "rollback detaches the native window through the host-effect queue",
        );
    }

    #[test]
    fn initial_window_is_reserved_once_across_repeated_resumes() {
        let mut state = test_state();
        state.queue_initial_window(spec("main", "boot")).expect("first resume");
        state.queue_initial_window(spec("ignored", "ignored")).expect("second resume");

        let (actions, _) = state.take_host_work();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], WindowHostAction::Create { spec, .. } if spec.title == "boot"));
    }

    #[test]
    fn closing_one_window_does_not_request_global_shutdown() {
        let mut state = test_state();
        insert_window(&mut state, WindowId(1), "first", true);
        insert_window(&mut state, WindowId(2), "second", false);
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        let effects = state.finish_window_close(WindowId(1), &mut ctx);

        assert!(effects.is_empty());
        assert!(state.windows.contains_key(&WindowId(2)));
    }

    #[test]
    fn closing_the_last_window_requests_shutdown_after_removal() {
        let mut state = test_state();
        insert_window(&mut state, WindowId(1), "first", true);
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        let effects = state.finish_window_close(WindowId(1), &mut ctx);

        assert!(matches!(effects.as_slice(), [WindowHostEffect::LastWindowClosed]));
        assert!(state.windows.is_empty());
    }

    #[test]
    fn pending_replacement_defers_last_window_shutdown_until_create_resolves() {
        let mut state = test_state();
        insert_window(&mut state, WindowId(1), "first", true);
        assert!(state.queue_create(spec("replacement", "Replacement"), None, false).is_ok());
        let replacement = predicted_window_id("replacement");
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        assert!(state.finish_window_close(WindowId(1), &mut ctx).is_empty());
        assert!(state.shutdown_when_idle);
        let effects = state.fail_window_creation(replacement, "native create failed".to_owned());

        assert!(matches!(effects.as_slice(), [WindowHostEffect::LastWindowClosed]));
    }

    #[test]
    fn failed_initial_create_rolls_back_and_requests_shutdown() {
        let mut state = test_state();
        state.queue_initial_window(spec("main", "boot")).expect("reserve boot window");
        let effects = state.fail_window_creation(predicted_window_id("main"), "native create failed".to_owned());

        assert!(matches!(effects.as_slice(), [WindowHostEffect::LastWindowClosed]));
        assert!(state.pending_creates.is_empty());
    }

    #[test]
    fn failed_attachment_removes_the_staged_initial_window_before_shutdown() {
        let mut state = test_state();
        state.queue_initial_window(spec("main", "boot")).expect("reserve boot window");
        let id = predicted_window_id("main");
        insert_window(&mut state, id, "main", false);
        state.windows.get_mut(&id).expect("staged window").lifecycle = DesktopWindowLifecycle::Attaching;
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        let effects = state.finish_window_attachment(id, Err("render attach failed".to_owned()), &mut ctx);

        assert!(matches!(effects.as_slice(), [WindowHostEffect::LastWindowClosed]));
        assert!(!state.windows.contains_key(&id));
        assert!(state.pending_creates.is_empty());
    }

    #[test]
    fn direct_publication_preserves_source_and_causal_lineage() {
        let registry = Arc::new(Registry::new());
        let (tx, rx) = mpsc::channel();
        let subscriber = registry.register_inbox(
            "test.window.subscriber",
            Arc::new(move |dispatch: OwnedDispatch| {
                dispatch.discharge();
                tx.send(dispatch).expect("record routed window event");
            }) as Arc<dyn InboxHandler>,
        );
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry)));
        let manager = MailboxId(0xA37E);
        let binding = Arc::new(NativeBinding::new_for_test(mailer, manager));
        let mut state = test_state();
        let root = MailId::new(MailboxId(0x100), 7);
        let parent = MailId::new(MailboxId(0x200), 9);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, parent, root);
        state.subscribers.subscribe(&mut ctx, crate::WindowSelector::All, Key::ID, subscriber);

        state.publish(&mut ctx, WindowId(5), &Key { window: WindowId(5), code: 41 });
        drop(ctx);

        let dispatch = rx.recv().expect("direct subscriber receives the event");
        assert_eq!(dispatch.root, root);
        assert_eq!(dispatch.parent_mail, Some(parent));
        assert_eq!(dispatch.sender.addr, SourceAddr::Component(manager));
        assert_eq!(Key::decode_from_bytes(dispatch.payload.bytes()), Some(Key { window: WindowId(5), code: 41 }),);
    }
}
