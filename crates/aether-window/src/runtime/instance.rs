//! Fail-fast runtime for named window endpoints without a window peripheral.

use aether_actor::runtime;

use super::{BootError, NativeActor, NativeCtx, NativeInitCtx, unsupported};
use crate::{
    CloseWindow, CloseWindowResult, FocusWindow, FocusWindowResult, HeadlessWindowInstance, RequestWindowRedraw,
    RequestWindowRedrawResult, SetWindowMode, SetWindowModeResult, SetWindowTitle, SetWindowTitleResult,
};

/// Inert state for a named window endpoint on a headless chassis.
pub struct HeadlessWindowInstanceState;

#[runtime]
impl NativeActor for HeadlessWindowInstance {
    type State = HeadlessWindowInstanceState;
    type Config = ();

    const NAMESPACE: &'static str = crate::WINDOW_INSTANCE_NAMESPACE;

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<HeadlessWindowInstanceState, BootError> {
        Ok(HeadlessWindowInstanceState)
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
}
