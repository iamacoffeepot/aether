//! Public wire vocabulary for the `aether.window` manager.

use aether_data::{KindId, MailId, MailboxId};
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
#[derive(
    aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, Default, PartialEq, Eq,
)]
#[kind(name = "aether.window.list")]
pub struct ListWindows;

/// Reply to [`ListWindows`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.list_result")]
pub enum ListWindowsResult {
    Ok { windows: Vec<WindowInfo> },
    Err { error: String },
}

/// Create a window from an explicit specification.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.create")]
pub struct CreateWindow {
    pub spec: WindowSpec,
}

/// Reply to [`CreateWindow`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.create_result")]
pub enum CreateWindowResult {
    Ok { window: WindowInfo },
    Err { error: String },
}

/// Begin closing the addressed window.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.close")]
pub struct CloseWindow;

/// Reply to [`CloseWindow`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.close_result")]
pub enum CloseWindowResult {
    Ok,
    Err { error: String },
}

/// Change one window's presentation mode.
///
/// `width` and `height` apply only to [`WindowMode::Windowed`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.set_mode")]
pub struct SetWindowMode {
    pub mode: WindowMode,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Reply to [`SetWindowMode`] with the resolved state.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.set_mode_result")]
pub enum SetWindowModeResult {
    Ok { mode: WindowMode, width: u32, height: u32 },
    Err { error: String },
}

/// Change one window's title.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.set_title")]
pub struct SetWindowTitle {
    pub title: String,
}

/// Reply to [`SetWindowTitle`] with the applied title.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.set_title_result")]
pub enum SetWindowTitleResult {
    Ok { title: String },
    Err { error: String },
}

/// Bring the addressed window to the foreground.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.focus")]
pub struct FocusWindow;

/// Reply to [`FocusWindow`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.focus_result")]
pub enum FocusWindowResult {
    Ok,
    Err { error: String },
}

/// Ask the platform to schedule the addressed window for redraw.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.request_redraw")]
pub struct RequestWindowRedraw;

/// Reply to [`RequestWindowRedraw`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.request_redraw_result")]
pub enum RequestWindowRedrawResult {
    Ok,
    Err { error: String },
}

/// Manager-private id-bearing command forwarded by one window child.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.internal.apply_command")]
pub(crate) struct ApplyWindowCommand {
    pub window: WindowId,
    pub command: WindowCommand,
}

#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) enum WindowCommand {
    Close,
    SetMode { mode: WindowMode, width: Option<u32>, height: Option<u32> },
    SetTitle { title: String },
    Focus,
    RequestRedraw,
}

/// Manager-private result returned to the forwarding child.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.internal.apply_command_result")]
pub(crate) enum ApplyWindowCommandResult {
    Close(CloseWindowResult),
    SetMode(SetWindowModeResult),
    SetTitle(SetWindowTitleResult),
    Focus(FocusWindowResult),
    RequestRedraw(RequestWindowRedrawResult),
}

/// Correlation stored on the private manager request.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.internal.forward_context")]
pub(crate) struct WindowForwardContext {
    pub inbound: MailId,
}

/// Manager-private request that retires a child after platform-originated close.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.internal.retire")]
pub(crate) struct RetireWindow;

/// Subscribe an explicit mailbox to a kind for a window selector.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.subscribe")]
pub struct SubscribeWindow {
    pub selector: WindowSelector,
    pub kind: KindId,
    pub mailbox: MailboxId,
}

/// Subscribe the sending actor to a kind for a window selector.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.subscribe_self")]
pub struct SubscribeWindowSelf {
    pub selector: WindowSelector,
    pub kind: KindId,
}

/// Remove an explicit mailbox's subscription for a selector and kind.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.unsubscribe")]
pub struct UnsubscribeWindow {
    pub selector: WindowSelector,
    pub kind: KindId,
    pub mailbox: MailboxId,
}

/// Remove the sending actor's subscription for a selector and kind.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.unsubscribe_self")]
pub struct UnsubscribeWindowSelf {
    pub selector: WindowSelector,
    pub kind: KindId,
}

/// Reply shared by the subscribe and unsubscribe request families.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.subscribe_result")]
pub enum SubscribeWindowResult {
    Ok,
    Err { error: String },
}

/// Remove one mailbox from every window-event subscription.
///
/// This is the externally sendable bulk form. Runtime monitor cleanup uses
/// the same operation internally when a subscriber mailbox closes.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.unsubscribe_all")]
pub struct UnsubscribeAllWindows {
    pub mailbox: MailboxId,
}

/// Raw, already-encoded window event injected through the synthetic runtime.
///
/// The runtime deliberately has one handler for this envelope rather than a
/// handler or cached id for every public window event kind.
#[cfg(feature = "synthetic")]
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.inject_event")]
pub struct InjectWindowEvent {
    pub window: WindowId,
    pub kind: KindId,
    pub payload: Vec<u8>,
}

/// Published after a newly created window is fully attached.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.opened")]
pub struct WindowOpened {
    pub window: WindowInfo,
}

/// Published after a window and its native resources are detached.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.closed")]
pub struct WindowClosed {
    pub window: WindowId,
}
