//! Fail-fast runtime for chassis without a window peripheral.

use aether_actor::runtime;

use super::{
    ApplyWindowCommand, ApplyWindowCommandResult, CloseWindow, CloseWindowResult, CreateWindow, CreateWindowResult,
    FocusWindow, FocusWindowResult, HeadlessWindowCapability, ListWindows, ListWindowsResult, RequestWindowRedraw,
    RequestWindowRedrawResult, SetWindowCursor, SetWindowCursorResult, SetWindowMenu, SetWindowMenuResult,
    SetWindowMode, SetWindowModeResult, SetWindowTitle, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult,
    SubscribeWindowSelf, UnsubscribeAllWindows, UnsubscribeWindow, UnsubscribeWindowSelf,
};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// Runtime state for the stateless no-window companion.
pub struct HeadlessWindowCapabilityState;

fn unsupported() -> String {
    "unsupported on this chassis — no window peripheral".to_owned()
}

mod instance;

#[runtime]
impl NativeActor for HeadlessWindowCapability {
    type State = HeadlessWindowCapabilityState;
    type Config = ();

    const NAMESPACE: &'static str = crate::WINDOW_NAMESPACE;

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<HeadlessWindowCapabilityState, BootError> {
        Ok(HeadlessWindowCapabilityState)
    }

    #[handler::single]
    fn on_list(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListWindows) -> ListWindowsResult {
        ListWindowsResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_create(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: CreateWindow) -> CreateWindowResult {
        CreateWindowResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_apply_command(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: ApplyWindowCommand,
    ) -> ApplyWindowCommandResult {
        mail.command.refused(unsupported())
    }

    #[handler::single]
    fn on_subscribe(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: SubscribeWindow,
    ) -> SubscribeWindowResult {
        SubscribeWindowResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_subscribe_self(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: SubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        SubscribeWindowResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_unsubscribe(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: UnsubscribeWindow,
    ) -> SubscribeWindowResult {
        SubscribeWindowResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_unsubscribe_self(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: UnsubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        SubscribeWindowResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_unsubscribe_all(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: UnsubscribeAllWindows) {}

    // The seven per-window commands, refused at the root as well as at an
    // endpoint (iamacoffeepot/aether#5505). Both identities are addressable, so
    // a command sent to either has to be answered rather than dropped on the
    // floor — a silent no-op reads as success to a caller whose mail settled.
    // The endpoint's identical seven stay written out beside these rather than
    // shared through a handler set, because a set's `HandlesKind` markers reach
    // an adopter through a `macro_rules!` bridge that lives with the set, and
    // both identities also compile in the marker-only build where this whole
    // runtime module is `cfg`-ed away.

    #[handler::single]
    fn on_close(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: CloseWindow) -> CloseWindowResult {
        CloseWindowResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_set_mode(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: SetWindowMode) -> SetWindowModeResult {
        SetWindowModeResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_set_title(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: SetWindowTitle) -> SetWindowTitleResult {
        SetWindowTitleResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_set_menu(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: SetWindowMenu) -> SetWindowMenuResult {
        SetWindowMenuResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_set_cursor(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: SetWindowCursor,
    ) -> SetWindowCursorResult {
        SetWindowCursorResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_focus(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: FocusWindow) -> FocusWindowResult {
        FocusWindowResult::Err { error: unsupported() }
    }

    #[handler::single]
    fn on_request_redraw(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        _mail: RequestWindowRedraw,
    ) -> RequestWindowRedrawResult {
        RequestWindowRedrawResult::Err { error: unsupported() }
    }
}

#[cfg(feature = "synthetic")]
pub mod synthetic;

#[cfg(feature = "desktop")]
pub mod desktop;

#[cfg(any(feature = "desktop", feature = "synthetic"))]
mod manager;
#[cfg(any(feature = "desktop", feature = "synthetic"))]
mod subscribers;
