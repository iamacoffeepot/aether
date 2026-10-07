//! Desktop `aether.window` manager and native winit integration.
//!
//! The chassis owns the application thread and pumps this actor on that
//! thread. The actor owns every engine/native window identity, window-local
//! state, native-event translation, and selector-aware subscription. Work
//! requiring winit's callback-scoped [`ActiveEventLoop`](winit::event_loop::ActiveEventLoop)
//! crosses the boundary as a [`WindowHostAction`] and returns through a second
//! host turn as a [`WindowHostEffect`].

mod application;
mod cursor;
mod input;
mod menu;
mod pacing;
mod slot;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

use aether_actor::{ActorRef, Anyone, ErasedActorRef, ProtocolRef, ReplyMode, Single};
use aether_data::ErasedActorPath;
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
    WindowMode, WindowSize,
};
use aether_substrate::actor::native::{Held, NativeCtx, SpawnOutcome};
use aether_substrate::runtime::effect_chain::OrderingDevice;
use aether_substrate::{MonitorHandle as ActorMonitorHandle, Subname};
#[cfg(target_os = "macos")]
use objc2::MainThreadMarker;
#[cfg(target_os = "macos")]
use objc2_app_kit::NSApplication;
#[cfg(target_os = "macos")]
use objc2_foundation::NSProcessInfo;
use winit::dpi::{PhysicalSize, Pixel};
use winit::event::{ElementState, Ime, WindowEvent};
use winit::keyboard::PhysicalKey;
use winit::monitor::{MonitorHandle as WinitMonitorHandle, VideoModeHandle};
use winit::window::{Fullscreen, Window, WindowId as WinitWindowId};

use self::cursor::map_cursor_icon;
use self::input::{
    KeyEdge, TextSource, ime_cursor_span, key_edge, map_mouse_button, map_winit_keycode, normalize_wheel,
    text_input_gate,
};
use self::menu::{apply_menu, parse_menu_item_id};
use super::WindowSpawnKey;
use super::manager::{RoutableWindow, WindowCommands};
use super::routing::Routed;
use super::subscribers::WindowSubscribers;
use crate::{
    ApplyWindowCommandResult, CloseWindowResult, CreateWindowResult, FocusWindowResult, RequestWindowRedrawResult,
    RetireWindow, SetWindowCursorResult, SetWindowMenuResult, SetWindowModeResult, SetWindowPresentationResult,
    SetWindowTitleResult, WindowCapability, WindowClosed, WindowCommand, WindowFocus, WindowInfo, WindowInstance,
    WindowMenuActivated, WindowOpened, WindowPresentation, WindowSpec,
};

pub use application::{DesktopWindowApplication, DesktopWindowIntegration, DesktopWindowUserEvent};
pub use slot::DesktopWindowSlot;

#[cfg(target_os = "macos")]
fn activate_application() {
    let Some(marker) = MainThreadMarker::new() else {
        return;
    };
    let application = NSApplication::sharedApplication(marker);
    if NSProcessInfo::processInfo().operatingSystemVersion().majorVersion >= 14 {
        application.activate();
    } else {
        activate_legacy_application(&application);
    }
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn activate_legacy_application(application: &NSApplication) {
    application.activateIgnoringOtherApps(true);
}

#[cfg(not(target_os = "macos"))]
fn activate_application() {}

/// Name this process for the surfaces macOS derives from a process name: `ps`
/// and Activity Monitor, the standard About panel, and the items winit builds
/// into its default menu (`About …`, `Quit …`).
///
/// It does **not** name the menu bar's bold first title. That one comes from
/// the *application's* name, which for an executable outside an `.app` bundle
/// macOS takes from the executed file itself at launch — reachable neither
/// through this call nor by writing `CFBundleName` into the main bundle's info
/// dictionary afterwards, both of which were tried against a live bar
/// (iamacoffeepot/aether#5518). The lever that does move it is the file name,
/// so the fleet materializes a spawned engine's binary under its `--app-name`
/// (`aether_fleet`'s `exec_file_name`) instead of the flat `substrate` that
/// published the hub's storage layout as the product's name.
///
/// Call it on the application thread; every other platform has nothing to set
/// and this is a no-op there.
#[cfg(target_os = "macos")]
pub fn set_application_name(app_name: &str) {
    if MainThreadMarker::new().is_none() {
        return;
    }
    NSProcessInfo::processInfo().setProcessName(&objc2_foundation::NSString::from_str(app_name));
}

#[cfg(not(target_os = "macos"))]
pub fn set_application_name(_app_name: &str) {}

/// Boot input for the desktop backend, carried by
/// [`WindowParams::Desktop`](crate::WindowParams::Desktop).
///
/// It has no public constructor: [`DesktopWindowSlot::boot`] mints the only
/// one, and boots the manager pumped with it. So the desktop backend never
/// runs pooled, where no application thread would realize its host actions.
///
/// The backend needs no native handle or initial identity at boot — the
/// application host reserves the boot window before winit begins dispatching
/// callbacks. What it does need is the application's name, which the platform
/// application menu names its About and Quit items after and which no
/// window-local state can supply.
pub struct DesktopWindowBoot {
    app_name: String,
}

impl DesktopWindowBoot {
    /// The crate's own `Rig` tests boot the desktop backend on a test
    /// chassis's pumped driver rather than a desktop driver.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self { app_name: "Aether".to_owned() }
    }
}

/// Host-only work that must be realized while a winit callback supplies an
/// `ActiveEventLoop`.
#[derive(Clone, Debug)]
pub enum WindowHostAction {
    Create {
        path: ErasedActorPath,
        spec: WindowSpec,
    },
    Close {
        path: ErasedActorPath,
    },
    /// Ask the integration's surface for `presentation`. The window's
    /// surface is render's, so the manager cannot answer the request on its
    /// own turn: its `finish_window_presentation` answers it with what the
    /// integration said.
    SetPresentation {
        path: ErasedActorPath,
        presentation: WindowPresentation,
    },
}

/// Owned semantic changes produced by a window host turn.
#[derive(Clone, Debug)]
pub enum WindowHostEffect {
    Created { path: ErasedActorPath, window: Arc<Window>, presentation: WindowPresentation },
    Closing { path: ErasedActorPath },
    Dirty { path: ErasedActorPath },
    Occluded { path: ErasedActorPath, occluded: bool },
    LastWindowClosed,
}

/// A create still waiting on its native window, render attachment, or staged
/// birth. The window it belongs to stays `Attaching` — absent from
/// `ListWindows`, the frame set, and every publication — until the
/// authoritative [`SpawnOutcome`] lands.
struct PendingCreate {
    spec: WindowSpec,
    /// The `create_window` caller's held reply; the boot window has none.
    held: Option<Held<CreateWindowResult>>,
    shutdown_on_failure: bool,
}

/// A window child that reached `Live`: the reference its spawn outcome proved,
/// which every later retire is sent through, and the monitor whose notice
/// removes the entry once the child departs.
struct WindowChild {
    reference: ActorRef<WindowInstance>,
    _monitor: ActorMonitorHandle,
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
    /// Physical pixels per logical pixel, as winit last reported it. Kept
    /// on the window because `WindowSize` publishes it: every coordinate
    /// this runtime forwards is already physical (winit's `CursorMoved`
    /// carries a `PhysicalPosition`), so the factor is what a consumer
    /// needs to size a logical measure, not to read the cursor.
    scale_factor: f32,
    cursor: (f32, f32),
    composing: bool,
    modifiers: Modifiers,
    focused: bool,
    occluded: bool,
    /// The presentation the window's surface was last configured for: the
    /// spec's at attachment, then each change the integration accepted.
    presentation: WindowPresentation,
    lifecycle: DesktopWindowLifecycle,
    /// The command proof installed when attachment publishes the child. It is
    /// cleared on departure while the explicitly tracked closing window stays
    /// listed and continues to participate in root-command cardinality.
    commands: Option<ProtocolRef<WindowCommands>>,
    /// The forwarding child's held reply for a close in flight, answered once
    /// the integration has detached the window.
    close_held: Option<Held<ApplyWindowCommandResult>>,
}

impl DesktopWindowState {
    fn info(&self, path: &ErasedActorPath) -> WindowInfo {
        WindowInfo {
            path: path.clone(),
            name: self.name.clone(),
            title: self.title.clone(),
            mode: self.mode.clone(),
            width: self.width,
            height: self.height,
            scale_factor: self.scale_factor,
            focused: self.focused,
            occluded: self.occluded,
            presentation: self.presentation,
        }
    }
}

/// The desktop backend's application-scoped state.
///
/// Engine identities are the canonical paths of supervised named children.
/// The `BTreeMap` makes `ListWindows` naturally ordered by path, which is
/// window-name order; the hash maps provide constant-time native lookup
/// without exposing winit identities on the wire.
pub struct DesktopWindows {
    /// The product name the platform application menu is titled with. Boot
    /// input rather than window-local state: macOS has one application menu
    /// for the whole process, whichever window installs it.
    app_name: String,
    windows: BTreeMap<ErasedActorPath, DesktopWindowState>,
    native_windows: HashMap<ErasedActorPath, Arc<Window>>,
    winit_windows: HashMap<WinitWindowId, ErasedActorPath>,
    children: HashMap<ErasedActorPath, WindowChild>,
    /// Each supervised child's window, keyed by the child's reference: the
    /// `MonitorNotice` sender a departing child is found by (ADR-0230).
    child_windows: HashMap<ErasedActorRef, ErasedActorPath>,
    pub(super) subscribers: WindowSubscribers,
    pending_creates: HashMap<ErasedActorPath, PendingCreate>,
    pending_host_actions: VecDeque<WindowHostAction>,
    pending_host_effects: Vec<WindowHostEffect>,
    /// The forwarding children's held replies for presentation changes in
    /// flight, one per queued [`WindowHostAction::SetPresentation`] and in
    /// the same order: the application realizes actions in queue order and
    /// finishes each exactly once, so the front reply is always the one the
    /// action being finished owes. They sit beside the action queue rather
    /// than on a window, so a window removed first still has its caller
    /// answered.
    presentation_helds: VecDeque<Held<ApplyWindowCommandResult>>,
    initial_window_reserved: bool,
    shutdown_when_idle: bool,
}

impl DesktopWindows {
    pub(super) fn new(boot: DesktopWindowBoot) -> Self {
        Self {
            app_name: boot.app_name,
            windows: BTreeMap::new(),
            native_windows: HashMap::new(),
            winit_windows: HashMap::new(),
            children: HashMap::new(),
            child_windows: HashMap::new(),
            subscribers: WindowSubscribers::new(),
            pending_creates: HashMap::new(),
            pending_host_actions: VecDeque::new(),
            pending_host_effects: Vec::new(),
            presentation_helds: VecDeque::new(),
            initial_window_reserved: false,
            shutdown_when_idle: false,
        }
    }

    /// Every window past attachment, in path order: an attaching window is
    /// not yet anyone's to address.
    pub(super) fn list(&self) -> Vec<WindowInfo> {
        self.windows
            .iter()
            .filter(|(_, window)| window.lifecycle != DesktopWindowLifecycle::Attaching)
            .map(|(path, window)| window.info(path))
            .collect()
    }

    /// Queue a create for the application's next host turn; `held` is
    /// answered once the window's child birth is authoritative.
    pub(super) fn create<A>(&mut self, ctx: &mut NativeCtx<'_, A>, spec: WindowSpec, held: Held<CreateWindowResult>) {
        if let Err((error, Some(held))) = self.queue_create(spec, Some(held), false) {
            held.answer(ctx, &CreateWindowResult::Err { error });
        }
    }

    /// Apply one per-window command at the native window of the child that
    /// forwarded it.
    ///
    /// `Close` and `SetPresentation` are the exceptions every other arm is
    /// not. A close cannot answer here, because the window is only gone once
    /// the integration has detached its render target and the manager has
    /// retired its child, so it hands its held reply to the close queue and
    /// is answered from [`Self::finish_window_close`]. A presentation change
    /// cannot either, because the surface that takes or refuses it is the
    /// integration's, so its held reply waits for
    /// [`Self::finish_window_presentation`]. Everything else resolves against
    /// the live `Arc<Window>` on this same turn — the desktop backend is
    /// pumped, so this *is* the winit thread — and answers immediately.
    pub(super) fn apply_command<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        command: WindowCommand,
        held: Held<ApplyWindowCommandResult>,
    ) {
        let Some(path) = ctx.sender().and_then(|sender| self.child_windows.get(&sender).cloned()) else {
            held.answer(
                ctx,
                &command.refused("window command from an actor that is not a live window child".to_owned()),
            );
            return;
        };
        match command {
            WindowCommand::Close => {
                if let Err((error, Some(held))) = self.queue_close(&path, Some(held)) {
                    held.answer(ctx, &ApplyWindowCommandResult::Close(CloseWindowResult::Err { error }));
                }
            }
            WindowCommand::SetPresentation { presentation } => {
                if let Err((error, held)) = self.queue_presentation(&path, presentation, held) {
                    held.answer(
                        ctx,
                        &ApplyWindowCommandResult::SetPresentation(SetWindowPresentationResult::Err { error }),
                    );
                }
            }
            command => held.answer(ctx, &self.apply_at_window(&path, command)),
        }
    }

    /// A monitored actor departed: when it is a window child, its window's
    /// command proof is cleared and the window queued to close.
    pub(super) fn child_departed(&mut self, departed: ErasedActorRef) {
        if let Some(path) = self.child_windows.remove(&departed)
            && self.children.remove(&path).is_some()
        {
            if let Some(window) = self.windows.get_mut(&path) {
                window.commands = None;
            }
            let _ = self.queue_close(&path, None);
        }
    }

    /// The same filter `list` publishes: an attaching window is not yet
    /// anyone's to address, and a closing one still is — its missing or dead
    /// child proof makes the root answer `window … is not live` rather than
    /// hiding it from the count the caller was just shown.
    pub(super) fn routable_windows(&self) -> Vec<RoutableWindow> {
        self.windows
            .iter()
            .filter(|(_, window)| window.lifecycle != DesktopWindowLifecycle::Attaching)
            .map(|(path, window)| RoutableWindow { path: path.clone(), target: window.commands })
            .collect()
    }

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
    pub fn stage_created_window(
        &mut self,
        path: ErasedActorPath,
        window: Arc<Window>,
    ) -> Result<WindowHostEffect, String> {
        let pending =
            self.pending_creates.get(&path).ok_or_else(|| format!("window {path} has no pending create action"))?;
        if self.winit_windows.contains_key(&window.id()) {
            return Err(format!("native window {:?} is already registered", window.id()));
        }
        let size = window.inner_size();
        self.windows.insert(
            path.clone(),
            DesktopWindowState {
                name: pending.spec.name.clone(),
                title: pending.spec.title.clone(),
                mode: pending.spec.mode.clone(),
                width: size.width,
                height: size.height,
                scale_factor: narrow_scale_factor(window.scale_factor()),
                cursor: (0.0, 0.0),
                composing: false,
                modifiers: Modifiers { window: path.clone(), shift: false, ctrl: false, alt: false, meta: false },
                focused: window.has_focus(),
                occluded: size.width == 0 || size.height == 0,
                presentation: pending.spec.presentation,
                lifecycle: DesktopWindowLifecycle::Attaching,
                commands: None,
                close_held: None,
            },
        );
        let presentation = pending.spec.presentation;
        self.winit_windows.insert(window.id(), path.clone());
        self.native_windows.insert(path.clone(), Arc::clone(&window));
        Ok(WindowHostEffect::Created { path, window, presentation })
    }

    /// Complete render attachment for a staged create by staging the window's
    /// child birth. The window stays `Attaching` and the create stays pending
    /// until [`Self::finish_window_child_spawn`] observes the authoritative
    /// [`SpawnOutcome`]; a render failure still rolls back and replies here.
    pub fn finish_window_attachment(
        &mut self,
        path: &ErasedActorPath,
        attachment: Result<(), String>,
        ctx: &mut NativeCtx<'_, WindowCapability, Anyone, Single>,
    ) -> Vec<WindowHostEffect> {
        let Some(mut pending) = self.pending_creates.remove(path) else {
            return Vec::new();
        };
        match attachment {
            Ok(()) => {
                // ADR-0168 §3: this runs on a `PumpedSlot::host_turn`, which
                // carries no chain, so the staged birth takes no settlement
                // hold — and is ordered anyway. `pending.held` is the
                // `create_window` request's held reply (ADR-0243 §1), whose
                // ledger entry keeps the request's settlement hold until
                // `finish_window_child_spawn` answers it, so the request's
                // chain cannot settle through the birth. Declared here because
                // that reasoning is three call frames away from this line. The
                // boot window has no caller, so no debt and no chain: its birth
                // keeps the builder's chainless-turn default. The birth carries
                // the window's path as its completion context, since that path
                // is what the reservation is keyed by.
                let birth = ctx.spawn_child::<WindowInstance>(Subname::Named(&pending.spec.name), (), ());
                let birth = if pending.held.is_some() {
                    birth.ordered_by(OrderingDevice::RetainedReplyDebt)
                } else {
                    birth
                };
                if let Err((error, _)) = birth.stage_with(WindowSpawnKey { path: path.clone() }) {
                    return self.rollback_attached_create(
                        ctx,
                        path,
                        &mut pending,
                        format!("failed to spawn window child: {error:?}"),
                    );
                }

                self.pending_creates.insert(path.clone(), pending);
                Vec::new()
            }
            Err(error) => {
                self.remove_window(path);
                if let Some(held) = pending.held.take() {
                    held.answer(ctx, &CreateWindowResult::Err { error });
                }
                self.failed_create_effects(pending.shutdown_on_failure)
            }
        }
    }

    /// Apply the authoritative result of a staged window child. Success
    /// installs the monitor, promotes the window to `Live`, replies, and
    /// publishes; every failure retires the applied child and rolls the create
    /// back. A child that closed before this ran is promoted all the same,
    /// and its notice, which arrives after this handler returns, retires
    /// the window. Rollback effects go to the host-effect queue because this runs on
    /// an ordinary mail turn rather than inside a native callback.
    pub(super) fn finish_window_child_spawn<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        path: &ErasedActorPath,
        outcome: &SpawnOutcome<WindowInstance>,
    ) {
        let Some(mut pending) = self.pending_creates.remove(path) else {
            if let Ok(child) = &outcome.result {
                ctx.send_to(child, &RetireWindow);
            }
            return;
        };
        let effects = match &outcome.result {
            Err(error) => self.rollback_attached_create(
                ctx,
                path,
                &mut pending,
                format!("failed to spawn window child: {error:?}"),
            ),
            // The reservation is keyed by the path consumers address, so a
            // child that path does not prove dooms it rather than publishing
            // a window nobody can reach: retire the child and roll back
            // before anything answers the caller.
            Ok(child) if ctx.resolve_path(path).ok() != Some(child.erase()) => {
                ctx.send_to(child, &RetireWindow);
                self.rollback_attached_create(
                    ctx,
                    path,
                    &mut pending,
                    format!("spawned window child {child:?} is not the window at {path}"),
                )
            }
            Ok(child) => {
                let monitor = ctx.monitor(*child);
                self.promote_attached_window(ctx, path, *child, monitor, &mut pending)
            }
        };
        self.pending_host_effects.extend(effects);
    }

    fn promote_attached_window<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        path: &ErasedActorPath,
        child: ActorRef<WindowInstance>,
        monitor: ActorMonitorHandle,
        pending: &mut PendingCreate,
    ) -> Vec<WindowHostEffect> {
        let Some(state) = self.windows.get_mut(path) else {
            let error = format!("window {path} disappeared during attachment");
            ctx.send_to(child, &RetireWindow);
            return self.rollback_attached_create(ctx, path, pending, error);
        };
        self.children.insert(path.clone(), WindowChild { reference: child, _monitor: monitor });
        self.child_windows.insert(child.erase(), path.clone());
        state.lifecycle = DesktopWindowLifecycle::Live;
        state.commands = Some(child.narrow::<WindowCommands>());
        self.shutdown_when_idle = false;
        let info = state.info(path);
        if let Some(held) = pending.held.take() {
            held.answer(ctx, &CreateWindowResult::Ok { window: info.clone() });
        }
        self.publish(ctx, path, &WindowOpened { window: info.clone() });
        if info.width != 0 && info.height != 0 {
            self.publish(ctx, path, &self.window_size(path, info.width, info.height));
        }
        Vec::new()
    }

    fn rollback_attached_create<A, S, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, S, M>,
        path: &ErasedActorPath,
        pending: &mut PendingCreate,
        error: String,
    ) -> Vec<WindowHostEffect> {
        self.remove_window(path);
        if let Some(held) = pending.held.take() {
            held.answer(ctx, &CreateWindowResult::Err { error });
        }
        let mut effects = vec![WindowHostEffect::Closing { path: path.clone() }];
        effects.extend(self.failed_create_effects(pending.shutdown_on_failure));
        effects
    }

    /// Fail a queued create before a native window could be staged.
    pub fn fail_window_creation<A, S, M: ReplyMode>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, S, M>,
        path: &ErasedActorPath,
        error: String,
    ) -> Vec<WindowHostEffect> {
        let Some(pending) = self.pending_creates.remove(path) else {
            return Vec::new();
        };
        if let Some(held) = pending.held {
            held.answer(ctx, &CreateWindowResult::Err { error });
        }
        self.failed_create_effects(pending.shutdown_on_failure)
    }

    /// Finish a close after the integration detached native resources.
    pub fn finish_window_close<A>(
        &mut self,
        path: &ErasedActorPath,
        ctx: &mut NativeCtx<'_, A, Anyone, Single>,
    ) -> Vec<WindowHostEffect> {
        let close_held = self.windows.get_mut(path).and_then(|state| state.close_held.take());
        let existed = self.remove_window(path);
        if !existed {
            if let Some(held) = close_held {
                held.answer(
                    ctx,
                    &ApplyWindowCommandResult::Close(CloseWindowResult::Err {
                        error: format!("unknown window {path}"),
                    }),
                );
            }
            return Vec::new();
        }
        // A close with no caller retires the child through its stored
        // reference; a child whose own departure notice already removed its
        // entry has nothing left to retire.
        if let Some(held) = close_held {
            held.answer(ctx, &ApplyWindowCommandResult::Close(CloseWindowResult::Ok));
        } else if let Some(child) = self.children.get(path) {
            ctx.send_to(child.reference, &RetireWindow);
        }
        self.publish(ctx, path, &WindowClosed { window: path.clone() });
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

    /// Finish a presentation change with what the integration's surface
    /// answered: record the new value and answer `Ok`, or answer `Err` with
    /// the surface's message and leave the stored value, so
    /// `aether.window.list` keeps reporting the presentation still in force.
    pub fn finish_window_presentation<A>(
        &mut self,
        path: &ErasedActorPath,
        presentation: WindowPresentation,
        outcome: Result<(), String>,
        ctx: &mut NativeCtx<'_, A, Anyone, Single>,
    ) {
        let Some(held) = self.presentation_helds.pop_front() else {
            return;
        };
        let reply = match (outcome, self.windows.get_mut(path)) {
            (Ok(()), Some(state)) => {
                state.presentation = presentation;
                SetWindowPresentationResult::Ok { presentation }
            }
            (Ok(()), None) => SetWindowPresentationResult::Err { error: format!("unknown window {path}") },
            (Err(error), _) => SetWindowPresentationResult::Err { error },
        };
        held.answer(ctx, &ApplyWindowCommandResult::SetPresentation(reply));
    }

    /// The synchronous half of the per-window command family: resolve the
    /// native window once, then apply. A command naming a window that is gone
    /// or not yet live comes back as that command's own `Err` rather than a
    /// generic one, because the forwarding child matches the reply variant
    /// against the request it retained.
    fn apply_at_window(&mut self, path: &ErasedActorPath, command: WindowCommand) -> ApplyWindowCommandResult {
        let window = match self.live_window(path) {
            Ok(window) => window,
            Err(error) => return command.refused(error),
        };
        match command {
            // Answered from the close queue, never here.
            WindowCommand::Close => command.refused(format!("close for {path} did not reach the close queue")),
            // Answered once the integration has asked its surface, never here.
            WindowCommand::SetPresentation { .. } => {
                command.refused(format!("presentation change for {path} did not reach the host-action queue"))
            }
            WindowCommand::SetMode { mode, width, height } => self.apply_mode(path, &window, mode, width, height),
            WindowCommand::SetTitle { title } => {
                window.set_title(&title);
                if let Some(state) = self.windows.get_mut(path) {
                    state.title.clone_from(&title);
                }
                ApplyWindowCommandResult::SetTitle(SetWindowTitleResult::Ok { title })
            }
            WindowCommand::SetMenu { menus } => {
                ApplyWindowCommandResult::SetMenu(match apply_menu(&self.app_name, &window, path, &menus) {
                    Ok(()) => SetWindowMenuResult::Ok,
                    Err(error) => SetWindowMenuResult::Err { error },
                })
            }
            WindowCommand::SetCursor { icon } => {
                window.set_cursor(map_cursor_icon(icon));
                ApplyWindowCommandResult::SetCursor(SetWindowCursorResult::Ok)
            }
            WindowCommand::Focus => {
                window.set_minimized(false);
                window.set_visible(true);
                activate_application();
                window.focus_window();
                ApplyWindowCommandResult::Focus(FocusWindowResult::Ok)
            }
            WindowCommand::RequestRedraw => {
                window.request_redraw();
                ApplyWindowCommandResult::RequestRedraw(RequestWindowRedrawResult::Ok)
            }
        }
    }

    /// The one command whose reply is not the value the caller asked for: an
    /// OS may clamp or refuse a size, so the reply reports the size winit
    /// resolved rather than the one requested.
    fn apply_mode(
        &mut self,
        path: &ErasedActorPath,
        window: &Window,
        mode: WindowMode,
        width: Option<u32>,
        height: Option<u32>,
    ) -> ApplyWindowCommandResult {
        let fullscreen = match resolve_fullscreen(&mode, window.current_monitor().as_ref()) {
            Ok(fullscreen) => fullscreen,
            Err(error) => return ApplyWindowCommandResult::SetMode(SetWindowModeResult::Err { error }),
        };
        window.set_fullscreen(fullscreen);
        if matches!(mode, WindowMode::Windowed)
            && let (Some(width), Some(height)) = (width, height)
        {
            let _ = window.request_inner_size(PhysicalSize::new(width, height));
        }
        window.request_redraw();

        let size = window.inner_size();
        if let Some(state) = self.windows.get_mut(path) {
            state.mode = mode.clone();
            state.width = size.width;
            state.height = size.height;
        }
        ApplyWindowCommandResult::SetMode(SetWindowModeResult::Ok { mode, width: size.width, height: size.height })
    }

    /// Publish one muda menu activation as [`WindowMenuActivated`], to the
    /// same selector-aware subscribers every other window-originated kind
    /// reaches.
    ///
    /// muda's channel is process-wide, so an id this manager never minted —
    /// a predefined Quit, another library's menu — resolves to no window and
    /// is dropped rather than attributed to whichever window happens to parse
    /// out of it.
    pub fn menu_activated<A>(&mut self, raw: &str, ctx: &mut NativeCtx<'_, A, Anyone, Single>) {
        let Some((window, item)) = parse_menu_item_id(raw) else {
            return;
        };
        if self.windows.get(&window).is_none_or(|state| state.lifecycle != DesktopWindowLifecycle::Live) {
            return;
        }
        self.publish(ctx, &window, &WindowMenuActivated { window: window.clone(), id: item });
    }

    /// Translate one native window event and publish typed input directly to
    /// selector-aware subscribers.
    #[allow(clippy::too_many_lines)]
    pub fn window_event<A>(
        &mut self,
        winit_id: WinitWindowId,
        event: WindowEvent,
        ctx: &mut NativeCtx<'_, A, Anyone, Single>,
    ) {
        let Some(path) = self.winit_windows.get(&winit_id).cloned() else {
            return;
        };
        if self.windows.get(&path).is_none_or(|state| state.lifecycle != DesktopWindowLifecycle::Live) {
            return;
        }

        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                let _ = self.queue_close(&path, None);
            }
            WindowEvent::Resized(size) => {
                let mut occlusion = None;
                if let Some(state) = self.windows.get_mut(&path) {
                    state.width = size.width;
                    state.height = size.height;
                    let next = size.width == 0 || size.height == 0;
                    if state.occluded != next {
                        state.occluded = next;
                        occlusion = Some(next);
                    }
                }
                if let Some(occluded) = occlusion {
                    self.pending_host_effects.push(WindowHostEffect::Occluded { path: path.clone(), occluded });
                }
                if size.width != 0 && size.height != 0 {
                    self.publish(ctx, &path, &self.window_size(&path, size.width, size.height));
                    if let Some(window) = self.native_windows.get(&path) {
                        window.request_redraw();
                    }
                }
            }
            // Dragging a window between displays of different density
            // changes the logical-to-physical ratio without necessarily
            // changing the physical size, so the cached `WindowSize` a
            // subscriber holds would otherwise keep the stale factor.
            // winit follows this with its own `Resized`; republishing here
            // costs one duplicate mail and closes the gap when it doesn't.
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let Some(state) = self.windows.get_mut(&path) else {
                    return;
                };
                state.scale_factor = narrow_scale_factor(scale_factor);
                let (width, height) = (state.width, state.height);

                if width != 0 && height != 0 {
                    self.publish(ctx, &path, &self.window_size(&path, width, height));
                }
            }
            WindowEvent::Occluded(occluded) => {
                if let Some(state) = self.windows.get_mut(&path)
                    && state.occluded != occluded
                {
                    state.occluded = occluded;
                    self.pending_host_effects.push(WindowHostEffect::Occluded { path: path.clone(), occluded });
                }
            }
            WindowEvent::Focused(focused) => {
                if let Some(state) = self.windows.get_mut(&path)
                    && state.focused != focused
                {
                    state.focused = focused;
                    self.publish(ctx, &path, &WindowFocus { window: path.clone(), focused });
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(window) = self.native_windows.get(&path) {
                    let size = window.inner_size();
                    if let Some(state) = self.windows.get_mut(&path) {
                        state.width = size.width;
                        state.height = size.height;
                    }
                    if size.width != 0 && size.height != 0 {
                        self.publish(ctx, &path, &self.window_size(&path, size.width, size.height));
                    }
                }
                self.pending_host_effects.push(WindowHostEffect::Dirty { path: path.clone() });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let committed = if event.state == ElementState::Pressed {
                    event.text.as_ref().and_then(|text| {
                        self.windows.get_mut(&path).and_then(|state| {
                            text_input_gate(&mut state.composing, TextSource::KeyText(text.to_string()))
                        })
                    })
                } else {
                    None
                };
                if let Some(text) = committed {
                    self.publish(ctx, &path, &TextInput { window: path.clone(), text });
                }
                if let Some(code) = match event.physical_key {
                    PhysicalKey::Code(code) => map_winit_keycode(code),
                    PhysicalKey::Unidentified(_) => None,
                } {
                    match key_edge(event.state, event.repeat) {
                        Some(KeyEdge::Press) => self.publish(ctx, &path, &Key { window: path.clone(), code }),
                        Some(KeyEdge::Release) => self.publish(ctx, &path, &KeyRelease { window: path.clone(), code }),
                        None => {}
                    }
                }
            }
            WindowEvent::Ime(ime) => match ime {
                Ime::Preedit(text, cursor) => {
                    if let Some(state) = self.windows.get_mut(&path) {
                        text_input_gate(&mut state.composing, TextSource::Preedit { active: !text.is_empty() });
                    }
                    let (cursor_begin, cursor_end) = ime_cursor_span(cursor);
                    self.publish(ctx, &path, &ImePreedit { window: path.clone(), text, cursor_begin, cursor_end });
                }
                Ime::Commit(text) => {
                    let committed = self
                        .windows
                        .get_mut(&path)
                        .and_then(|state| text_input_gate(&mut state.composing, TextSource::Commit(text)));
                    if let Some(text) = committed {
                        self.publish(ctx, &path, &TextInput { window: path.clone(), text });
                    }
                }
                Ime::Disabled => {
                    if let Some(state) = self.windows.get_mut(&path) {
                        text_input_gate(&mut state.composing, TextSource::Disabled);
                    }
                }
                Ime::Enabled => {}
            },
            WindowEvent::ModifiersChanged(modifiers) => {
                let state = modifiers.state();
                let modifiers = Modifiers {
                    window: path.clone(),
                    shift: state.shift_key(),
                    ctrl: state.control_key(),
                    alt: state.alt_key(),
                    meta: state.super_key(),
                };
                self.publish(ctx, &path, &modifiers);
                if let Some(window) = self.windows.get_mut(&path) {
                    window.modifiers = modifiers;
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(button) = map_mouse_button(button) {
                    let (x, y) = self.windows.get(&path).map_or((0.0, 0.0), |window| window.cursor);
                    match state {
                        ElementState::Pressed => {
                            self.publish(ctx, &path, &MouseButton { window: path.clone(), button, x, y });
                        }
                        ElementState::Released => {
                            self.publish(ctx, &path, &MouseButtonRelease { window: path.clone(), button, x, y });
                        }
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (delta_x, delta_y) = normalize_wheel(delta);
                let (x, y) = self.windows.get(&path).map_or((0.0, 0.0), |window| window.cursor);
                self.publish(ctx, &path, &MouseWheel { window: path.clone(), delta_x, delta_y, x, y });
            }
            WindowEvent::CursorMoved { position, .. } => {
                #[allow(clippy::cast_possible_truncation)]
                let (x, y) = (position.x as f32, position.y as f32);
                if let Some(state) = self.windows.get_mut(&path) {
                    state.cursor = (x, y);
                }
                self.publish(ctx, &path, &MouseMove { window: path.clone(), x, y });
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
        held: Option<Held<CreateWindowResult>>,
        shutdown_on_failure: bool,
    ) -> Result<ErasedActorPath, (String, Option<Held<CreateWindowResult>>)> {
        let path = match crate::window_name(&spec.name) {
            Ok(name) => crate::window_path(&name),
            Err(error) => return Err((error, held)),
        };
        if self.pending_creates.values().any(|pending| pending.spec.name == spec.name)
            || self.windows.values().any(|window| window.name == spec.name)
        {
            return Err((format!("window name `{}` is already in use", spec.name), held));
        }
        self.pending_host_actions.push_back(WindowHostAction::Create { path: path.clone(), spec: spec.clone() });
        self.pending_creates.insert(path.clone(), PendingCreate { spec, held, shutdown_on_failure });
        Ok(path)
    }

    fn queue_close(
        &mut self,
        path: &ErasedActorPath,
        held: Option<Held<ApplyWindowCommandResult>>,
    ) -> Result<(), (String, Option<Held<ApplyWindowCommandResult>>)> {
        let Some(state) = self.windows.get_mut(path) else {
            return Err((format!("unknown window {path}"), held));
        };
        if state.lifecycle != DesktopWindowLifecycle::Live {
            return Err((format!("window {path} is not live"), held));
        }
        state.lifecycle = DesktopWindowLifecycle::Closing;
        state.close_held = held;
        self.pending_host_actions.push_back(WindowHostAction::Close { path: path.clone() });
        Ok(())
    }

    /// Queue a presentation change for the application's next host turn,
    /// keeping `held` until [`Self::finish_window_presentation`] answers it.
    fn queue_presentation(
        &mut self,
        path: &ErasedActorPath,
        presentation: WindowPresentation,
        held: Held<ApplyWindowCommandResult>,
    ) -> Result<(), (String, Held<ApplyWindowCommandResult>)> {
        if let Err(error) = self.live_state(path) {
            return Err((error, held));
        }
        self.presentation_helds.push_back(held);
        self.pending_host_actions.push_back(WindowHostAction::SetPresentation { path: path.clone(), presentation });
        Ok(())
    }

    fn remove_window(&mut self, path: &ErasedActorPath) -> bool {
        if let Some(window) = self.native_windows.remove(path) {
            self.winit_windows.remove(&window.id());
        } else {
            self.winit_windows.retain(|_, mapped| mapped != path);
        }
        self.windows.remove(path).is_some()
    }

    /// The state of the window at `path`, when it is live.
    fn live_state(&self, path: &ErasedActorPath) -> Result<&DesktopWindowState, String> {
        match self.windows.get(path) {
            None => Err(format!("unknown window {path}")),
            Some(window) if window.lifecycle != DesktopWindowLifecycle::Live => {
                Err(format!("window {path} is not live"))
            }
            Some(window) => Ok(window),
        }
    }

    fn live_window(&self, path: &ErasedActorPath) -> Result<Arc<Window>, String> {
        self.live_state(path)?;
        self.native_windows.get(path).cloned().ok_or_else(|| format!("window {path} has no native handle"))
    }

    /// The `WindowSize` event for a physical size, carrying the scale
    /// factor the window last reported. A window that has already left the
    /// map reports `1.0` rather than suppressing the publish, so the two
    /// pixel spaces still coincide for whoever reads it.
    fn window_size(&self, path: &ErasedActorPath, width: u32, height: u32) -> WindowSize {
        let scale_factor = self.windows.get(path).map_or(1.0, |state| state.scale_factor);
        WindowSize { window: path.clone(), width, height, scale_factor }
    }

    fn publish<K: Routed, A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A, Anyone, Single>,
        window: &ErasedActorPath,
        event: &K,
    ) {
        self.subscribers.publish(ctx, window, event);
    }
}

/// winit reports the scale factor as `f64`; `WindowSize` carries `f32`,
/// the width every other coordinate in the input vocabulary uses.
/// `dpi::Pixel::from_f64` is the narrowing winit itself applies to every
/// dpi quantity it hands back as `f32` (`to_logical::<f32>`,
/// `LogicalPosition<f32>`), so this stays the library's own conversion
/// rather than a second, hand-rolled one.
fn narrow_scale_factor(scale_factor: f64) -> f32 {
    <f32 as Pixel>::from_f64(scale_factor)
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
    use std::fmt::Debug;

    use aether_data::{ErasedActorPath, Kind, SessionToken, Uuid};
    use aether_kinds::mouse_button;
    use aether_substrate::ReplyTarget;
    use aether_substrate::actor::native::SpawnError;
    use aether_substrate::testing::{boot_bare_test_chassis, decode_session_reply, fresh_substrate_and_rx};

    use super::*;
    use crate::runtime::WindowBackend;
    use crate::runtime::subscribers::fixture::{Receipt, Rig, watcher};
    use crate::{CreateWindow, ListWindows, ListWindowsResult, SubscribeWindowResult, WindowSubscription};

    fn test_state() -> DesktopWindows {
        DesktopWindows::new(DesktopWindowBoot::for_test())
    }

    fn spec(name: &str, title: &str) -> WindowSpec {
        WindowSpec {
            name: name.to_owned(),
            title: title.to_owned(),
            mode: WindowMode::Windowed,
            size: None,
            presentation: WindowPresentation::Display,
        }
    }

    fn rig() -> Rig<WindowCapability> {
        Rig::desktop()
    }

    fn path(name: &str) -> ErasedActorPath {
        crate::window_path(&aether_data::LoadName::new(name).expect("fixture window name"))
    }

    /// Insert a live (or closing) window named `name`, answering its path.
    pub(in crate::runtime::desktop) fn insert_window(
        state: &mut DesktopWindows,
        name: &str,
        closing: bool,
    ) -> ErasedActorPath {
        let window = path(name);
        state.windows.insert(
            window.clone(),
            DesktopWindowState {
                name: name.to_owned(),
                title: format!("window-{name}"),
                mode: WindowMode::Windowed,
                width: 640,
                height: 480,
                scale_factor: 1.0,
                cursor: (0.0, 0.0),
                composing: false,
                modifiers: Modifiers { window: window.clone(), shift: false, ctrl: false, alt: false, meta: false },
                focused: false,
                occluded: false,
                presentation: WindowPresentation::Display,
                lifecycle: if closing {
                    DesktopWindowLifecycle::Closing
                } else {
                    DesktopWindowLifecycle::Live
                },
                commands: None,
                close_held: None,
            },
        );
        window
    }

    #[test]
    fn window_paths_name_the_named_children_and_actions_remain_ordered() {
        let mut state = test_state();
        assert!(state.queue_create(spec("first", "First"), None, false).is_ok());
        assert!(state.queue_create(spec("second", "Second"), None, false).is_ok());

        let (actions, _) = state.take_host_work();
        assert!(matches!(&actions[0], WindowHostAction::Create { path: created, .. } if *created == path("first")));
        assert!(matches!(&actions[1], WindowHostAction::Create { path: created, .. } if *created == path("second")));
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
        insert_window(&mut live, "tools", false);
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
        let main = insert_window(&mut state, "main", false);

        state.windows.get_mut(&main).expect("live window").title = "Renamed".to_owned();
        let info = state.windows[&main].info(&main);

        assert_eq!(info.name, "main");
        assert_eq!(info.title, "Renamed");
    }

    /// Fails if `aether.window.list` stops answering in window-path order.
    #[test]
    fn list_windows_is_sorted_by_window_path() {
        let mut rig = rig();
        rig.desktop_turn(|state, _ctx| {
            insert_window(state, "two", false);
            insert_window(state, "nine", false);
        })
        .expect("the desktop manager is live");

        rig.send(&ListWindows);
        let ListWindowsResult::Ok { windows } = rig.reply() else {
            panic!("desktop manager list succeeds");
        };

        assert_eq!(windows.into_iter().map(|window| window.path).collect::<Vec<_>>(), [path("nine"), path("two")]);
    }

    /// A create is reserved by `aether.window.create`; the native window a
    /// winit callback would stage is placed directly, since the test has no
    /// event loop. It pins the two properties the staged path owes — a
    /// reserved child is not enumerable, and an authoritative rejection rolls
    /// the window back and answers the caller exactly once. Fails if an
    /// attaching window is listed, or if a failed birth leaks its window, its
    /// reservation, or its caller's reply.
    #[test]
    fn a_reserved_window_child_is_not_enumerable_and_rolls_back_when_its_birth_fails() {
        let mut rig = rig();
        rig.push(&CreateWindow { spec: spec("tools", "Tools") });
        rig.pump_desktop_until("the create's reservation", |state| state.pending_creates.contains_key(&path("tools")));
        let tools = rig
            .desktop_turn(|state, _ctx| {
                let tools = insert_window(state, "tools", false);
                state.windows.get_mut(&tools).expect("attaching window").lifecycle = DesktopWindowLifecycle::Attaching;
                tools
            })
            .expect("the desktop manager is live");

        rig.send(&ListWindows);
        let ListWindowsResult::Ok { windows } = rig.reply() else {
            panic!("desktop manager list succeeds");
        };
        assert!(windows.is_empty(), "a reserved window child is absent from live enumeration");

        rig.desktop_turn(|state, ctx| {
            state.finish_window_child_spawn(
                ctx,
                &tools,
                &SpawnOutcome::<WindowInstance> {
                    canonical_name: ErasedActorPath::new("aether.window/aether.window.instance:tools")
                        .expect("fixture is an actor path"),
                    result: Err(SpawnError::OwnerClosed),
                },
            );
        })
        .expect("the desktop manager is live");

        assert!(matches!(rig.reply(), CreateWindowResult::Err { .. }), "the caller is answered once, with the failure");
        rig.read_desktop(|state| {
            assert!(!state.windows.contains_key(&tools), "a rejected birth rolls its window back");
            assert!(state.pending_creates.is_empty(), "a rejected birth clears its reservation");
            assert!(
                state
                    .pending_host_effects
                    .iter()
                    .any(|effect| matches!(effect, WindowHostEffect::Closing { path: closing } if *closing == tools)),
                "rollback detaches the native window through the host-effect queue",
            );
        })
        .expect("the desktop manager is live");
    }

    /// The divergence guard runs when the birth completes: a Live child the
    /// reserved window's path does not prove is never promoted, and the
    /// caller's reply names the divergence. The child is placed flat at the
    /// chassis root under another name, so its reference is real but names a
    /// window nobody reserved.
    #[test]
    fn a_window_child_that_diverges_from_its_prediction_rolls_back() {
        let (registry, mailer, rx) = fresh_substrate_and_rx();
        let chassis = boot_bare_test_chassis(&registry, &mailer);
        let (mut slot, _wake) = chassis
            .boot_pumped_actor::<WindowCapability>((), crate::WindowParams::Desktop(DesktopWindowBoot::for_test()))
            .expect("the desktop manager boots pumped");
        let child = chassis
            .spawn_actor_for_test::<WindowInstance>(Subname::Named("elsewhere"), (), ())
            .finish()
            .expect("the stray window child spawns");

        chassis.send_for_reply(
            chassis.actor_ref::<WindowCapability>(),
            &CreateWindow { spec: spec("tools", "Tools") },
            ReplyTarget::Session { session: SessionToken(Uuid::from_u128(0)), correlation: 1 },
        );
        slot.drain_available();
        let tools = slot
            .host_turn(|state, ctx| {
                let WindowBackend::Desktop(state) = &mut state.backend else {
                    panic!("the manager runs the desktop backend");
                };
                let tools = insert_window(state, "tools", false);
                state.windows.get_mut(&tools).expect("attaching window").lifecycle = DesktopWindowLifecycle::Attaching;
                state.finish_window_child_spawn(
                    ctx,
                    &tools,
                    &SpawnOutcome::<WindowInstance> {
                        canonical_name: ErasedActorPath::new("aether.window/aether.window.instance:tools")
                            .expect("fixture is an actor path"),
                        result: Ok(child),
                    },
                );
                tools
            })
            .expect("the desktop manager is live");

        let CreateWindowResult::Err { error } = decode_session_reply::<CreateWindowResult>(&rx) else {
            panic!("a divergent child fails the create");
        };
        assert!(error.contains("is not the window at"), "the reply names the divergence: {error}");
        slot.host_turn(|state, _ctx| {
            let WindowBackend::Desktop(state) = &mut state.backend else {
                panic!("the manager runs the desktop backend");
            };
            assert!(state.children.is_empty(), "a divergent child is never supervised");
            assert!(!state.windows.contains_key(&tools), "a divergent child rolls its window back");
            assert!(state.pending_creates.is_empty(), "a divergent child clears its reservation");
            assert!(
                state
                    .pending_host_effects
                    .iter()
                    .any(|effect| matches!(effect, WindowHostEffect::Closing { path: closing } if *closing == tools)),
                "rollback detaches the native window through the host-effect queue",
            );
        })
        .expect("the desktop manager is live");
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

    /// Fails if detaching one of several windows asks the application to
    /// shut down.
    #[test]
    fn closing_one_window_does_not_request_global_shutdown() {
        let mut rig = rig();

        rig.desktop_turn(|state, ctx| {
            let first = insert_window(state, "first", true);
            let second = insert_window(state, "second", false);

            assert!(state.finish_window_close(&first, ctx).is_empty());
            assert!(state.windows.contains_key(&second));
        })
        .expect("the desktop manager is live");
    }

    #[test]
    fn closing_window_without_child_proof_stays_in_root_command_cardinality() {
        let mut state = test_state();
        let path = insert_window(&mut state, "closing", true);

        let windows = state.routable_windows();

        assert!(matches!(windows.as_slice(), [window] if window.path == path && window.target.is_none()));
    }

    /// Fails if the last window's detach does not ask for shutdown, or asks
    /// before the window has left the map.
    #[test]
    fn closing_the_last_window_requests_shutdown_after_removal() {
        let mut rig = rig();

        rig.desktop_turn(|state, ctx| {
            let first = insert_window(state, "first", true);

            assert!(matches!(state.finish_window_close(&first, ctx).as_slice(), [WindowHostEffect::LastWindowClosed]));
            assert!(state.windows.is_empty());
        })
        .expect("the desktop manager is live");
    }

    /// A create still in flight when the last window closes defers the
    /// shutdown to the create's outcome. Fails if the close shuts down under
    /// a pending replacement, or if the replacement's failure forgets the
    /// deferred shutdown or its caller's reply.
    #[test]
    fn pending_replacement_defers_last_window_shutdown_until_create_resolves() {
        let mut rig = rig();
        let first = rig.desktop_turn(|state, _ctx| insert_window(state, "first", true)).expect("the manager is live");
        rig.push(&CreateWindow { spec: spec("replacement", "Replacement") });
        rig.pump_desktop_until("the create's reservation", |state| {
            state.pending_creates.contains_key(&path("replacement"))
        });

        rig.desktop_turn(|state, ctx| {
            assert!(state.finish_window_close(&first, ctx).is_empty());
            assert!(state.shutdown_when_idle);
            let effects = state.fail_window_creation(ctx, &path("replacement"), "native create failed".to_owned());

            assert!(matches!(effects.as_slice(), [WindowHostEffect::LastWindowClosed]));
        })
        .expect("the desktop manager is live");
        assert!(matches!(rig.reply(), CreateWindowResult::Err { .. }), "the replacement's caller hears the failure");
    }

    /// Fails if a boot window whose native create fails leaves its
    /// reservation behind or leaves the application running with no window.
    #[test]
    fn failed_initial_create_rolls_back_and_requests_shutdown() {
        let mut rig = rig();

        rig.desktop_turn(|state, ctx| {
            state.queue_initial_window(spec("main", "boot")).expect("reserve boot window");
            let effects = state.fail_window_creation(ctx, &path("main"), "native create failed".to_owned());

            assert!(matches!(effects.as_slice(), [WindowHostEffect::LastWindowClosed]));
            assert!(state.pending_creates.is_empty());
        })
        .expect("the desktop manager is live");
    }

    /// Fails if a boot window whose render attachment fails stays staged, or
    /// the application keeps running with no window.
    #[test]
    fn failed_attachment_removes_the_staged_initial_window_before_shutdown() {
        let mut rig = rig();

        rig.desktop_turn(|state, ctx| {
            state.queue_initial_window(spec("main", "boot")).expect("reserve boot window");
            let main = insert_window(state, "main", false);
            state.windows.get_mut(&main).expect("staged window").lifecycle = DesktopWindowLifecycle::Attaching;
            let effects = state.finish_window_attachment(&main, Err("render attach failed".to_owned()), ctx);

            assert!(matches!(effects.as_slice(), [WindowHostEffect::LastWindowClosed]));
            assert!(!state.windows.contains_key(&main));
            assert!(state.pending_creates.is_empty());
        })
        .expect("the desktop manager is live");
    }

    /// A live window at a chosen display density, registered under winit's
    /// dummy id so [`DesktopWindows::window_event`] — winit
    /// event in, published kind out — can be driven without an event loop.
    fn insert_scaled_window(state: &mut DesktopWindows, scale_factor: f32) -> (ErasedActorPath, WinitWindowId) {
        let main = insert_window(state, "main", false);
        state.windows.get_mut(&main).expect("live window").scale_factor = scale_factor;
        let winit_id = WinitWindowId::dummy();
        state.winit_windows.insert(winit_id, main.clone());
        (main, winit_id)
    }

    /// The sole received publication of kind `K`, decoded.
    fn sole_published<K: Kind + PartialEq + Debug>(receipts: &[Receipt]) -> K {
        let mut matched = receipts.iter().filter_map(Receipt::event::<K>);
        let event = matched.next().unwrap_or_else(|| panic!("{} was published", K::NAME));
        assert!(matched.next().is_none(), "{} was published exactly once", K::NAME);
        event
    }

    // Tripwire: every pixel coordinate this translation publishes is a
    // *physical* pixel, forwarded from winit unconverted, and the display
    // density rides beside it on `WindowSize` rather than being folded into
    // any of it. The mouse kinds documented themselves as logical for months
    // while this arm cast winit's `PhysicalPosition` straight onto the wire,
    // and a consumer that believed the docs and multiplied by `scale_factor`
    // missed every hover target by exactly 2x on a 2x display
    // (iamacoffeepot/aether#5509) — the docs were corrected, but nothing
    // asserted the space, so the same drift could recur in either direction.
    //
    // The window here is at `2.0`, which is what makes the assertion bite: a
    // conversion inserted anywhere between the winit event and the published
    // kind moves the cursor's 400.0 to 200.0 or 800.0, and the resize's
    // 1280x960 to 640x480 or 2560x1920. At the `1.0` every other window
    // fixture uses, both spaces coincide and every such mistake passes.
    #[test]
    fn published_pixel_quantities_stay_in_winits_physical_space_on_a_scaled_display() {
        use winit::dpi::PhysicalPosition;
        use winit::event::{DeviceId, MouseButton as WinitMouseButton, MouseScrollDelta, TouchPhase};

        let mut rig = rig();
        rig.watcher("pixel-space");
        let pixel_space = watcher("pixel-space");
        for subscription in [
            WindowSubscription::MouseMove(pixel_space.narrow()),
            WindowSubscription::MouseButton(pixel_space.narrow()),
            WindowSubscription::MouseWheel(pixel_space.narrow()),
            WindowSubscription::WindowSize(pixel_space.narrow()),
        ] {
            assert!(matches!(rig.subscribe(crate::WindowSelector::All, subscription), SubscribeWindowResult::Ok));
        }

        let window = rig
            .desktop_turn(|state, ctx| {
                let (window, winit_id) = insert_scaled_window(state, 2.0);
                let device_id = DeviceId::dummy();
                state.window_event(
                    winit_id,
                    WindowEvent::CursorMoved { device_id, position: PhysicalPosition::new(400.0, 300.0) },
                    ctx,
                );
                state.window_event(
                    winit_id,
                    WindowEvent::MouseInput { device_id, state: ElementState::Pressed, button: WinitMouseButton::Left },
                    ctx,
                );
                state.window_event(
                    winit_id,
                    WindowEvent::MouseWheel {
                        device_id,
                        delta: MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, -120.0)),
                        phase: TouchPhase::Moved,
                    },
                    ctx,
                );
                state.window_event(winit_id, WindowEvent::Resized(PhysicalSize::new(1280, 960)), ctx);
                window
            })
            .expect("the desktop manager is live");

        let recorded = rig.receipts(4);

        assert_eq!(
            sole_published::<MouseMove>(&recorded),
            MouseMove { window: window.clone(), x: 400.0, y: 300.0 },
            "the cursor reaches the wire at winit's physical position, unscaled",
        );
        assert_eq!(
            sole_published::<MouseButton>(&recorded),
            MouseButton { window: window.clone(), button: mouse_button::LEFT, x: 400.0, y: 300.0 },
            "a click reports the same physical position the move published",
        );
        assert_eq!(
            sole_published::<MouseWheel>(&recorded),
            MouseWheel { window: window.clone(), delta_x: 0.0, delta_y: -120.0, x: 400.0, y: 300.0 },
            "a wheel event's pixel delta and cursor position share that space",
        );
        assert_eq!(
            sole_published::<WindowSize>(&recorded),
            WindowSize { window, width: 1280, height: 960, scale_factor: 2.0 },
            "the size is the physical one winit reported and the factor rides beside it",
        );
    }

    /// Fails if the listed row reports a constant factor instead of the window's
    /// own, which passes at the `1.0` every other window fixture uses.
    #[test]
    fn a_listed_window_reports_its_scale_factor() {
        let mut rig = rig();
        rig.desktop_turn(|state, _ctx| {
            insert_scaled_window(state, 2.0);
        })
        .expect("the desktop manager is live");

        rig.send(&ListWindows);
        let ListWindowsResult::Ok { windows } = rig.reply() else {
            panic!("desktop manager list succeeds");
        };

        assert_eq!(windows.into_iter().map(|window| window.scale_factor).collect::<Vec<_>>(), [2.0]);
    }

    /// Fails if the focus arm records the state without publishing it, or
    /// publishes every platform notification instead of each change.
    #[test]
    fn a_subscriber_hears_each_focus_change_once() {
        let mut rig = rig();
        rig.watcher("focus");
        let focus = watcher("focus");
        for subscription in
            [WindowSubscription::WindowFocus(focus.narrow()), WindowSubscription::WindowSize(focus.narrow())]
        {
            assert!(matches!(rig.subscribe(crate::WindowSelector::All, subscription), SubscribeWindowResult::Ok));
        }

        let window = rig
            .desktop_turn(|state, ctx| {
                let (window, winit_id) = insert_scaled_window(state, 1.0);
                state.window_event(winit_id, WindowEvent::Focused(true), ctx);
                state.window_event(winit_id, WindowEvent::Focused(false), ctx);
                state.window_event(winit_id, WindowEvent::Focused(false), ctx);
                state.window_event(winit_id, WindowEvent::Resized(PhysicalSize::new(800, 600)), ctx);
                window
            })
            .expect("the desktop manager is live");

        let recorded = rig.receipts(3);

        assert_eq!(recorded[0].event::<WindowFocus>(), Some(WindowFocus { window: window.clone(), focused: true }));
        assert_eq!(recorded[1].event::<WindowFocus>(), Some(WindowFocus { window, focused: false }));
        assert!(recorded[2].event::<WindowSize>().is_some(), "the repeated loss published nothing before the resize");
    }

    /// Fails if the desktop backend fans its events out without the shared
    /// publish, so a held key focus slot narrows injected events and not the
    /// ones winit raises. The trailing resize, which goes to everyone, is the
    /// settled marker: each watcher's receipts arrive in the order they were
    /// sent, so an outsider that was sent the text reports it before its
    /// resize.
    #[test]
    fn a_held_slot_narrows_the_text_winit_raises() {
        let mut rig = rig();
        let holder = rig.watcher("holder");
        rig.watcher("outsider");
        for name in ["holder", "outsider"] {
            for subscription in [
                WindowSubscription::TextInput(watcher(name).narrow()),
                WindowSubscription::WindowSize(watcher(name).narrow()),
            ] {
                assert!(matches!(rig.subscribe(crate::WindowSelector::All, subscription), SubscribeWindowResult::Ok));
            }
        }
        let (window, winit_id) =
            rig.desktop_turn(|state, _ctx| insert_scaled_window(state, 1.0)).expect("the desktop manager is live");
        rig.take(holder, &window, crate::KeyFocusScope::Actor);

        rig.desktop_turn(|state, ctx| {
            state.window_event(winit_id, WindowEvent::Ime(Ime::Commit("a".to_owned())), ctx);
            state.window_event(winit_id, WindowEvent::Resized(PhysicalSize::new(800, 600)), ctx);
        })
        .expect("the desktop manager is live");

        let recorded = rig.receipts(3);
        assert_eq!(received::<TextInput>(&recorded), ["holder"]);
        assert_eq!(received::<WindowSize>(&recorded), ["holder", "outsider"]);
    }

    /// The keys of the watchers that received a `K` among `receipts`, sorted.
    fn received<K: Kind>(receipts: &[Receipt]) -> Vec<&str> {
        let mut watchers = receipts
            .iter()
            .filter(|receipt| receipt.event::<K>().is_some())
            .map(|receipt| receipt.watcher.as_str())
            .collect::<Vec<_>>();
        watchers.sort_unstable();
        watchers
    }
}
