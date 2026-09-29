//! The mail surface every concrete window manager carries (ADR-0169).

use aether_actor::{Manual, OutboundReply, Protocol, ProtocolRef, RowAt, handler_set, protocol};
use aether_data::{ActorMail, ErasedActorPath};
use aether_substrate::actor::native::{Erased, NativeCtx};

use super::subscribers::WindowSubscribers;
use crate::{
    CloseWindow, CloseWindowResult, FocusWindow, FocusWindowResult, RequestWindowRedraw, RequestWindowRedrawResult,
    SetWindowCursor, SetWindowCursorResult, SetWindowMenu, SetWindowMenuResult, SetWindowMode, SetWindowModeResult,
    SetWindowTitle, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult, SubscribeWindowSelf,
    UnsubscribeWindow, UnsubscribeWindowSelf,
};

/// The seven command rows a concrete window endpoint exposes, each naming the
/// reply its endpoint holds and answers later. A manager retains only this
/// view of each successfully published child, so a root forward cannot select
/// a kind outside the shared endpoint surface.
#[protocol]
pub trait WindowCommands {
    fn close(mail: CloseWindow) -> CloseWindowResult;
    fn set_mode(mail: SetWindowMode) -> SetWindowModeResult;
    fn set_title(mail: SetWindowTitle) -> SetWindowTitleResult;
    fn set_menu(mail: SetWindowMenu) -> SetWindowMenuResult;
    fn set_cursor(mail: SetWindowCursor) -> SetWindowCursorResult;
    fn focus(mail: FocusWindow) -> FocusWindowResult;
    fn request_redraw(mail: RequestWindowRedraw) -> RequestWindowRedrawResult;
}

/// One listed window and the retained command proof for its current child.
/// A missing proof means the child has departed while the manager still lists
/// the window (the desktop closing interval); it remains in cardinality and
/// is refused for liveness when it is the sole entry.
pub struct RoutableWindow {
    pub path: ErasedActorPath,
    pub target: Option<ProtocolRef<WindowCommands>>,
}

/// Re-dispatch one root-addressed per-window command at the sole live window,
/// answering the *original* requester rather than this manager.
///
/// The seven command kinds are the window endpoint's (`runtime::instance`), so
/// the root owns no copy of their semantics: it proves the sole window live
/// (ADR-0230) and forwards the request verbatim through that proof with the
/// requester's own `reply_to` pinned, and the reply the endpoint holds
/// captures that pinned target, so its answer goes straight to the caller
/// under the caller's correlation. Every per-window consequence the endpoint owns — a
/// close retiring its own actor — still happens, and the manager keeps no
/// correlation state.
///
/// `Err` carries the refusal text for the two ambiguous cases and for a sole
/// window that is no longer live, which the caller receives as the command's
/// own `Err` variant rather than as silence or a forward into a dead mailbox.
fn route_to_sole_window<K: ActorMail, A, I>(
    windows: &[RoutableWindow],
    ctx: &mut NativeCtx<'_, A, Manual>,
    mail: &K,
) -> Result<(), String>
where
    <WindowCommands as Protocol>::Rows: RowAt<K, I>,
{
    let window = match windows {
        [window] => window,
        [] => return Err(format!("{} reached the aether.window root, which has no live window", K::NAME)),
        several => {
            return Err(format!(
                "{} reached the aether.window root, but {} windows are live — address one window's own mailbox \
                 instead (aether.window.list reports each window's path)",
                K::NAME,
                several.len(),
            ));
        }
    };
    ctx.resolve_path(&window.path).map_err(|error| {
        format!("{} reached the aether.window root, but window {} is not live: {error}", K::NAME, window.path)
    })?;
    let target = window
        .target
        .ok_or_else(|| format!("{} reached the aether.window root, but window {} is not live", K::NAME, window.path))?;
    ctx.forward_to(target, mail);
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
    fn routable_windows(state: &Self::State) -> Vec<RoutableWindow>;

    /// Subscribe an explicitly named actor to one kind for one selector.
    ///
    /// The subscriber's path reached this handler only because its decode
    /// proved the live route there handles the kind silently (ADR-0231 §3);
    /// it is proven live once more here, at receipt, and the table keeps the
    /// `ProtocolRef<Subscriber<K>>` that proof returns. A path whose actor
    /// has gone answers `Err` naming it.
    #[handler::single]
    fn on_subscribe(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SubscribeWindow) -> SubscribeWindowResult {
        match Self::subscribers(state).subscribe_path(ctx, mail.selector, &mail.subscription) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::Err { error: error.to_string() },
        }
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

    /// Drop an explicitly named actor's subscription to one kind for one
    /// selector. The path is proven live at receipt and its key removed; a
    /// path whose actor has gone answers `Err` naming it.
    #[handler::single]
    fn on_unsubscribe(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: UnsubscribeWindow,
    ) -> SubscribeWindowResult {
        match Self::subscribers(state).unsubscribe_path(ctx, mail.selector, &mail.subscription) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::Err { error: error.to_string() },
        }
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

    /// Close the sole window.
    #[handler::manual]
    fn on_close(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, mail: CloseWindow) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&CloseWindowResult::Err { error });
        }
    }

    /// Change the sole window's presentation mode.
    #[handler::manual]
    fn on_set_mode(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, mail: SetWindowMode) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&SetWindowModeResult::Err { error });
        }
    }

    /// Change the sole window's title.
    #[handler::manual]
    fn on_set_title(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, mail: SetWindowTitle) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&SetWindowTitleResult::Err { error });
        }
    }

    /// Install the sole window's native menu bar.
    #[handler::manual]
    fn on_set_menu(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, mail: SetWindowMenu) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&SetWindowMenuResult::Err { error });
        }
    }

    /// Set the sole window's pointer shape.
    #[handler::manual]
    fn on_set_cursor(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, mail: SetWindowCursor) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&SetWindowCursorResult::Err { error });
        }
    }

    /// Bring the sole window to the foreground.
    #[handler::manual]
    fn on_focus(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, mail: FocusWindow) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&FocusWindowResult::Err { error });
        }
    }

    /// Schedule the sole window for redraw.
    #[handler::manual]
    fn on_request_redraw(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Manual>, mail: RequestWindowRedraw) {
        if let Err(error) = route_to_sole_window(&Self::routable_windows(state), ctx, &mail) {
            ctx.reply(&RequestWindowRedrawResult::Err { error });
        }
    }
}

#[cfg(all(test, feature = "synthetic"))]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use aether_data::LoadName;
    use aether_substrate::testing::await_settled;

    use super::*;
    use crate::runtime::subscribers::fixture::Rig;
    use crate::{CreateWindow, CreateWindowResult, RetireWindow, SyntheticWindowCapability, SyntheticWindowInstance};
    use crate::{WindowMode, WindowSpec};

    /// The sole window's child departs while a root command for it is
    /// already queued behind the departure: the window stays listed until
    /// its `MonitorNotice` is processed, but the command must not be
    /// forwarded into the dead mailbox, where it would settle with no reply.
    /// Fails if the root forwards without proving the sole window live.
    #[test]
    fn sole_window_departure_is_refused_before_its_monitor_notice_is_processed() {
        let mut rig = Rig::<SyntheticWindowCapability>::boot(());
        let spec =
            WindowSpec { name: "main".to_owned(), title: "Main".to_owned(), mode: WindowMode::Windowed, size: None };
        rig.send(&CreateWindow { spec });
        let CreateWindowResult::Ok { window } = rig.reply::<CreateWindowResult>() else {
            panic!("the synthetic manager creates the window");
        };
        let main_child = |rig: &Rig<SyntheticWindowCapability>| {
            rig.chassis().child::<SyntheticWindowCapability, SyntheticWindowInstance>(
                rig.manager(),
                LoadName::new("main").expect("fixture name"),
            )
        };
        let child = main_child(&rig).expect("the window child is live");

        let too_late = rig.push(&SetWindowTitle { title: "too late".to_owned() });
        let (_, retired) = rig.chassis().send_tracked(child, &RetireWindow, None);
        await_settled(&retired, "the child retires");
        let deadline = Instant::now() + Duration::from_secs(2);
        while main_child(&rig).is_ok() {
            assert!(Instant::now() < deadline, "the child's route did not drop within the deadline");
            thread::sleep(Duration::from_millis(1));
        }
        rig.driver.settle(&[too_late]);

        let SetWindowTitleResult::Err { error } = rig.reply::<SetWindowTitleResult>() else {
            panic!("a dead child remains listed until its monitor notice, but cannot receive a root command");
        };
        assert!(error.contains(window.path.as_str()), "the refusal names the window: {error}");
        assert!(error.contains("not live"), "the refusal says why: {error}");
    }
}
