//! The mail surface every concrete window manager carries (ADR-0169).

use aether_actor::{Manual, OutboundReply, handler_set};
use aether_data::{Kind, MailboxId};
use aether_substrate::actor::native::NativeCtx;

use super::subscribers::{WindowSubscribers, validate_subscriber_mailbox};
use crate::{
    CloseWindow, CloseWindowResult, FocusWindow, FocusWindowResult, RequestWindowRedraw, RequestWindowRedrawResult,
    SetWindowMode, SetWindowModeResult, SetWindowTitle, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult,
    SubscribeWindowSelf, UnsubscribeAllWindows, UnsubscribeWindow, UnsubscribeWindowSelf, WindowId,
};

/// Re-dispatch one root-addressed per-window command at the sole live window,
/// answering the *original* requester rather than this manager.
///
/// The five command kinds are the window endpoint's (`runtime::instance`), so
/// the root owns no copy of their semantics: it forwards the request verbatim
/// with the requester's own `reply_to` pinned, and the endpoint's existing
/// retain-and-answer plumbing replies straight to the caller under the
/// caller's correlation. Every per-window consequence the endpoint owns — a
/// close retiring its own actor — still happens, and the manager keeps no
/// correlation state.
///
/// `Err` carries the refusal text for the two ambiguous cases, which the
/// caller receives as the command's own `Err` variant rather than as silence.
fn route_to_sole_window<K: Kind>(
    windows: &[WindowId],
    ctx: &mut NativeCtx<'_, Manual>,
    mail: &K,
) -> Result<(), String> {
    let window = match windows {
        [window] => *window,
        [] => return Err(format!("{} reached the aether.window root, which has no live window", K::NAME)),
        several => {
            return Err(format!(
                "{} reached the aether.window root, but {} windows are live — address one window's own mailbox \
                 instead (aether.window.list reports each window's id)",
                K::NAME,
                several.len(),
            ));
        }
    };
    let _ = ctx.send_envelope_tracked_with_reply_to(
        MailboxId(window.0),
        K::ID,
        &mail.encode_into_bytes(),
        ctx.reply_target(),
    );
    Ok(())
}

/// The shared receive surface of a concrete window manager (ADR-0169).
///
/// Two blocks of behavior are properties of *being* the `aether.window` root
/// rather than of any one chassis, so a concrete manager contributes only the
/// two accessors below and inherits both.
///
/// Subscription: who may subscribe, how a selector is stored, and which errors
/// come back are properties of [`WindowSubscribers`]. Event *publication* stays
/// with the manager — what counts as an event, and when, is exactly where
/// desktop and synthetic differ.
///
/// Root-addressed commands: the per-window command kinds are handled by the
/// window endpoint, so the root used to drop them silently
/// (iamacoffeepot/aether#5505). It routes each to the sole window when the
/// engine has exactly one — the overwhelmingly common case, and the one the
/// documented surface assumes — and otherwise answers the command's `Err`
/// variant naming the situation. Which windows are routable is the manager's
/// call; the routing and the refusals are not.
#[handler_set]
pub trait WindowManagerSurface {
    /// The manager's subscription table.
    fn subscribers(state: &mut Self::State) -> &mut WindowSubscribers;

    /// Every window a root-addressed command may be routed to — the same set
    /// `aether.window.list` enumerates, so the count a refusal reports is the
    /// count the caller can see. Per-window liveness stays the endpoint's
    /// answer, not a reason to hide a window from the root's arithmetic.
    fn routable_windows(state: &Self::State) -> Vec<WindowId>;

    /// Subscribe an explicit mailbox to one kind for one selector.
    #[handler::single]
    fn on_subscribe(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SubscribeWindow) -> SubscribeWindowResult {
        if let Err(error) = validate_subscriber_mailbox(ctx, mail.mailbox) {
            return SubscribeWindowResult::Err { error };
        }
        Self::subscribers(state).subscribe(ctx, mail.selector, mail.kind, mail.mailbox);
        SubscribeWindowResult::Ok
    }

    /// Subscribe the calling actor to one kind for one selector.
    #[handler::single]
    fn on_subscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        match Self::subscribers(state).subscribe_self(ctx, mail.selector, mail.kind) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::Err { error },
        }
    }

    /// Drop an explicit mailbox's subscription to one kind for one selector.
    #[handler::single]
    fn on_unsubscribe(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: UnsubscribeWindow,
    ) -> SubscribeWindowResult {
        if let Err(error) = validate_subscriber_mailbox(ctx, mail.mailbox) {
            return SubscribeWindowResult::Err { error };
        }
        Self::subscribers(state).unsubscribe(mail.selector, mail.kind, mail.mailbox);
        SubscribeWindowResult::Ok
    }

    /// Drop the calling actor's subscription to one kind for one selector.
    #[handler::single]
    fn on_unsubscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: UnsubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        match Self::subscribers(state).unsubscribe_self(ctx, mail.selector, mail.kind) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::Err { error },
        }
    }

    /// Drop a mailbox from every window-event subscription it holds.
    #[handler::single]
    fn on_unsubscribe_all(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: UnsubscribeAllWindows) {
        Self::subscribers(state).unsubscribe_all(mail.mailbox);
    }

    /// Close the sole window.
    #[handler::manual]
    fn on_close(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: CloseWindow) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&CloseWindowResult::Err { error });
        }
    }

    /// Change the sole window's presentation mode.
    #[handler::manual]
    fn on_set_mode(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: SetWindowMode) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&SetWindowModeResult::Err { error });
        }
    }

    /// Change the sole window's title.
    #[handler::manual]
    fn on_set_title(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: SetWindowTitle) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&SetWindowTitleResult::Err { error });
        }
    }

    /// Bring the sole window to the foreground.
    #[handler::manual]
    fn on_focus(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: FocusWindow) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&FocusWindowResult::Err { error });
        }
    }

    /// Schedule the sole window for redraw.
    #[handler::manual]
    fn on_request_redraw(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: RequestWindowRedraw) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&RequestWindowRedrawResult::Err { error });
        }
    }
}
