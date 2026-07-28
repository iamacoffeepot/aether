//! Pooled forwarding runtime for one synthetic window endpoint.

use aether_actor::{Manual, runtime};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;

use crate::runtime::instance::{WindowInstanceState, complete, forward, retire, unwire};
use crate::{
    ApplyWindowCommandResult, CloseWindow, FocusWindow, RequestWindowRedraw, RetireWindow, SetWindowMode,
    SetWindowTitle, SyntheticWindowInstance, WindowCommand,
};

#[runtime]
impl NativeActor for SyntheticWindowInstance {
    type State = WindowInstanceState;
    type Config = ();

    const NAMESPACE: &'static str = crate::WINDOW_INSTANCE_NAMESPACE;

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<WindowInstanceState, BootError> {
        Ok(WindowInstanceState::new())
    }

    fn unwire(state: &mut Self::State, _ctx: &mut NativeCtx<'_>) {
        unwire(state);
    }

    #[handler::manual]
    fn on_close(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, _mail: CloseWindow) {
        forward(state, ctx, WindowCommand::Close);
    }

    #[handler::manual]
    fn on_set_mode(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: SetWindowMode) {
        forward(state, ctx, WindowCommand::SetMode { mode: mail.mode, width: mail.width, height: mail.height });
    }

    #[handler::manual]
    fn on_set_title(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: SetWindowTitle) {
        forward(state, ctx, WindowCommand::SetTitle { title: mail.title });
    }

    #[handler::manual]
    fn on_focus(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, _mail: FocusWindow) {
        forward(state, ctx, WindowCommand::Focus);
    }

    #[handler::manual]
    fn on_request_redraw(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, _mail: RequestWindowRedraw) {
        forward(state, ctx, WindowCommand::RequestRedraw);
    }

    #[handler::single]
    fn on_command_result(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: ApplyWindowCommandResult) {
        complete(state, ctx, result);
    }

    #[handler::single]
    fn on_retire(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: RetireWindow) {
        retire(state, ctx, mail);
    }
}
