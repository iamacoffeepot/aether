//! Fail-fast runtime for chassis without a window peripheral.

use aether_actor::runtime;

use super::{
    CloseWindow, CloseWindowResult, CreateWindow, CreateWindowResult, FocusWindow, FocusWindowResult, ListWindows,
    ListWindowsResult, RequestWindowRedraw, RequestWindowRedrawResult, SetWindowMode, SetWindowModeResult,
    SetWindowTitle, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult, SubscribeWindowSelf,
    UnsubscribeAllWindows, UnsubscribeWindow, UnsubscribeWindowSelf, WindowCapability,
};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

/// Runtime state for the stateless no-window companion.
pub struct HeadlessWindowCapabilityState;

fn unsupported() -> String {
    "unsupported on this chassis — no window peripheral".to_owned()
}

#[runtime]
impl NativeActor for WindowCapability {
    type State = HeadlessWindowCapabilityState;
    type Config = ();

    const NAMESPACE: &'static str = "aether.window";

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
    fn on_close(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: CloseWindow) -> CloseWindowResult {
        CloseWindowResult::Err { window: mail.window, error: unsupported() }
    }

    #[handler::single]
    fn on_set_mode(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: SetWindowMode) -> SetWindowModeResult {
        SetWindowModeResult::Err { window: mail.window, error: unsupported() }
    }

    #[handler::single]
    fn on_set_title(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: SetWindowTitle) -> SetWindowTitleResult {
        SetWindowTitleResult::Err { window: mail.window, error: unsupported() }
    }

    #[handler::single]
    fn on_focus(_state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: FocusWindow) -> FocusWindowResult {
        FocusWindowResult::Err { window: mail.window, error: unsupported() }
    }

    #[handler::single]
    fn on_request_redraw(
        _state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: RequestWindowRedraw,
    ) -> RequestWindowRedrawResult {
        RequestWindowRedrawResult::Err { window: mail.window, error: unsupported() }
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
}
