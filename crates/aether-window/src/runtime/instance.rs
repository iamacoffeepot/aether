//! Shared named-window forwarding plus the fail-fast headless runtime.

#[cfg(any(feature = "desktop", feature = "synthetic"))]
use std::collections::HashMap;

use aether_actor::runtime;
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_actor::{Manual, ReplyMode};
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_data::{Kind, MailId};
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_substrate::InboundMail;

use super::{BootError, NativeActor, NativeCtx, NativeInitCtx, unsupported};
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use crate::{
    ApplyWindowCommand, ApplyWindowCommandResult, RetireWindow, WindowCapability, WindowCommand, WindowForwardContext,
    WindowId,
};
use crate::{
    CloseWindow, CloseWindowResult, FocusWindow, FocusWindowResult, HeadlessWindowInstance, RequestWindowRedraw,
    RequestWindowRedrawResult, SetWindowMode, SetWindowModeResult, SetWindowTitle, SetWindowTitleResult,
};

/// Retained public requests for one concrete forwarding child.
#[cfg(any(feature = "desktop", feature = "synthetic"))]
pub struct WindowInstanceState {
    pending: HashMap<MailId, InboundMail>,
}

#[cfg(any(feature = "desktop", feature = "synthetic"))]
impl WindowInstanceState {
    pub(super) fn new() -> Self {
        Self { pending: HashMap::new() }
    }
}

#[cfg(any(feature = "desktop", feature = "synthetic"))]
pub(super) fn forward(state: &mut WindowInstanceState, ctx: &mut NativeCtx<'_, Manual>, command: WindowCommand) {
    let inbound = ctx.take_inbound();
    let mail_id = inbound.mail_id();
    if state.pending.insert(mail_id, inbound).is_some() {
        ctx.fatal_abort(format!("duplicate retained window request {mail_id:?}"));
    }
    let _ = ctx
        .actor::<WindowCapability>()
        .with_context(&WindowForwardContext { inbound: mail_id })
        .send(&ApplyWindowCommand { window: WindowId(ctx.self_id().0), command });
}

#[cfg(any(feature = "desktop", feature = "synthetic"))]
pub(super) fn complete<M: ReplyMode>(
    state: &mut WindowInstanceState,
    ctx: &mut NativeCtx<'_, M>,
    result: ApplyWindowCommandResult,
) {
    let Some(context) = ctx.take_context::<WindowForwardContext>() else {
        ctx.fatal_abort("window child received an uncorrelated manager result".to_owned());
    };
    let Some(inbound) = state.pending.remove(&context.inbound) else {
        ctx.fatal_abort(format!("window child has no retained request {:?}", context.inbound));
    };

    let close_succeeded = match result {
        ApplyWindowCommandResult::Close(reply) if inbound.kind() == CloseWindow::ID => {
            let succeeded = matches!(reply, CloseWindowResult::Ok);
            inbound.reply(&reply);
            succeeded
        }
        ApplyWindowCommandResult::SetMode(reply) if inbound.kind() == SetWindowMode::ID => {
            inbound.reply(&reply);
            false
        }
        ApplyWindowCommandResult::SetTitle(reply) if inbound.kind() == SetWindowTitle::ID => {
            inbound.reply(&reply);
            false
        }
        ApplyWindowCommandResult::Focus(reply) if inbound.kind() == FocusWindow::ID => {
            inbound.reply(&reply);
            false
        }
        ApplyWindowCommandResult::RequestRedraw(reply) if inbound.kind() == RequestWindowRedraw::ID => {
            inbound.reply(&reply);
            false
        }
        result => ctx.fatal_abort(format!(
            "window child manager result {result:?} does not match retained kind {:?}",
            inbound.kind()
        )),
    };
    if close_succeeded {
        ctx.shutdown();
    }
}

#[cfg(any(feature = "desktop", feature = "synthetic"))]
pub(super) fn retire<M: ReplyMode>(_state: &mut WindowInstanceState, ctx: &mut NativeCtx<'_, M>, _mail: RetireWindow) {
    ctx.shutdown();
}

#[cfg(any(feature = "desktop", feature = "synthetic"))]
pub(super) fn unwire(state: &mut WindowInstanceState) {
    for (_, inbound) in state.pending.drain() {
        let error = "window endpoint shutting down".to_owned();
        match inbound.kind() {
            kind if kind == CloseWindow::ID => {
                inbound.reply(&CloseWindowResult::Err { error });
            }
            kind if kind == SetWindowMode::ID => {
                inbound.reply(&SetWindowModeResult::Err { error });
            }
            kind if kind == SetWindowTitle::ID => {
                inbound.reply(&SetWindowTitleResult::Err { error });
            }
            kind if kind == FocusWindow::ID => {
                inbound.reply(&FocusWindowResult::Err { error });
            }
            kind if kind == RequestWindowRedraw::ID => {
                inbound.reply(&RequestWindowRedrawResult::Err { error });
            }
            _ => {}
        }
    }
}

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
