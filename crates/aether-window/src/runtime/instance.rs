//! Shared named-window forwarding plus the fail-fast headless runtime.

use aether_actor::runtime;
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_actor::{DependsOn, HeldReply, ReplyMode, handler_set};
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_substrate::actor::native::{Held, Pending};

use super::{BootError, NativeActor, NativeCtx, NativeInitCtx, unsupported};
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use crate::{ApplyWindowCommand, ApplyWindowCommandResult, RetireWindow, WindowCapability, WindowCommand};
use crate::{
    CloseWindow, CloseWindowResult, FocusWindow, FocusWindowResult, HeadlessWindowInstance, RequestWindowRedraw,
    RequestWindowRedrawResult, SetWindowCursor, SetWindowCursorResult, SetWindowMenu, SetWindowMenuResult,
    SetWindowMode, SetWindowModeResult, SetWindowTitle, SetWindowTitleResult,
};

/// State of one concrete forwarding child. It keeps none: each public
/// request's held reply rides the context stored under its forward.
#[cfg(any(feature = "desktop", feature = "synthetic"))]
pub struct WindowInstanceState;

/// The context stored under a forwarded [`ApplyWindowCommand`]: the public
/// request's held reply (ADR-0243 §4), in the variant naming its command. The
/// completion claims it back and answers it with the manager's result.
///
/// Only a window-bearing runtime forwards, so it carries the same gate as
/// the handlers that store it. It is a reply context, never mail, so it stays
/// private beside its only user.
#[cfg(any(feature = "desktop", feature = "synthetic"))]
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

#[cfg(any(feature = "desktop", feature = "synthetic"))]
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
#[cfg(any(feature = "desktop", feature = "synthetic"))]
fn forward<A: DependsOn<WindowCapability>, R: HeldReply>(
    ctx: &mut NativeCtx<'_, A>,
    command: WindowCommand,
    context: fn(Held<R>) -> WindowForwardContext,
) -> Pending<R> {
    let (pending, held) = ctx.hold::<R>();
    let _ = ctx.send_with_context::<WindowCapability>(&ApplyWindowCommand { command }, context(held));
    pending
}

/// Answer the public request the manager just resolved, from the held reply
/// its forward's context carries. A successful close retires this endpoint.
#[cfg(any(feature = "desktop", feature = "synthetic"))]
fn complete<A, M: ReplyMode>(ctx: &mut NativeCtx<'_, A, M>, result: ApplyWindowCommandResult) {
    let Some(context) = ctx.take_context::<WindowForwardContext>() else {
        ctx.fatal_abort("window child received an uncorrelated manager result".to_owned());
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
            ctx.fatal_abort(format!("window child manager result {result:?} does not match retained {context:?}"))
        }
    }
}

#[cfg(any(feature = "desktop", feature = "synthetic"))]
fn retire<A, M: ReplyMode>(ctx: &mut NativeCtx<'_, A, M>, _mail: RetireWindow) {
    ctx.shutdown();
}

/// The whole receive surface of a pooled window endpoint (ADR-0169).
///
/// A concrete endpoint — desktop or synthetic — differs from its sibling only
/// in which manager it forwards to, and the manager is reached through
/// [`WindowCapability`], the neutral alias. So the seven handlers are identical
/// across the family down to the token, and an adopter contributes only its
/// identity.
#[cfg(any(feature = "desktop", feature = "synthetic"))]
#[handler_set]
pub trait WindowEndpoint: DependsOn<WindowCapability> {
    /// Ask the manager to close this window.
    #[handler::single]
    fn on_close(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: CloseWindow) -> Pending<CloseWindowResult> {
        forward(ctx, WindowCommand::Close, WindowForwardContext::Close)
    }

    /// Ask the manager to change this window's presentation mode.
    #[handler::single]
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
    #[handler::single]
    fn on_set_title(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SetWindowTitle,
    ) -> Pending<SetWindowTitleResult> {
        forward(ctx, WindowCommand::SetTitle { title: mail.title }, WindowForwardContext::SetTitle)
    }

    /// Ask the manager to install this window's native menu bar.
    #[handler::single]
    fn on_set_menu(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SetWindowMenu,
    ) -> Pending<SetWindowMenuResult> {
        forward(ctx, WindowCommand::SetMenu { menus: mail.menus }, WindowForwardContext::SetMenu)
    }

    /// Ask the manager to set this window's pointer shape.
    #[handler::single]
    fn on_set_cursor(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SetWindowCursor,
    ) -> Pending<SetWindowCursorResult> {
        forward(ctx, WindowCommand::SetCursor { icon: mail.icon }, WindowForwardContext::SetCursor)
    }

    /// Ask the manager to bring this window to the foreground.
    #[handler::single]
    fn on_focus(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, _mail: FocusWindow) -> Pending<FocusWindowResult> {
        forward(ctx, WindowCommand::Focus, WindowForwardContext::Focus)
    }

    /// Ask the manager to schedule this window for redraw.
    #[handler::single]
    fn on_request_redraw(
        _state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        _mail: RequestWindowRedraw,
    ) -> Pending<RequestWindowRedrawResult> {
        forward(ctx, WindowCommand::RequestRedraw, WindowForwardContext::RequestRedraw)
    }

    /// Answer the held public request the manager just resolved.
    #[handler::single]
    fn on_command_result(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: ApplyWindowCommandResult) {
        complete(ctx, result);
    }

    /// Shut down: the manager is retiring this endpoint.
    #[handler::single]
    fn on_retire(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: RetireWindow) {
        retire(ctx, mail);
    }
}

/// Inert state for a named window endpoint on a headless chassis.
pub struct HeadlessWindowInstanceState;

/// The endpoint's half of the headless refusals. Written out here rather than
/// shared with the root's identical seven (`runtime::mod`): a handler set's
/// `HandlesKind` markers travel through a `macro_rules!` bridge that lives with
/// the set, and these two identities compile in the marker-only build where
/// this whole runtime module is `cfg`-ed away — so an inherited handler would
/// take the endpoint's control markers with it and break every typed
/// window-control send from a wasm guest.
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aether_data::Kind;
    use aether_substrate::Registry;
    use aether_substrate::actor::native::{Dispatch, NativeCtx};
    use aether_substrate::mail::Source;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::testing::unrouted_binding;

    use super::super::HeadlessWindowCapabilityState;
    use super::{HeadlessWindowInstanceState, SetWindowCursor, SetWindowCursorResult, SetWindowMenu};
    use crate::{CursorIcon, HeadlessWindowCapability, HeadlessWindowInstance, SetWindowMenuResult};

    /// A window op a headless chassis cannot perform has to come back as that
    /// op's own `Err`, at the root and at a window endpoint alike, because the
    /// alternative is not a no-op — an unhandled kind settles with no reply and
    /// the caller waits out its whole settlement budget for an answer that is
    /// never coming.
    ///
    /// The advertised-surface half is the part a new op actually gets wrong:
    /// the `Err` arms below are easy to remember and the *registration* is not,
    /// and an identity that advertises a kind is an identity that dispatches
    /// it. So the pair — advertised, and answered — is what pins "replies
    /// rather than hangs" without booting a real headless engine.
    #[test]
    fn headless_refuses_the_native_chrome_ops_at_both_identities_rather_than_dropping_them() {
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        let binding = unrouted_binding(&mailer);
        let mut capability_ctx = NativeCtx::new_for_actor(&binding, Source::NONE, None, None);
        let mut instance_ctx = NativeCtx::new_for_actor(&binding, Source::NONE, None, None);

        for advertised in [
            <HeadlessWindowCapability as Dispatch<HeadlessWindowCapabilityState>>::capabilities(),
            <HeadlessWindowInstance as Dispatch<HeadlessWindowInstanceState>>::capabilities(),
        ] {
            let kinds = advertised.handlers.iter().map(|handler| handler.id).collect::<Vec<_>>();
            assert!(kinds.contains(&SetWindowMenu::ID), "the headless identity advertises aether.window.set_menu");
            assert!(kinds.contains(&SetWindowCursor::ID), "the headless identity advertises aether.window.set_cursor");
        }

        assert!(matches!(
            HeadlessWindowCapability::on_set_menu(
                &mut HeadlessWindowCapabilityState,
                &mut capability_ctx,
                SetWindowMenu { menus: Vec::new() },
            ),
            SetWindowMenuResult::Err { .. }
        ));
        assert!(matches!(
            HeadlessWindowInstance::on_set_menu(
                &mut HeadlessWindowInstanceState,
                &mut instance_ctx,
                SetWindowMenu { menus: Vec::new() },
            ),
            SetWindowMenuResult::Err { .. }
        ));
        assert!(matches!(
            HeadlessWindowCapability::on_set_cursor(
                &mut HeadlessWindowCapabilityState,
                &mut capability_ctx,
                SetWindowCursor { icon: CursorIcon::Move },
            ),
            SetWindowCursorResult::Err { .. }
        ));
        assert!(matches!(
            HeadlessWindowInstance::on_set_cursor(
                &mut HeadlessWindowInstanceState,
                &mut instance_ctx,
                SetWindowCursor { icon: CursorIcon::Move },
            ),
            SetWindowCursorResult::Err { .. }
        ));
    }
}
