//! Shared named-window forwarding plus the fail-fast headless runtime.

#[cfg(any(feature = "desktop", feature = "synthetic"))]
use std::collections::HashMap;

use aether_actor::runtime;
#[cfg(any(feature = "desktop", feature = "synthetic"))]
use aether_actor::{Manual, ReplyMode, handler_set};
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
    RequestWindowRedrawResult, SetWindowCursor, SetWindowCursorResult, SetWindowMenu, SetWindowMenuResult,
    SetWindowMode, SetWindowModeResult, SetWindowTitle, SetWindowTitleResult,
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
        ApplyWindowCommandResult::SetMenu(reply) if inbound.kind() == SetWindowMenu::ID => {
            inbound.reply(&reply);
            false
        }
        ApplyWindowCommandResult::SetCursor(reply) if inbound.kind() == SetWindowCursor::ID => {
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
            kind if kind == SetWindowMenu::ID => {
                inbound.reply(&SetWindowMenuResult::Err { error });
            }
            kind if kind == SetWindowCursor::ID => {
                inbound.reply(&SetWindowCursorResult::Err { error });
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

/// The whole receive surface of a pooled window endpoint (ADR-0169).
///
/// A concrete endpoint — desktop or synthetic — differs from its sibling only
/// in which manager it forwards to, and the manager is reached through
/// [`WindowCapability`], the neutral alias. So the seven handlers are identical
/// across the family down to the token, and an adopter contributes only its
/// identity plus the one accessor below.
#[cfg(any(feature = "desktop", feature = "synthetic"))]
#[handler_set]
pub trait WindowEndpoint {
    /// The retained-request state these handlers forward through.
    fn endpoint(state: &mut Self::State) -> &mut WindowInstanceState;

    /// Ask the manager to close this window.
    #[handler::manual]
    fn on_close(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, _mail: CloseWindow) {
        forward(Self::endpoint(state), ctx, WindowCommand::Close);
    }

    /// Ask the manager to change this window's presentation mode.
    #[handler::manual]
    fn on_set_mode(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: SetWindowMode) {
        forward(
            Self::endpoint(state),
            ctx,
            WindowCommand::SetMode { mode: mail.mode, width: mail.width, height: mail.height },
        );
    }

    /// Ask the manager to retitle this window.
    #[handler::manual]
    fn on_set_title(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: SetWindowTitle) {
        forward(Self::endpoint(state), ctx, WindowCommand::SetTitle { title: mail.title });
    }

    /// Ask the manager to install this window's native menu bar.
    #[handler::manual]
    fn on_set_menu(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: SetWindowMenu) {
        forward(Self::endpoint(state), ctx, WindowCommand::SetMenu { menus: mail.menus });
    }

    /// Ask the manager to set this window's pointer shape.
    #[handler::manual]
    fn on_set_cursor(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: SetWindowCursor) {
        forward(Self::endpoint(state), ctx, WindowCommand::SetCursor { icon: mail.icon });
    }

    /// Ask the manager to bring this window to the foreground.
    #[handler::manual]
    fn on_focus(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, _mail: FocusWindow) {
        forward(Self::endpoint(state), ctx, WindowCommand::Focus);
    }

    /// Ask the manager to schedule this window for redraw.
    #[handler::manual]
    fn on_request_redraw(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, _mail: RequestWindowRedraw) {
        forward(Self::endpoint(state), ctx, WindowCommand::RequestRedraw);
    }

    /// Answer the retained public request the manager just resolved.
    #[handler::single]
    fn on_command_result(state: &mut Self::State, ctx: &mut NativeCtx<'_>, result: ApplyWindowCommandResult) {
        complete(Self::endpoint(state), ctx, result);
    }

    /// Shut down: the manager is retiring this endpoint.
    #[handler::single]
    fn on_retire(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: RetireWindow) {
        retire(Self::endpoint(state), ctx, mail);
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
/// `WindowMailboxExt` send from a wasm guest.
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

    use aether_data::{Kind, MailboxId};
    use aether_substrate::Registry;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::actor::native::{Dispatch, NativeCtx};
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::{MailId, Source};

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
        let binding = Arc::new(NativeBinding::new_for_test(mailer, MailboxId(1)));
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

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
                &mut ctx,
                SetWindowMenu { menus: Vec::new() },
            ),
            SetWindowMenuResult::Err { .. }
        ));
        assert!(matches!(
            HeadlessWindowInstance::on_set_menu(
                &mut HeadlessWindowInstanceState,
                &mut ctx,
                SetWindowMenu { menus: Vec::new() },
            ),
            SetWindowMenuResult::Err { .. }
        ));
        assert!(matches!(
            HeadlessWindowCapability::on_set_cursor(
                &mut HeadlessWindowCapabilityState,
                &mut ctx,
                SetWindowCursor { icon: CursorIcon::Move },
            ),
            SetWindowCursorResult::Err { .. }
        ));
        assert!(matches!(
            HeadlessWindowInstance::on_set_cursor(
                &mut HeadlessWindowInstanceState,
                &mut ctx,
                SetWindowCursor { icon: CursorIcon::Move },
            ),
            SetWindowCursorResult::Err { .. }
        ));
    }
}
