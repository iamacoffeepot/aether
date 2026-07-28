//! Fail-fast runtime for chassis without a window peripheral.

use aether_actor::runtime;

use super::{
    ApplyWindowCommand, ApplyWindowCommandResult, CloseWindowResult, CreateWindow, CreateWindowResult,
    FocusWindowResult, HeadlessWindowCapability, ListWindows, ListWindowsResult, RequestWindowRedrawResult,
    SetWindowModeResult, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult, SubscribeWindowSelf,
    UnsubscribeAllWindows, UnsubscribeWindow, UnsubscribeWindowSelf, WindowCommand,
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
        let error = unsupported();
        match mail.command {
            WindowCommand::Close => ApplyWindowCommandResult::Close(CloseWindowResult::Err { error }),
            WindowCommand::SetMode { .. } => ApplyWindowCommandResult::SetMode(SetWindowModeResult::Err { error }),
            WindowCommand::SetTitle { .. } => ApplyWindowCommandResult::SetTitle(SetWindowTitleResult::Err { error }),
            WindowCommand::Focus => ApplyWindowCommandResult::Focus(FocusWindowResult::Err { error }),
            WindowCommand::RequestRedraw => {
                ApplyWindowCommandResult::RequestRedraw(RequestWindowRedrawResult::Err { error })
            }
        }
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

#[cfg(feature = "synthetic")]
pub mod synthetic;

#[cfg(feature = "desktop")]
pub mod desktop;

#[cfg(any(feature = "desktop", feature = "synthetic"))]
mod subscribers;
