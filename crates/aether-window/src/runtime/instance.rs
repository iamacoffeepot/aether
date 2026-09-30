//! The named-window endpoint: every command it receives is forwarded to the
//! manager, whichever backend the manager runs.

use aether_actor::{HeldReply, ReplyMode, runtime};
use aether_substrate::actor::native::{Held, Pending};

use super::{BootError, NativeActor, NativeCtx, NativeInitCtx};
use crate::{
    ApplyWindowCommand, ApplyWindowCommandResult, CloseWindow, CloseWindowResult, FocusWindow, FocusWindowResult,
    RequestWindowRedraw, RequestWindowRedrawResult, RetireWindow, SetWindowCursor, SetWindowCursorResult,
    SetWindowMenu, SetWindowMenuResult, SetWindowMode, SetWindowModeResult, SetWindowTitle, SetWindowTitleResult,
    WindowCapability, WindowCommand, WindowInstance,
};

/// State of one forwarding endpoint. It keeps none: each public request's
/// held reply rides the context stored under its forward.
pub struct WindowInstanceState;

/// The context stored under a forwarded [`ApplyWindowCommand`]: the public
/// request's held reply (ADR-0243 §4), in the variant naming its command. The
/// completion claims it back and answers it with the manager's result.
///
/// It is a reply context, never mail, so it stays private beside its only
/// user.
#[aether_data::kind(name = "aether.window.internal.forward_context")]
enum WindowForwardContext {
    Close(Held<CloseWindowResult>),
    SetMode(Held<SetWindowModeResult>),
    SetTitle(Held<SetWindowTitleResult>),
    SetMenu(Held<SetWindowMenuResult>),
    SetCursor(Held<SetWindowCursorResult>),
    Focus(Held<FocusWindowResult>),
    RequestRedraw(Held<RequestWindowRedrawResult>),
}

impl WindowForwardContext {
    /// Answer the held request with its own command's `Err`, carrying `error`.
    fn refuse<A, M: ReplyMode>(self, ctx: &mut NativeCtx<'_, A, M>, error: String) {
        match self {
            Self::Close(held) => held.answer(ctx, &CloseWindowResult::Err { error }),
            Self::SetMode(held) => held.answer(ctx, &SetWindowModeResult::Err { error }),
            Self::SetTitle(held) => held.answer(ctx, &SetWindowTitleResult::Err { error }),
            Self::SetMenu(held) => held.answer(ctx, &SetWindowMenuResult::Err { error }),
            Self::SetCursor(held) => held.answer(ctx, &SetWindowCursorResult::Err { error }),
            Self::Focus(held) => held.answer(ctx, &FocusWindowResult::Err { error }),
            Self::RequestRedraw(held) => held.answer(ctx, &RequestWindowRedrawResult::Err { error }),
        }
    }
}

/// Hold the public request's reply and forward `command` to the manager, the
/// held reply riding the forward's context in the variant `context` names.
fn forward<R: HeldReply>(
    ctx: &mut NativeCtx<'_, WindowInstance>,
    command: WindowCommand,
    context: fn(Held<R>) -> WindowForwardContext,
) -> Pending<R> {
    let (pending, held) = ctx.hold::<R>();
    let _ = ctx.send_with_context::<WindowCapability>(&ApplyWindowCommand { command }, context(held));
    pending
}

/// Answer the public request the manager just resolved, from the held reply
/// its forward's context carries. A successful close retires this endpoint.
///
/// A result with no context answers nothing, as a reply handler does with a
/// reply it did not ask for (ADR-0243 §4): any actor may send this endpoint an
/// `ApplyWindowCommandResult`, so a stray one must not stop it. A result whose
/// variant does not match the stored context still answers the held request,
/// with that request's own `Err` naming the mismatch, so the debt is paid.
fn complete<A, M: ReplyMode>(ctx: &mut NativeCtx<'_, A, M>, result: ApplyWindowCommandResult) {
    let Some(context) = ctx.take_context::<WindowForwardContext>() else {
        return;
    };

    match (result, context) {
        (ApplyWindowCommandResult::Close(reply), WindowForwardContext::Close(held)) => {
            let closed = matches!(reply, CloseWindowResult::Ok);
            held.answer(ctx, &reply);
            if closed {
                ctx.shutdown();
            }
        }
        (ApplyWindowCommandResult::SetMode(reply), WindowForwardContext::SetMode(held)) => held.answer(ctx, &reply),
        (ApplyWindowCommandResult::SetTitle(reply), WindowForwardContext::SetTitle(held)) => held.answer(ctx, &reply),
        (ApplyWindowCommandResult::SetMenu(reply), WindowForwardContext::SetMenu(held)) => held.answer(ctx, &reply),
        (ApplyWindowCommandResult::SetCursor(reply), WindowForwardContext::SetCursor(held)) => {
            held.answer(ctx, &reply);
        }
        (ApplyWindowCommandResult::Focus(reply), WindowForwardContext::Focus(held)) => held.answer(ctx, &reply),
        (ApplyWindowCommandResult::RequestRedraw(reply), WindowForwardContext::RequestRedraw(held)) => {
            held.answer(ctx, &reply);
        }
        (ApplyWindowCommandResult::Unanswered { error }, context) => context.refuse(ctx, error),
        (result, context) => {
            let error = format!("the window manager answered {result:?} to a forwarded {}", context.command_name());
            context.refuse(ctx, error);
        }
    }
}

impl WindowForwardContext {
    /// The public command this context holds the reply of.
    fn command_name(&self) -> &'static str {
        match self {
            Self::Close(_) => <CloseWindow as aether_data::Kind>::NAME,
            Self::SetMode(_) => <SetWindowMode as aether_data::Kind>::NAME,
            Self::SetTitle(_) => <SetWindowTitle as aether_data::Kind>::NAME,
            Self::SetMenu(_) => <SetWindowMenu as aether_data::Kind>::NAME,
            Self::SetCursor(_) => <SetWindowCursor as aether_data::Kind>::NAME,
            Self::Focus(_) => <FocusWindow as aether_data::Kind>::NAME,
            Self::RequestRedraw(_) => <RequestWindowRedraw as aether_data::Kind>::NAME,
        }
    }
}

#[runtime]
impl NativeActor for WindowInstance {
    type State = WindowInstanceState;
    type Config = ();

    const NAMESPACE: &'static str = crate::WINDOW_INSTANCE_NAMESPACE;

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<WindowInstanceState, BootError> {
        Ok(WindowInstanceState)
    }

    /// Ask the manager to close this window.
    #[handler::request]
    fn on_close(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: CloseWindow) -> Pending<CloseWindowResult> {
        forward(ctx, WindowCommand::Close, WindowForwardContext::Close)
    }

    /// Ask the manager to change this window's presentation mode.
    #[handler::request]
    fn on_set_mode(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SetWindowMode,
    ) -> Pending<SetWindowModeResult> {
        forward(
            ctx,
            WindowCommand::SetMode { mode: mail.mode, width: mail.width, height: mail.height },
            WindowForwardContext::SetMode,
        )
    }

    /// Ask the manager to retitle this window.
    #[handler::request]
    fn on_set_title(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SetWindowTitle,
    ) -> Pending<SetWindowTitleResult> {
        forward(ctx, WindowCommand::SetTitle { title: mail.title }, WindowForwardContext::SetTitle)
    }

    /// Ask the manager to install this window's native menu bar.
    #[handler::request]
    fn on_set_menu(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SetWindowMenu,
    ) -> Pending<SetWindowMenuResult> {
        forward(ctx, WindowCommand::SetMenu { menus: mail.menus }, WindowForwardContext::SetMenu)
    }

    /// Ask the manager to set this window's pointer shape.
    #[handler::request]
    fn on_set_cursor(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SetWindowCursor,
    ) -> Pending<SetWindowCursorResult> {
        forward(ctx, WindowCommand::SetCursor { icon: mail.icon }, WindowForwardContext::SetCursor)
    }

    /// Ask the manager to bring this window to the foreground.
    #[handler::request]
    fn on_focus(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: FocusWindow) -> Pending<FocusWindowResult> {
        forward(ctx, WindowCommand::Focus, WindowForwardContext::Focus)
    }

    /// Ask the manager to schedule this window for redraw.
    #[handler::request]
    fn on_request_redraw(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        _mail: RequestWindowRedraw,
    ) -> Pending<RequestWindowRedrawResult> {
        forward(ctx, WindowCommand::RequestRedraw, WindowForwardContext::RequestRedraw)
    }

    /// Answer the held public request the manager just resolved.
    #[handler::response]
    fn on_command_result(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: ApplyWindowCommandResult) {
        complete(ctx, result);
    }

    /// Shut down: the manager is retiring this endpoint.
    #[handler::tell]
    fn on_retire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: RetireWindow) {
        ctx.shutdown();
    }
}

#[cfg(all(test, feature = "synthetic"))]
mod tests {
    use aether_data::LoadName;

    use crate::runtime::subscribers::fixture::Rig;
    use crate::{
        ApplyWindowCommandResult, CreateWindow, CreateWindowResult, SetWindowTitle, SetWindowTitleResult,
        WindowCapability, WindowInstance, WindowMode, WindowSpec,
    };

    /// Any actor may send a window endpoint an `ApplyWindowCommandResult`, so
    /// one that answers no forward must not stop the endpoint or the engine:
    /// the stray result is dropped, and the window still answers its next
    /// command. Fails if a result with no stored context aborts, or leaves the
    /// endpoint unable to answer.
    #[test]
    fn a_stray_manager_result_is_dropped_and_the_window_still_answers() {
        let mut rig = Rig::synthetic();
        let spec =
            WindowSpec { name: "main".to_owned(), title: "Main".to_owned(), mode: WindowMode::Windowed, size: None };
        rig.send(&CreateWindow { spec });
        assert!(matches!(rig.reply::<CreateWindowResult>(), CreateWindowResult::Ok { .. }), "the window opens");
        let main = rig
            .chassis()
            .child::<WindowCapability, WindowInstance>(rig.manager(), LoadName::new("main").expect("fixture name"))
            .expect("the window child is live");

        let stray = ApplyWindowCommandResult::SetTitle(SetWindowTitleResult::Ok { title: "stray".to_owned() });
        rig.send_to(main, &stray);
        rig.send_to(main, &SetWindowTitle { title: "after".to_owned() });

        assert_eq!(
            rig.reply::<SetWindowTitleResult>(),
            SetWindowTitleResult::Ok { title: "after".to_owned() },
            "the window answers its next command with the manager's result",
        );
    }
}
