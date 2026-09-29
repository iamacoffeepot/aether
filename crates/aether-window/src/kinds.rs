//! Public wire vocabulary for the `aether.window` manager.

use aether_actor::{HeldReply, ProtocolPath, Subscriber};
use aether_data::{ErasedActorPath, KindId};
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
    WindowMode, WindowSize,
};
use serde::{Deserialize, Serialize};

/// Select one window, by its canonical actor path, or every current and
/// future window.
///
/// `All` is prospective: a subscription using it also observes matching
/// events from windows created after the subscription is installed.
#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum WindowSelector {
    One(ErasedActorPath),
    All,
}

/// Optional physical-pixel size requested for a windowed window.
#[derive(aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
pub struct WindowSizeRequest {
    pub width: u32,
    pub height: u32,
}

/// Creation specification for one window.
#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WindowSpec {
    pub name: String,
    pub title: String,
    pub mode: WindowMode,
    pub size: Option<WindowSizeRequest>,
}

/// Public state for one live window. `path` is the window's canonical actor
/// path (`aether.window/aether.window.instance:main`), the text every
/// window-originated event carries and `capture_frame` takes.
#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub path: ErasedActorPath,
    pub name: String,
    pub title: String,
    pub mode: WindowMode,
    pub width: u32,
    pub height: u32,
    pub focused: bool,
    pub occluded: bool,
}

/// List every live window in ascending path order, which is ascending window
/// name.
#[aether_data::kind(name = "aether.window.list", copy, default, eq)]
pub struct ListWindows;

/// Reply to [`ListWindows`].
#[aether_data::kind(name = "aether.window.list_result", eq)]
pub enum ListWindowsResult {
    Ok { windows: Vec<WindowInfo> },
    Err { error: String },
}

/// Create a window from an explicit specification.
#[aether_data::kind(name = "aether.window.create", eq)]
pub struct CreateWindow {
    pub spec: WindowSpec,
}

/// Reply to [`CreateWindow`].
#[aether_data::kind(name = "aether.window.create_result", eq)]
pub enum CreateWindowResult {
    Ok { window: WindowInfo },
    Err { error: String },
}

impl HeldReply for CreateWindowResult {
    fn unanswered() -> Self {
        Self::Err { error: "window manager closed before answering".into() }
    }
}

/// Begin closing the addressed window.
#[aether_data::kind(name = "aether.window.close", copy, eq)]
pub struct CloseWindow;

/// Reply to [`CloseWindow`].
#[aether_data::kind(name = "aether.window.close_result", eq)]
pub enum CloseWindowResult {
    Ok,
    Err { error: String },
}

impl HeldReply for CloseWindowResult {
    fn unanswered() -> Self {
        Self::Err { error: "window endpoint closed before answering".into() }
    }
}

/// Change one window's presentation mode.
///
/// `width` and `height` apply only to [`WindowMode::Windowed`].
#[aether_data::kind(name = "aether.window.set_mode", eq)]
pub struct SetWindowMode {
    pub mode: WindowMode,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Reply to [`SetWindowMode`] with the resolved state.
#[aether_data::kind(name = "aether.window.set_mode_result", eq)]
pub enum SetWindowModeResult {
    Ok { mode: WindowMode, width: u32, height: u32 },
    Err { error: String },
}

impl HeldReply for SetWindowModeResult {
    fn unanswered() -> Self {
        Self::Err { error: "window endpoint closed before answering".into() }
    }
}

/// Change one window's title.
#[aether_data::kind(name = "aether.window.set_title", eq)]
pub struct SetWindowTitle {
    pub title: String,
}

/// Reply to [`SetWindowTitle`] with the applied title.
#[aether_data::kind(name = "aether.window.set_title_result", eq)]
pub enum SetWindowTitleResult {
    Ok { title: String },
    Err { error: String },
}

impl HeldReply for SetWindowTitleResult {
    fn unanswered() -> Self {
        Self::Err { error: "window endpoint closed before answering".into() }
    }
}

/// One command in a native menu.
///
/// `id` is the caller's own opaque handle: it rides back verbatim on the
/// [`WindowMenuActivated`] the platform publishes when the item is chosen, so
/// a component numbers its items however it likes and switches on the number.
/// `shortcut` is advisory accelerator text in muda's grammar (`"Cmd+S"`,
/// `"Ctrl+Shift+P"`); the platform renders it where it can and ignores an
/// unparseable value rather than refusing the whole menu. An empty string
/// requests no accelerator.
#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WindowMenuItem {
    pub id: u32,
    pub label: String,
    pub shortcut: String,
    pub enabled: bool,
    pub separator_after: bool,
}

/// One top-level menu — a title in the bar and the items beneath it.
#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WindowMenu {
    pub title: String,
    pub items: Vec<WindowMenuItem>,
}

/// Install a native menu bar for the addressed window.
///
/// An empty `menus` list installs a bar carrying only the platform's own
/// application menu, where the platform has one.
#[aether_data::kind(name = "aether.window.set_menu", eq)]
pub struct SetWindowMenu {
    pub menus: Vec<WindowMenu>,
}

/// Reply to [`SetWindowMenu`].
#[aether_data::kind(name = "aether.window.set_menu_result", eq)]
pub enum SetWindowMenuResult {
    Ok,
    Err { error: String },
}

impl HeldReply for SetWindowMenuResult {
    fn unanswered() -> Self {
        Self::Err { error: "window endpoint closed before answering".into() }
    }
}

/// Published when a native menu item is chosen, carrying the window whose
/// menu owns the item and the caller's own [`WindowMenuItem::id`].
///
/// Routed by the same selector-aware subscription family as [`aether_kinds::Key`].
#[aether_data::kind(name = "aether.window.menu_activated", eq)]
pub struct WindowMenuActivated {
    pub window: ErasedActorPath,
    pub id: u32,
}

/// The pointer shape a window asks the platform to draw.
///
/// The four resize shapes name the axis the drag moves along, so a component
/// hovering a resizable edge or a movable splitter says what the gesture does
/// rather than which corner it is near. `ResizeDiagonalRising` runs
/// bottom-left to top-right; `ResizeDiagonalFalling` runs top-left to
/// bottom-right.
#[derive(aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum CursorIcon {
    #[default]
    Default,
    Pointer,
    Text,
    Move,
    ResizeHorizontal,
    ResizeVertical,
    ResizeDiagonalRising,
    ResizeDiagonalFalling,
    Grab,
    Grabbing,
    NotAllowed,
    Wait,
}

/// Set the addressed window's pointer shape.
#[aether_data::kind(name = "aether.window.set_cursor", copy, eq)]
pub struct SetWindowCursor {
    pub icon: CursorIcon,
}

/// Reply to [`SetWindowCursor`].
#[aether_data::kind(name = "aether.window.set_cursor_result", eq)]
pub enum SetWindowCursorResult {
    Ok,
    Err { error: String },
}

impl HeldReply for SetWindowCursorResult {
    fn unanswered() -> Self {
        Self::Err { error: "window endpoint closed before answering".into() }
    }
}

/// Bring the addressed window to the foreground.
#[aether_data::kind(name = "aether.window.focus", copy, eq)]
pub struct FocusWindow;

/// Reply to [`FocusWindow`].
#[aether_data::kind(name = "aether.window.focus_result", eq)]
pub enum FocusWindowResult {
    Ok,
    Err { error: String },
}

impl HeldReply for FocusWindowResult {
    fn unanswered() -> Self {
        Self::Err { error: "window endpoint closed before answering".into() }
    }
}

/// Ask the platform to schedule the addressed window for redraw.
#[aether_data::kind(name = "aether.window.request_redraw", copy, eq)]
pub struct RequestWindowRedraw;

/// Reply to [`RequestWindowRedraw`].
#[aether_data::kind(name = "aether.window.request_redraw_result", eq)]
pub enum RequestWindowRedrawResult {
    Ok,
    Err { error: String },
}

impl HeldReply for RequestWindowRedrawResult {
    fn unanswered() -> Self {
        Self::Err { error: "window endpoint closed before answering".into() }
    }
}

/// Manager-private command forwarded by one window child. It names no window:
/// the manager resolves the window from the child's stamped sender against the
/// children it tracks, and refuses a command from any other sender with the
/// command's own `Err`.
///
/// `internal` names who sends it — a window child to its own manager, never a
/// peer — not whether the type can be named. The manager identity's always-on
/// `#[actor]` markers declare this command and [`ApplyWindowCommandResult`] as
/// its handler contract whether or not the runtime half compiles, which is what
/// `describe_handlers` reports, so the pair and the [`WindowCommand`] it carries
/// are reachable rather than crate-private items no marker-only build can
/// construct.
#[aether_data::kind(name = "aether.window.internal.apply_command", eq)]
pub struct ApplyWindowCommand {
    pub command: WindowCommand,
}

/// The seven per-window operations a child forwards to its manager.
#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum WindowCommand {
    Close,
    SetMode { mode: WindowMode, width: Option<u32>, height: Option<u32> },
    SetTitle { title: String },
    SetMenu { menus: Vec<WindowMenu> },
    SetCursor { icon: CursorIcon },
    Focus,
    RequestRedraw,
}

/// Only a runtime refuses a command, so the mapping compiles with the runtime
/// half — a marker-only build carries the command vocabulary without it.
#[cfg(feature = "runtime")]
impl WindowCommand {
    /// This command's own `Err` reply, carrying `error`.
    ///
    /// Every manager needs the same mapping — a chassis with no window
    /// peripheral refuses all seven, and a concrete one refuses whichever
    /// names a window it cannot reach — and the reply variant has to match the
    /// command or the forwarding child aborts on the correlation check
    /// (`runtime::instance::complete`). One mapping, so a new command wires its
    /// refusal once.
    pub(crate) fn refused(&self, error: String) -> ApplyWindowCommandResult {
        match self {
            Self::Close => ApplyWindowCommandResult::Close(CloseWindowResult::Err { error }),
            Self::SetMode { .. } => ApplyWindowCommandResult::SetMode(SetWindowModeResult::Err { error }),
            Self::SetTitle { .. } => ApplyWindowCommandResult::SetTitle(SetWindowTitleResult::Err { error }),
            Self::SetMenu { .. } => ApplyWindowCommandResult::SetMenu(SetWindowMenuResult::Err { error }),
            Self::SetCursor { .. } => ApplyWindowCommandResult::SetCursor(SetWindowCursorResult::Err { error }),
            Self::Focus => ApplyWindowCommandResult::Focus(FocusWindowResult::Err { error }),
            Self::RequestRedraw => ApplyWindowCommandResult::RequestRedraw(RequestWindowRedrawResult::Err { error }),
        }
    }
}

/// Manager-private result returned to the forwarding child — the declared
/// reply of [`ApplyWindowCommand`], and reachable for the same reason.
#[aether_data::kind(name = "aether.window.internal.apply_command_result", eq)]
pub enum ApplyWindowCommandResult {
    Close(CloseWindowResult),
    SetMode(SetWindowModeResult),
    SetTitle(SetWindowTitleResult),
    SetMenu(SetWindowMenuResult),
    SetCursor(SetWindowCursorResult),
    Focus(FocusWindowResult),
    RequestRedraw(RequestWindowRedrawResult),
    /// The manager closed before it answered the forwarded command (ADR-0243
    /// §1). The forwarding child answers its caller with that command's own
    /// `Err` carrying `error`.
    Unanswered {
        error: String,
    },
}

impl HeldReply for ApplyWindowCommandResult {
    fn unanswered() -> Self {
        Self::Unanswered { error: "window manager closed before answering".into() }
    }
}

/// The manager-private kinds a window child handles. A handled kind enters
/// its public actor's contract row list, so it is declared `pub` (ADR-0231
/// §10); this module is private, so no other crate has a path to it.
#[cfg(any(feature = "desktop", feature = "synthetic"))]
mod internal {
    /// Manager-private request that retires a child after platform-originated
    /// close.
    #[aether_data::kind(name = "aether.window.internal.retire", copy, eq)]
    pub struct RetireWindow;
}

#[cfg(any(feature = "desktop", feature = "synthetic"))]
pub(crate) use internal::RetireWindow;

/// Writes [`WindowSubscription`] from the published-kind list.
macro_rules! subscription {
    ($($kind:ident $field:ident),+ $(,)?) => {
        /// One published kind and the subscriber to hold for it: the path of an
        /// actor that handles the kind silently (ADR-0231 §8). The path decodes
        /// only against a live route that publishes that silent row.
        ///
        /// Over MCP each variant takes the subscriber's canonical path, as in
        /// `{"Key": "aether.component/aether.embedded:ui"}`.
        #[derive(aether_data::Schema, Debug, Clone, PartialEq, Eq)]
        pub enum WindowSubscription {
            $(
                #[doc = concat!("A subscriber to [`", stringify!($kind), "`].")]
                $kind(ProtocolPath<Subscriber<$kind>>),
            )+
        }
    };
}

published_window_kinds!(subscription);

/// Subscribe an explicitly named actor to one published kind for a window
/// selector. `subscription` names the kind and the subscriber's canonical
/// path; the manager proves it live at receipt and replies `Err` when it is
/// not.
#[aether_data::kind(name = "aether.window.subscribe", no_serde, eq)]
pub struct SubscribeWindow {
    pub selector: WindowSelector,
    pub subscription: WindowSubscription,
}

/// Subscribe the sending actor to a kind for a window selector. The sender
/// must handle the kind silently or manually; the manager refuses one whose
/// published rows do not.
#[aether_data::kind(name = "aether.window.subscribe_self", eq)]
pub struct SubscribeWindowSelf {
    pub selector: WindowSelector,
    pub kind: KindId,
}

/// Remove an explicitly named actor's subscription for a selector and kind,
/// named by the same `subscription` its subscribe carried.
#[aether_data::kind(name = "aether.window.unsubscribe", no_serde, eq)]
pub struct UnsubscribeWindow {
    pub selector: WindowSelector,
    pub subscription: WindowSubscription,
}

/// Remove the sending actor's subscription for a selector and kind.
#[aether_data::kind(name = "aether.window.unsubscribe_self", eq)]
pub struct UnsubscribeWindowSelf {
    pub selector: WindowSelector,
    pub kind: KindId,
}

/// Reply shared by the subscribe and unsubscribe request families.
#[aether_data::kind(name = "aether.window.subscribe_result", eq)]
pub enum SubscribeWindowResult {
    Ok,
    Err { error: String },
}

/// Raw, already-encoded window event injected through the synthetic runtime.
///
/// The runtime deliberately has one handler for this envelope rather than a
/// handler or cached id for every public window event kind.
#[cfg(feature = "synthetic")]
#[aether_data::kind(name = "aether.window.inject_event", eq)]
pub struct InjectWindowEvent {
    pub window: ErasedActorPath,
    pub kind: KindId,
    #[serde(with = "aether_data::bytes")]
    pub payload: Vec<u8>,
}

/// Published after a newly created window is fully attached.
#[aether_data::kind(name = "aether.window.opened", eq)]
pub struct WindowOpened {
    pub window: WindowInfo,
}

/// Published after a window and its native resources are detached.
#[aether_data::kind(name = "aether.window.closed", eq)]
pub struct WindowClosed {
    pub window: ErasedActorPath,
}
