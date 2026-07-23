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

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, OnceLock};

use aether_actor::{HandlesKind, Manual, actor};
use aether_data::Kind;
use aether_input::InputCapability;
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MonitorNotice, MouseButton, MouseButtonRelease, MouseMove, MouseWheel,
    TextInput, WindowMode, WindowSize,
};
use aether_substrate::InboundMail;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Ime, WindowEvent};
use winit::keyboard::PhysicalKey;
use winit::monitor::{MonitorHandle, VideoModeHandle};
use winit::window::{Fullscreen, Window, WindowId as WinitWindowId};

use self::input::{TextSource, ime_cursor_span, map_mouse_button, map_winit_keycode, normalize_wheel, text_input_gate};
use super::{
    CloseWindow, CloseWindowResult, CreateWindow, CreateWindowResult, FocusWindow, FocusWindowResult, ListWindows,
    ListWindowsResult, RequestWindowRedraw, RequestWindowRedrawResult, SetWindowMode, SetWindowModeResult,
    SetWindowTitle, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult, SubscribeWindowSelf,
    UnsubscribeAllWindows, UnsubscribeWindow, UnsubscribeWindowSelf, WindowClosed, WindowId, WindowInfo, WindowOpened,
    WindowSpec,
};
use crate::subscribers::WindowSubscribers;

pub use application::{DesktopWindowApplication, DesktopWindowIntegration, DesktopWindowUserEvent};

/// Temporary single-render-target cell retained only for the desktop
/// compatibility bridge. Window-manager state does not read or write it.
pub type WindowCell = Arc<OnceLock<Arc<Window>>>;

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
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DesktopWindowLifecycle {
    Attaching,
    Live,
    Closing,
}

struct DesktopWindowState {
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
/// Engine identities are monotonic and never reused. The `BTreeMap` makes
/// `ListWindows` naturally ordered; the two hash maps provide constant-time
/// native lookup without exposing winit identities on the wire.
pub struct DesktopWindowCapabilityState {
    next_window_id: u64,
    windows: BTreeMap<WindowId, DesktopWindowState>,
    native_windows: HashMap<WindowId, Arc<Window>>,
    winit_windows: HashMap<WinitWindowId, WindowId>,
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
        self.initial_window_reserved = true;
        self.queue_create(spec, None, true).map(|_| ()).map_err(|(error, _)| error)
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

    /// Complete render attachment for a staged create. Success makes the
    /// window live and publishes/replies with its final metadata; failure
    /// rolls every mapping back before replying.
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
                let Some(state) = self.windows.get_mut(&id) else {
                    let error = format!("window {id:?} disappeared during attachment");
                    self.remove_window(id);
                    if let Some(reply) = pending.reply.take() {
                        reply.reply(&CreateWindowResult::Err { error });
                    }
                    return self.failed_create_effects(pending.shutdown_on_failure);
                };
                state.lifecycle = DesktopWindowLifecycle::Live;
                self.shutdown_when_idle = false;
                let info = state.info(id);
                if let Some(reply) = pending.reply.take() {
                    reply.reply(&CreateWindowResult::Ok { window: info.clone() });
                }
                self.publish(ctx, id, &WindowOpened { window: info.clone() });
                if info.width != 0 && info.height != 0 {
                    self.publish_input(ctx, id, &WindowSize { window: id, width: info.width, height: info.height });
                }
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
                reply.reply(&CloseWindowResult::Err { window: id, error: format!("unknown window {id:?}") });
            }
            return Vec::new();
        }
        if let Some(reply) = close_reply {
            reply.reply(&CloseWindowResult::Ok { window: id });
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

    /// Translate one native window event and update manager state. Every
    /// typed publication goes both to selector-aware direct subscribers and,
    /// temporarily, to `aether.input` for unmigrated consumers.
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
                    self.publish_input(ctx, id, &WindowSize { window: id, width: size.width, height: size.height });
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
                        self.publish_input(ctx, id, &WindowSize { window: id, width: size.width, height: size.height });
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
                    self.publish_input(ctx, id, &TextInput { window: id, text });
                }
                if !event.repeat
                    && let Some(code) = match event.physical_key {
                        PhysicalKey::Code(code) => map_winit_keycode(code),
                        PhysicalKey::Unidentified(_) => None,
                    }
                {
                    match event.state {
                        ElementState::Pressed => self.publish_input(ctx, id, &Key { window: id, code }),
                        ElementState::Released => self.publish_input(ctx, id, &KeyRelease { window: id, code }),
                    }
                }
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Preedit(text, cursor) => {
                    if let Some(state) = self.windows.get_mut(&id) {
                        text_input_gate(&mut state.composing, TextSource::Preedit { active: !text.is_empty() });
                    }
                    let (cursor_begin, cursor_end) = ime_cursor_span(cursor);
                    self.publish_input(ctx, id, &ImePreedit { window: id, text, cursor_begin, cursor_end });
                }
                Ime::Commit(text) => {
                    let committed = self
                        .windows
                        .get_mut(&id)
                        .and_then(|state| text_input_gate(&mut state.composing, TextSource::Commit(text)));
                    if let Some(text) = committed {
                        self.publish_input(ctx, id, &TextInput { window: id, text });
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
                self.publish_input(ctx, id, &modifiers);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = map_mouse_button(button) {
                    let (x, y) = self.windows.get(&id).map_or((0.0, 0.0), |window| window.cursor);
                    match state {
                        ElementState::Pressed => {
                            self.publish_input(ctx, id, &MouseButton { window: id, button, x, y });
                        }
                        ElementState::Released => {
                            self.publish_input(ctx, id, &MouseButtonRelease { window: id, button, x, y });
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (delta_x, delta_y) = normalize_wheel(delta);
                let (x, y) = self.windows.get(&id).map_or((0.0, 0.0), |window| window.cursor);
                self.publish_input(ctx, id, &MouseWheel { window: id, delta_x, delta_y, x, y });
            }
            WindowEvent::CursorMoved { position, .. } => {
                #[allow(clippy::cast_possible_truncation)]
                let (x, y) = (position.x as f32, position.y as f32);
                if let Some(state) = self.windows.get_mut(&id) {
                    state.cursor = (x, y);
                }
                self.publish_input(ctx, id, &MouseMove { window: id, x, y });
            }
            _ => {}
        }
    }

    fn allocate_window_id(&mut self) -> Result<WindowId, String> {
        let id = self.next_window_id;
        if id == 0 {
            return Err("window identity space exhausted".to_owned());
        }
        self.next_window_id = self.next_window_id.wrapping_add(1);
        Ok(WindowId(id))
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
        let id = match self.allocate_window_id() {
            Ok(id) => id,
            Err(error) => return Err((error, reply)),
        };
        self.pending_host_actions.push_back(WindowHostAction::Create { id, spec: spec.clone() });
        self.pending_creates.insert(id, PendingCreate { spec, reply, shutdown_on_failure });
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

    fn publish_input<K: Kind>(&self, ctx: &mut NativeCtx<'_>, window: WindowId, event: &K)
    where
        InputCapability: HandlesKind<K>,
    {
        self.publish(ctx, window, event);
        ctx.actor::<InputCapability>().send(event);
    }
}

/// Desktop implementation identity for the neutral `aether.window` mailbox.
pub struct DesktopWindowCapability;

#[actor(singleton)]
impl NativeActor for DesktopWindowCapability {
    type State = DesktopWindowCapabilityState;

    type Config = ();
    type Params = DesktopWindowParams;

    const NAMESPACE: &'static str = crate::WINDOW_NAMESPACE;

    fn init(
        (): (),
        _params: DesktopWindowParams,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<DesktopWindowCapabilityState, BootError> {
        Ok(DesktopWindowCapabilityState {
            next_window_id: 1,
            windows: BTreeMap::new(),
            native_windows: HashMap::new(),
            winit_windows: HashMap::new(),
            subscribers: WindowSubscribers::new(Arc::clone(ctx.mailer().registry())),
            pending_creates: HashMap::new(),
            pending_host_actions: VecDeque::new(),
            pending_host_effects: Vec::new(),
            initial_window_reserved: false,
            shutdown_when_idle: false,
        })
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        for (_, mut pending) in state.pending_creates.drain() {
            if let Some(reply) = pending.reply.take() {
                reply.reply(&CreateWindowResult::Err { error: "window manager shutting down".to_owned() });
            }
        }
        for (id, window) in &mut state.windows {
            if let Some(reply) = window.close_reply.take() {
                reply.reply(&CloseWindowResult::Err { window: *id, error: "window manager shutting down".to_owned() });
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
    fn on_close(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: CloseWindow) {
        let reply = ctx.take_inbound();
        if let Err((error, reply)) = state.queue_close(mail.window, Some(Box::new(reply)))
            && let Some(reply) = reply
        {
            reply.reply(&CloseWindowResult::Err { window: mail.window, error });
        }
    }

    #[handler::single]
    fn on_set_mode(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: SetWindowMode) -> SetWindowModeResult {
        let window = match state.live_window(mail.window) {
            Ok(window) => window,
            Err(error) => return SetWindowModeResult::Err { window: mail.window, error },
        };
        let fullscreen = match resolve_fullscreen(&mail.mode, window.current_monitor().as_ref()) {
            Ok(fullscreen) => fullscreen,
            Err(error) => return SetWindowModeResult::Err { window: mail.window, error },
        };
        window.set_fullscreen(fullscreen);
        if matches!(mail.mode, WindowMode::Windowed)
            && let (Some(width), Some(height)) = (mail.width, mail.height)
        {
            let _ = window.request_inner_size(PhysicalSize::new(width, height));
        }
        window.request_redraw();
        let size = window.inner_size();
        if let Some(state) = state.windows.get_mut(&mail.window) {
            state.mode = mail.mode.clone();
            state.width = size.width;
            state.height = size.height;
        }
        SetWindowModeResult::Ok { window: mail.window, mode: mail.mode, width: size.width, height: size.height }
    }

    #[handler::single]
    fn on_set_title(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: SetWindowTitle) -> SetWindowTitleResult {
        let window = match state.live_window(mail.window) {
            Ok(window) => window,
            Err(error) => return SetWindowTitleResult::Err { window: mail.window, error },
        };
        window.set_title(&mail.title);
        if let Some(state) = state.windows.get_mut(&mail.window) {
            state.title.clone_from(&mail.title);
        }
        SetWindowTitleResult::Ok { window: mail.window, title: mail.title }
    }

    #[handler::single]
    fn on_focus(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: FocusWindow) -> FocusWindowResult {
        let window = match state.live_window(mail.window) {
            Ok(window) => window,
            Err(error) => return FocusWindowResult::Err { window: mail.window, error },
        };
        window.set_minimized(false);
        window.set_visible(true);
        window.focus_window();
        FocusWindowResult::Ok { window: mail.window }
    }

    #[handler::single]
    fn on_request_redraw(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: RequestWindowRedraw,
    ) -> RequestWindowRedrawResult {
        let window = match state.live_window(mail.window) {
            Ok(window) => window,
            Err(error) => return RequestWindowRedrawResult::Err { window: mail.window, error },
        };
        window.request_redraw();
        RequestWindowRedrawResult::Ok { window: mail.window }
    }

    #[handler::single]
    fn on_subscribe(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SubscribeWindow) -> SubscribeWindowResult {
        match state.subscribers.subscribe(ctx, mail.selector, mail.kind, mail.mailbox) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::Err { error },
        }
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
        _ctx: &mut NativeCtx<'_>,
        mail: UnsubscribeWindow,
    ) -> SubscribeWindowResult {
        match state.subscribers.unsubscribe(mail.selector, mail.kind, mail.mailbox) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::Err { error },
        }
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
        state.subscribers.purge_departed(notice.target);
    }
}

fn find_exclusive_mode(monitor: &MonitorHandle, width: u32, height: u32, refresh_mhz: u32) -> Option<VideoModeHandle> {
    monitor.video_modes().find(|mode| {
        mode.size().width == width && mode.size().height == height && mode.refresh_rate_millihertz() == refresh_mhz
    })
}

/// Resolve a public window mode to winit's native fullscreen representation.
pub fn resolve_fullscreen(
    mode: &WindowMode,
    monitor_for_exclusive: Option<&MonitorHandle>,
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
    use std::sync::mpsc;

    use aether_substrate::Registry;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::{InboxHandler, OwnedDispatch};
    use aether_substrate::mail::{MailId, Source, SourceAddr};

    use super::*;

    fn test_state() -> DesktopWindowCapabilityState {
        let registry = Arc::new(Registry::new());
        DesktopWindowCapabilityState {
            next_window_id: 1,
            windows: BTreeMap::new(),
            native_windows: HashMap::new(),
            winit_windows: HashMap::new(),
            subscribers: WindowSubscribers::new(registry),
            pending_creates: HashMap::new(),
            pending_host_actions: VecDeque::new(),
            pending_host_effects: Vec::new(),
            initial_window_reserved: false,
            shutdown_when_idle: false,
        }
    }

    fn test_ctx() -> (Arc<NativeBinding>, Arc<Mailer>) {
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), aether_data::MailboxId(1)));
        (binding, mailer)
    }

    fn spec(title: &str) -> WindowSpec {
        WindowSpec { title: title.to_owned(), mode: WindowMode::Windowed, size: None }
    }

    fn insert_window(state: &mut DesktopWindowCapabilityState, id: WindowId, closing: bool) {
        state.windows.insert(
            id,
            DesktopWindowState {
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
    fn window_ids_are_monotonic_and_actions_remain_ordered() {
        let mut state = test_state();
        assert!(state.queue_create(spec("first"), None, false).is_ok());
        assert!(state.queue_create(spec("second"), None, false).is_ok());

        let (actions, _) = state.take_host_work();
        assert!(matches!(actions[0], WindowHostAction::Create { id: WindowId(1), .. }));
        assert!(matches!(actions[1], WindowHostAction::Create { id: WindowId(2), .. }));
        assert_eq!(state.next_window_id, 3);
    }

    #[test]
    fn list_windows_is_sorted_by_engine_identity() {
        let mut state = test_state();
        insert_window(&mut state, WindowId(9), false);
        insert_window(&mut state, WindowId(2), false);
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        let ListWindowsResult::Ok { windows } = DesktopWindowCapability::on_list(&mut state, &mut ctx, ListWindows)
        else {
            panic!("desktop manager list succeeds");
        };

        assert_eq!(windows.iter().map(|window| window.id).collect::<Vec<_>>(), [WindowId(2), WindowId(9)]);
    }

    #[test]
    fn initial_window_is_reserved_once_across_repeated_resumes() {
        let mut state = test_state();
        state.queue_initial_window(spec("boot")).expect("first resume");
        state.queue_initial_window(spec("ignored")).expect("second resume");

        let (actions, _) = state.take_host_work();
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], WindowHostAction::Create { spec, .. } if spec.title == "boot"));
    }

    #[test]
    fn closing_one_window_does_not_request_global_shutdown() {
        let mut state = test_state();
        insert_window(&mut state, WindowId(1), true);
        insert_window(&mut state, WindowId(2), false);
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        let effects = state.finish_window_close(WindowId(1), &mut ctx);

        assert!(effects.is_empty());
        assert!(state.windows.contains_key(&WindowId(2)));
    }

    #[test]
    fn closing_the_last_window_requests_shutdown_after_removal() {
        let mut state = test_state();
        insert_window(&mut state, WindowId(1), true);
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        let effects = state.finish_window_close(WindowId(1), &mut ctx);

        assert!(matches!(effects.as_slice(), [WindowHostEffect::LastWindowClosed]));
        assert!(state.windows.is_empty());
    }

    #[test]
    fn pending_replacement_defers_last_window_shutdown_until_create_resolves() {
        let mut state = test_state();
        insert_window(&mut state, WindowId(1), true);
        state.next_window_id = 2;
        assert!(state.queue_create(spec("replacement"), None, false).is_ok());
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        assert!(state.finish_window_close(WindowId(1), &mut ctx).is_empty());
        assert!(state.shutdown_when_idle);
        let effects = state.fail_window_creation(WindowId(2), "native create failed".to_owned());

        assert!(matches!(effects.as_slice(), [WindowHostEffect::LastWindowClosed]));
    }

    #[test]
    fn failed_initial_create_rolls_back_and_requests_shutdown() {
        let mut state = test_state();
        state.queue_initial_window(spec("boot")).expect("reserve boot window");
        let effects = state.fail_window_creation(WindowId(1), "native create failed".to_owned());

        assert!(matches!(effects.as_slice(), [WindowHostEffect::LastWindowClosed]));
        assert!(state.pending_creates.is_empty());
    }

    #[test]
    fn successful_attachment_makes_the_staged_window_live() {
        let mut state = test_state();
        assert!(state.queue_create(spec("tools"), None, false).is_ok());
        insert_window(&mut state, WindowId(1), false);
        state.windows.get_mut(&WindowId(1)).expect("staged window").lifecycle = DesktopWindowLifecycle::Attaching;
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        let effects = state.finish_window_attachment(WindowId(1), Ok(()), &mut ctx);

        assert!(effects.is_empty());
        assert_eq!(state.windows[&WindowId(1)].lifecycle, DesktopWindowLifecycle::Live);
        assert!(state.pending_creates.is_empty());
    }

    #[test]
    fn failed_attachment_removes_the_staged_initial_window_before_shutdown() {
        let mut state = test_state();
        state.queue_initial_window(spec("boot")).expect("reserve boot window");
        insert_window(&mut state, WindowId(1), false);
        state.windows.get_mut(&WindowId(1)).expect("staged window").lifecycle = DesktopWindowLifecycle::Attaching;
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        let effects = state.finish_window_attachment(WindowId(1), Err("render attach failed".to_owned()), &mut ctx);

        assert!(matches!(effects.as_slice(), [WindowHostEffect::LastWindowClosed]));
        assert!(!state.windows.contains_key(&WindowId(1)));
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
        let manager = aether_data::MailboxId(0xA37E);
        let binding = Arc::new(NativeBinding::new_for_test(mailer, manager));
        let mut state = test_state();
        state.subscribers = WindowSubscribers::new(registry);
        let root = MailId::new(aether_data::MailboxId(0x100), 7);
        let parent = MailId::new(aether_data::MailboxId(0x200), 9);
        let mut ctx = NativeCtx::new(&binding, Source::NONE, parent, root);
        state
            .subscribers
            .subscribe(&mut ctx, super::super::WindowSelector::All, Key::ID, subscriber)
            .expect("registered subscriber is valid");

        state.publish(&mut ctx, WindowId(5), &Key { window: WindowId(5), code: 41 });
        drop(ctx);

        let dispatch = rx.recv().expect("direct subscriber receives the event");
        assert_eq!(dispatch.root, root);
        assert_eq!(dispatch.parent_mail, Some(parent));
        assert_eq!(dispatch.sender.addr, SourceAddr::Component(manager));
        assert_eq!(Key::decode_from_bytes(dispatch.payload.bytes()), Some(Key { window: WindowId(5), code: 41 }),);
    }
}
