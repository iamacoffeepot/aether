//! Public wire vocabulary for the `aether.window` manager.

use aether_data::{KindId, MailboxId};
// The forwarding correlation's own type, gated with it.
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_data::MailId;
use aether_kinds::{WindowId, WindowMode};
use serde::{Deserialize, Serialize};

/// Select one window or every current and future window.
///
/// `All` is prospective: a subscription using it also observes matching
/// events from windows created after the subscription is installed.
#[derive(aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
pub enum WindowSelector {
    One(WindowId),
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

/// Public state for one live window.
#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: WindowId,
    pub name: String,
    pub title: String,
    pub mode: WindowMode,
    pub width: u32,
    pub height: u32,
    pub focused: bool,
    pub occluded: bool,
}

/// List every live window in ascending [`WindowId`] order.
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

/// Begin closing the addressed window.
#[aether_data::kind(name = "aether.window.close", copy, eq)]
pub struct CloseWindow;

/// Reply to [`CloseWindow`].
#[aether_data::kind(name = "aether.window.close_result", eq)]
pub enum CloseWindowResult {
    Ok,
    Err { error: String },
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

/// Published when a native menu item is chosen, carrying the window whose
/// menu owns the item and the caller's own [`WindowMenuItem::id`].
///
/// Routed by the same selector-aware subscription family as [`aether_kinds::Key`].
#[aether_data::kind(name = "aether.window.menu_activated", copy, eq)]
pub struct WindowMenuActivated {
    pub window: WindowId,
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

/// Bring the addressed window to the foreground.
#[aether_data::kind(name = "aether.window.focus", copy, eq)]
pub struct FocusWindow;

/// Reply to [`FocusWindow`].
#[aether_data::kind(name = "aether.window.focus_result", eq)]
pub enum FocusWindowResult {
    Ok,
    Err { error: String },
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

/// Manager-private id-bearing command forwarded by one window child.
#[aether_data::kind(name = "aether.window.internal.apply_command", eq)]
pub(crate) struct ApplyWindowCommand {
    pub window: WindowId,
    pub command: WindowCommand,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) enum WindowCommand {
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

/// Manager-private result returned to the forwarding child.
///
/// The reply half of the private forwarding protocol: a manager runtime
/// produces it and a forwarding child consumes it, so it rides the same
/// `runtime` gate both of those halves do.
#[cfg(feature = "runtime")]
#[aether_data::kind(name = "aether.window.internal.apply_command_result", eq)]
pub(crate) enum ApplyWindowCommandResult {
    Close(CloseWindowResult),
    SetMode(SetWindowModeResult),
    SetTitle(SetWindowTitleResult),
    SetMenu(SetWindowMenuResult),
    SetCursor(SetWindowCursorResult),
    Focus(FocusWindowResult),
    RequestRedraw(RequestWindowRedrawResult),
}

/// Correlation stored on the private manager request.
///
/// Only a window-bearing runtime forwards, so this and [`RetireWindow`] carry
/// the same gate their crate-root re-export already carries.
#[cfg(any(feature = "desktop", feature = "synthetic"))]
#[aether_data::kind(name = "aether.window.internal.forward_context", copy, eq)]
pub(crate) struct WindowForwardContext {
    pub inbound: MailId,
}

/// Manager-private request that retires a child after platform-originated close.
#[cfg(any(feature = "desktop", feature = "synthetic"))]
#[aether_data::kind(name = "aether.window.internal.retire", copy, eq)]
pub(crate) struct RetireWindow;

/// Subscribe an explicit mailbox to a kind for a window selector.
#[aether_data::kind(name = "aether.window.subscribe", copy, eq)]
pub struct SubscribeWindow {
    pub selector: WindowSelector,
    pub kind: KindId,
    pub mailbox: MailboxId,
}

/// Subscribe the sending actor to a kind for a window selector.
#[aether_data::kind(name = "aether.window.subscribe_self", copy, eq)]
pub struct SubscribeWindowSelf {
    pub selector: WindowSelector,
    pub kind: KindId,
}

/// Remove an explicit mailbox's subscription for a selector and kind.
#[aether_data::kind(name = "aether.window.unsubscribe", copy, eq)]
pub struct UnsubscribeWindow {
    pub selector: WindowSelector,
    pub kind: KindId,
    pub mailbox: MailboxId,
}

/// Remove the sending actor's subscription for a selector and kind.
#[aether_data::kind(name = "aether.window.unsubscribe_self", copy, eq)]
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

/// Remove one mailbox from every window-event subscription.
///
/// This is the externally sendable bulk form. Runtime monitor cleanup uses
/// the same operation internally when a subscriber mailbox closes.
#[aether_data::kind(name = "aether.window.unsubscribe_all", copy, eq)]
pub struct UnsubscribeAllWindows {
    pub mailbox: MailboxId,
}

/// Raw, already-encoded window event injected through the synthetic runtime.
///
/// The runtime deliberately has one handler for this envelope rather than a
/// handler or cached id for every public window event kind.
#[cfg(feature = "synthetic")]
#[aether_data::kind(name = "aether.window.inject_event", eq)]
pub struct InjectWindowEvent {
    pub window: WindowId,
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
#[aether_data::kind(name = "aether.window.closed", copy, eq)]
pub struct WindowClosed {
    pub window: WindowId,
}
