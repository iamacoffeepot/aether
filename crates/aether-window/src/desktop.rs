//! The `aether.window` desktop runtime (ADR-0160 §Decision 2): the
//! state-bearing companion to [`HeadlessWindowCapability`](super::HeadlessWindowCapability),
//! co-located per the ADR-0121/ADR-0122 reading that one crate owns every
//! runtime of one mailbox. Gated behind the `desktop` feature (`runtime` +
//! winit), so headless and substrate-harness builds — which never touch a
//! window peripheral — pay nothing.
//!
//! Unlike the headless companion (which `Err`-replies off `ctx` alone),
//! this runtime mutates a real winit [`Window`]. The window is created by
//! the desktop chassis's winit `resumed` handler on its own schedule, so
//! the handle is shared through a [`WindowCell`] — a one-shot
//! `Arc<OnceLock<Arc<Window>>>` the chassis fills exactly once and the
//! actor reads. Until the cell is filled, every handler replies `Err`
//! (the same "no window yet" contract the headless companion and the
//! pre-actor driver drain both hold).
//!
//! The identity/runtime split is impl-hosted here rather than struct-hosted
//! (the [`HeadlessWindowCapability`](super::HeadlessWindowCapability) shape):
//! the `desktop` feature always implies `runtime`, so there is no
//! marker-only build of this cap to protect, and a single-file impl carrying
//! `type State = DesktopWindowCapabilityState` keeps the whole desktop
//! runtime in one place.

use std::sync::{Arc, OnceLock};

use aether_actor::actor;
use aether_kinds::WindowMode;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use winit::dpi::PhysicalSize;
use winit::monitor::{MonitorHandle, VideoModeHandle};
use winit::window::{Fullscreen, Window};

use super::{
    CloseWindow, CloseWindowResult, CreateWindow, CreateWindowResult, FocusWindow, FocusWindowResult, ListWindows,
    ListWindowsResult, RequestWindowRedraw, RequestWindowRedrawResult, SetWindowMode, SetWindowModeResult,
    SetWindowTitle, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult, SubscribeWindowSelf,
    UnsubscribeAllWindows, UnsubscribeWindow, UnsubscribeWindowSelf, WindowId,
};

/// The late-bound winit window handle, shared between the desktop chassis's
/// winit `resumed` handler (which fills it exactly once after
/// `create_window`) and the actor state (which reads it in every handler).
/// A one-shot `OnceLock` behind an `Arc` so both sides hold
/// the same cell; winit 0.30's `Window` is `Send + Sync`, so the inner
/// `Arc<Window>` crosses the fill/read boundary freely.
pub type WindowCell = Arc<OnceLock<Arc<Window>>>;

/// Composer-supplied construction input for [`DesktopWindowCapability`]
/// (ADR-0156 §1): the shared [`WindowCell`] the chassis fills post-boot and
/// the window's initial mode (mirroring the boot `AETHER_WINDOW_MODE`). Not
/// a config section — the cell is a runtime handle the chassis mints at
/// Start, not an env/argv-resolved value.
pub struct DesktopWindowParams {
    /// Transitional engine identity for the chassis-owned boot window.
    pub id: WindowId,
    /// The one-shot window handle the winit `resumed` handler fills.
    pub window: WindowCell,
    /// The mode the window boots into; seeds the actor state's current mode.
    pub initial_mode: WindowMode,
}

/// `aether.window` desktop runtime state (ADR-0122 split). Holds the shared
/// [`WindowCell`] every handler reads and the currently-applied
/// [`WindowMode`]. The addressing identity is the distinct ZST
/// [`DesktopWindowCapability`].
pub struct DesktopWindowCapabilityState {
    /// Engine-owned identity of the transitional boot window.
    id: WindowId,
    /// The late-bound window handle. `None` (`get()` empty) until the
    /// chassis's `resumed` fills the cell; handlers reply `Err` until then.
    window: WindowCell,
    /// Currently-applied window mode, updated by `on_set_mode`. Carried on
    /// the state as the natural home for a future `get_mode` query.
    current_mode: WindowMode,
}

fn unknown_window(requested: WindowId, available: WindowId) -> String {
    format!("unknown window {requested:?}; transitional desktop bridge owns only {available:?}")
}

fn manager_pending() -> String {
    "multi-window manager is not installed yet".to_owned()
}

/// `aether.window` desktop-runtime cap **identity** (ADR-0122 identity/runtime
/// split). The state-bearing companion to
/// [`HeadlessWindowCapability`](super::HeadlessWindowCapability), claiming the
/// same `aether.window` mailbox on the desktop chassis — its handlers mutate a
/// real winit [`Window`] (fullscreen mode, title, focus) instead of
/// `Err`-replying.
///
/// Each chassis composes one of {this cap, the headless companion}, never
/// both — the chassis builder rejects double-claiming a mailbox. The two
/// identities legitimately share `NAMESPACE = "aether.window"`; the
/// link-time handler inventory is deduped on read (`aether.inventory`'s
/// `on_handlers`) so a desktop binary linking both doesn't double-report
/// each window handler.
pub struct DesktopWindowCapability;

#[actor(singleton)]
impl NativeActor for DesktopWindowCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// shared [`WindowCell`] plus the currently-applied mode.
    type State = DesktopWindowCapabilityState;

    type Config = ();
    type Params = DesktopWindowParams;

    const NAMESPACE: &'static str = "aether.window";

    fn init(
        (): (),
        params: DesktopWindowParams,
        _ctx: &mut NativeInitCtx<'_>,
    ) -> Result<DesktopWindowCapabilityState, BootError> {
        Ok(DesktopWindowCapabilityState { id: params.id, window: params.window, current_mode: params.initial_mode })
    }

    #[handler::single]
    fn on_list(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListWindows) -> ListWindowsResult {
        ListWindowsResult::Err { error: manager_pending() }
    }

    #[handler::single]
    fn on_create(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: CreateWindow) -> CreateWindowResult {
        CreateWindowResult::Err { error: manager_pending() }
    }

    #[handler::single]
    fn on_close(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: CloseWindow) -> CloseWindowResult {
        CloseWindowResult::Err { window: mail.window, error: manager_pending() }
    }

    /// Apply a window mode (windowed / fullscreen-borderless /
    /// fullscreen-exclusive) and, for windowed, an optional inner size.
    /// `Err` if the window isn't created yet (mail arrived before the
    /// chassis's `resumed` filled the cell) — the "no window yet" contract
    /// both runtimes hold.
    #[handler::single]
    fn on_set_mode(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: SetWindowMode) -> SetWindowModeResult {
        if mail.window != state.id {
            return SetWindowModeResult::Err { window: mail.window, error: unknown_window(mail.window, state.id) };
        }
        let Some(window) = state.window.get().cloned() else {
            return SetWindowModeResult::Err {
                window: mail.window,
                error: "set_window_mode requested before window initialized".to_owned(),
            };
        };
        let monitor = window.current_monitor();
        let fullscreen = match resolve_fullscreen(&mail.mode, monitor.as_ref()) {
            Ok(fs) => fs,
            Err(error) => return SetWindowModeResult::Err { window: mail.window, error },
        };
        window.set_fullscreen(fullscreen);
        if matches!(mail.mode, WindowMode::Windowed)
            && let (Some(w), Some(h)) = (mail.width, mail.height)
        {
            let _ = window.request_inner_size(PhysicalSize::new(w, h));
        }

        state.current_mode = mail.mode.clone();
        let size = window.inner_size();
        SetWindowModeResult::Ok { window: mail.window, mode: mail.mode, width: size.width, height: size.height }
    }

    /// Set the window title. `Err` if the window isn't created yet.
    #[handler::single]
    fn on_set_title(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: SetWindowTitle) -> SetWindowTitleResult {
        if mail.window != state.id {
            return SetWindowTitleResult::Err { window: mail.window, error: unknown_window(mail.window, state.id) };
        }
        let Some(window) = state.window.get() else {
            return SetWindowTitleResult::Err {
                window: mail.window,
                error: "set_window_title requested before window initialized".to_owned(),
            };
        };
        window.set_title(&mail.title);
        SetWindowTitleResult::Ok { window: mail.window, title: mail.title }
    }

    /// Bring the window to the foreground (iamacoffeepot/aether#1318):
    /// un-minimize, show if hidden, then raise + focus. winit's
    /// `focus_window` is best-effort per platform, but the three calls are
    /// the full lever the substrate has. `Err` if the window isn't created
    /// yet (mail arrived before the chassis's `resumed` filled the cell).
    #[handler::single]
    fn on_focus(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: FocusWindow) -> FocusWindowResult {
        if mail.window != state.id {
            return FocusWindowResult::Err { window: mail.window, error: unknown_window(mail.window, state.id) };
        }
        let Some(window) = state.window.get() else {
            return FocusWindowResult::Err {
                window: mail.window,
                error: "focus requested before window initialized".to_owned(),
            };
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
        if mail.window != state.id {
            return RequestWindowRedrawResult::Err {
                window: mail.window,
                error: unknown_window(mail.window, state.id),
            };
        }
        let Some(window) = state.window.get() else {
            return RequestWindowRedrawResult::Err {
                window: mail.window,
                error: "redraw requested before window initialized".to_owned(),
            };
        };
        window.request_redraw();
        RequestWindowRedrawResult::Ok { window: mail.window }
    }

    #[handler::single]
    fn on_subscribe(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: SubscribeWindow,
    ) -> SubscribeWindowResult {
        SubscribeWindowResult::Err { error: manager_pending() }
    }

    #[handler::single]
    fn on_subscribe_self(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: SubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        SubscribeWindowResult::Err { error: manager_pending() }
    }

    #[handler::single]
    fn on_unsubscribe(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: UnsubscribeWindow,
    ) -> SubscribeWindowResult {
        SubscribeWindowResult::Err { error: manager_pending() }
    }

    #[handler::single]
    fn on_unsubscribe_self(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: UnsubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        SubscribeWindowResult::Err { error: manager_pending() }
    }

    #[handler::single]
    fn on_unsubscribe_all(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: UnsubscribeAllWindows) {}
}

/// Find a `VideoModeHandle` on `monitor` matching the given size + refresh
/// exactly. Returns `None` if no match — the caller surfaces this as
/// `SetWindowModeResult::Err` rather than falling back silently to something
/// close.
fn find_exclusive_mode(monitor: &MonitorHandle, width: u32, height: u32, refresh_mhz: u32) -> Option<VideoModeHandle> {
    monitor
        .video_modes()
        .find(|m| m.size().width == width && m.size().height == height && m.refresh_rate_millihertz() == refresh_mhz)
}

/// Build winit's `Option<Fullscreen>` for the requested mode.
/// `monitor_for_exclusive` is the monitor to match video modes against — the
/// window's current monitor at runtime, the primary at boot.
///
/// Window semantics owned by the cap crate (ADR-0121/ADR-0122): both the
/// desktop chassis's boot-time window creation and this runtime's
/// `on_set_mode` resolve fullscreen through this one function.
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
