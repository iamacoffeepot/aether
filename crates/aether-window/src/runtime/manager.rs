//! Root-command routing: the command view a manager retains of each window
//! child, and the forward of a root-addressed command to the sole window.

use aether_actor::{Protocol, ProtocolRef, ReplyMode, RowAt, protocol};
use aether_data::{ActorMail, ErasedActorPath};
use aether_substrate::actor::native::NativeCtx;

use crate::{
    CloseWindow, CloseWindowResult, FocusWindow, FocusWindowResult, RequestWindowRedraw, RequestWindowRedrawResult,
    SetWindowCursor, SetWindowCursorResult, SetWindowMenu, SetWindowMenuResult, SetWindowMode, SetWindowModeResult,
    SetWindowPresentation, SetWindowPresentationResult, SetWindowTitle, SetWindowTitleResult,
};

/// The eight command rows a concrete window endpoint exposes, each naming the
/// reply its endpoint holds and answers later. A manager retains only this
/// view of each successfully published child, so a root forward cannot select
/// a kind outside the shared endpoint surface.
#[protocol]
pub trait WindowCommands {
    fn close(mail: CloseWindow) -> CloseWindowResult;
    fn set_mode(mail: SetWindowMode) -> SetWindowModeResult;
    fn set_presentation(mail: SetWindowPresentation) -> SetWindowPresentationResult;
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
/// The eight command kinds are the window endpoint's (`runtime::instance`), so
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
pub(super) fn route_to_sole_window<K: ActorMail, A, S, I, M: ReplyMode>(
    windows: &[RoutableWindow],
    ctx: &mut NativeCtx<'_, A, S, M>,
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

#[cfg(all(test, feature = "synthetic"))]
mod tests {
    use aether_data::LoadName;
    use aether_substrate::testing::await_settled;

    use super::*;
    use crate::runtime::subscribers::fixture::Rig;
    use crate::{CreateWindow, CreateWindowResult, RetireWindow, WindowCapability, WindowInstance};
    use crate::{WindowMode, WindowPresentation, WindowSpec};

    /// The sole window's child departs while a root command for it is
    /// already queued behind the departure: the window stays listed until
    /// its `MonitorNotice` is processed, but the command must not be
    /// forwarded into the dead mailbox, where it would settle with no reply.
    /// Fails if the root forwards without proving the sole window live.
    #[test]
    fn sole_window_departure_is_refused_before_its_monitor_notice_is_processed() {
        let mut rig = Rig::synthetic();
        let spec = WindowSpec {
            name: "main".to_owned(),
            title: "Main".to_owned(),
            mode: WindowMode::Windowed,
            size: None,
            presentation: WindowPresentation::Display,
        };
        rig.send(&CreateWindow { spec });
        let CreateWindowResult::Ok { window } = rig.reply::<CreateWindowResult>() else {
            panic!("the synthetic manager creates the window");
        };
        let main_child = |rig: &Rig<WindowCapability>| {
            rig.chassis()
                .child::<WindowCapability, WindowInstance>(rig.manager(), LoadName::new("main").expect("fixture name"))
        };
        let child = main_child(&rig).expect("the window child is live");

        let too_late = rig.push(&SetWindowTitle { title: "too late".to_owned() });
        let (_, retired) = rig.chassis().send_tracked(child, &RetireWindow, None);
        await_settled(&retired, "the child retires");
        rig.chassis().await_closed(child.erase());
        assert!(main_child(&rig).is_err(), "the retired child's route is dropped");
        rig.driver.settle(&[too_late]);

        let SetWindowTitleResult::Err { error } = rig.reply::<SetWindowTitleResult>() else {
            panic!("a dead child remains listed until its monitor notice, but cannot receive a root command");
        };
        assert!(error.contains(window.path.as_str()), "the refusal names the window: {error}");
        assert!(error.contains("not live"), "the refusal says why: {error}");
    }
}
