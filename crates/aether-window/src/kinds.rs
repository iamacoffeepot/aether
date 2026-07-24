//! Public wire vocabulary for the `aether.window` manager.

use aether_data::{KindId, MailboxId};
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
    pub title: String,
    pub mode: WindowMode,
    pub size: Option<WindowSizeRequest>,
}

/// Public state for one live window.
#[derive(aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: WindowId,
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

/// Begin closing one explicitly addressed window.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.close")]
pub struct CloseWindow {
    pub window: WindowId,
}

/// Reply to [`CloseWindow`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.close_result")]
pub enum CloseWindowResult {
    Ok { window: WindowId },
    Err { window: WindowId, error: String },
}

/// Change one window's presentation mode.
///
/// `width` and `height` apply only to [`WindowMode::Windowed`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.set_mode")]
pub struct SetWindowMode {
    pub window: WindowId,
    pub mode: WindowMode,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Reply to [`SetWindowMode`] with the resolved state.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.set_mode_result")]
pub enum SetWindowModeResult {
    Ok { window: WindowId, mode: WindowMode, width: u32, height: u32 },
    Err { window: WindowId, error: String },
}

/// Change one window's title.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.set_title")]
pub struct SetWindowTitle {
    pub window: WindowId,
    pub title: String,
}

/// Reply to [`SetWindowTitle`] with the applied title.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.set_title_result")]
pub enum SetWindowTitleResult {
    Ok { window: WindowId, title: String },
    Err { window: WindowId, error: String },
}

/// Bring one window to the foreground.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.focus")]
pub struct FocusWindow {
    pub window: WindowId,
}

/// Reply to [`FocusWindow`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.focus_result")]
pub enum FocusWindowResult {
    Ok { window: WindowId },
    Err { window: WindowId, error: String },
}

/// Ask the platform to schedule a redraw for one window.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.request_redraw")]
pub struct RequestWindowRedraw {
    pub window: WindowId,
}

/// Reply to [`RequestWindowRedraw`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[kind(name = "aether.window.request_redraw_result")]
pub enum RequestWindowRedrawResult {
    Ok { window: WindowId },
    Err { window: WindowId, error: String },
}

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

#[cfg(test)]
mod tests {
    use core::fmt::Debug;

    use aether_data::Kind;

    use super::*;

    fn assert_round_trip<K>(value: K)
    where
        K: Kind + Debug + PartialEq,
    {
        let encoded = value.encode_into_bytes();
        assert_eq!(K::decode_from_bytes(&encoded), Some(value));
    }

    fn spec() -> WindowSpec {
        WindowSpec {
            title: "tools".to_owned(),
            mode: WindowMode::Windowed,
            size: Some(WindowSizeRequest { width: 1280, height: 720 }),
        }
    }

    fn info() -> WindowInfo {
        WindowInfo {
            id: WindowId(7),
            title: "tools".to_owned(),
            mode: WindowMode::Windowed,
            width: 1280,
            height: 720,
            focused: true,
            occluded: false,
        }
    }

    #[test]
    fn lifecycle_and_control_kinds_round_trip_with_explicit_window_ids() {
        let window = WindowId(7);
        let window_info = info();

        assert_round_trip(ListWindows);
        assert_round_trip(ListWindowsResult::Ok { windows: vec![window_info.clone()] });
        assert_round_trip(CreateWindow { spec: spec() });
        assert_round_trip(CreateWindowResult::Ok { window: window_info.clone() });
        assert_round_trip(CloseWindow { window });
        assert_round_trip(CloseWindowResult::Ok { window });
        assert_round_trip(SetWindowMode { window, mode: WindowMode::FullscreenBorderless, width: None, height: None });
        assert_round_trip(SetWindowModeResult::Ok { window, mode: WindowMode::Windowed, width: 1280, height: 720 });
        assert_round_trip(SetWindowTitle { window, title: "scene".to_owned() });
        assert_round_trip(SetWindowTitleResult::Ok { window, title: "scene".to_owned() });
        assert_round_trip(FocusWindow { window });
        assert_round_trip(FocusWindowResult::Ok { window });
        assert_round_trip(RequestWindowRedraw { window });
        assert_round_trip(RequestWindowRedrawResult::Ok { window });
        assert_round_trip(WindowOpened { window: window_info });
        assert_round_trip(WindowClosed { window });
    }

    #[test]
    fn subscription_kinds_round_trip_one_and_prospective_all_selectors() {
        let window = WindowId(7);
        let kind = aether_kinds::Key::ID;
        let mailbox = MailboxId(9);

        assert_round_trip(SubscribeWindowSelf { selector: WindowSelector::All, kind });
        assert_round_trip(SubscribeWindow { selector: WindowSelector::One(window), kind, mailbox });
        assert_round_trip(UnsubscribeWindowSelf { selector: WindowSelector::One(window), kind });
        assert_round_trip(UnsubscribeWindow { selector: WindowSelector::All, kind, mailbox });
        assert_round_trip(SubscribeWindowResult::Ok);
        assert_round_trip(UnsubscribeAllWindows { mailbox });
    }
}
