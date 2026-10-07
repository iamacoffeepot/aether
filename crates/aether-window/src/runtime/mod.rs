//! The `aether.window` manager runtime: one receive surface over the backend
//! [`WindowParams`] picks at boot.
//!
//! A backend is compiled in by its feature and chosen by `Params`, never by the
//! feature alone: cargo unifies features across a build, so a workspace build
//! compiles both `desktop` (for the desktop chassis) and `synthetic` (for the
//! harnesses) into one crate, and only the composer knows which it wants.

use aether_actor::{Anyone, OutboundReply, PathRefused, Unchecked, runtime};
use aether_data::ErasedActorPath;
use aether_kinds::MonitorNotice;
use aether_substrate::actor::native::{Erased, Pending, SpawnOutcome, TaskDone};

use crate::{
    ApplyWindowCommand, ApplyWindowCommandResult, CloseWindow, CloseWindowResult, CreateWindow, CreateWindowResult,
    FocusWindow, FocusWindowResult, KeyFocusGained, KeyFocusHolder, KeyFocusLost, ListWindows, ListWindowsResult,
    ReleaseKeyFocus, RequestWindowRedraw, RequestWindowRedrawResult, SetWindowCursor, SetWindowCursorResult,
    SetWindowMenu, SetWindowMenuResult, SetWindowMode, SetWindowModeResult, SetWindowPresentation,
    SetWindowPresentationResult, SetWindowTitle, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult,
    SubscribeWindowSelf, TakeKeyFocus, UnsubscribeWindow, UnsubscribeWindowSelf, WindowCapability, WindowInstance,
};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

#[cfg(feature = "desktop")]
pub mod desktop;
#[cfg(feature = "synthetic")]
pub mod synthetic;

mod instance;
mod manager;
mod routing;
mod subscribers;

use self::manager::{RoutableWindow, route_to_sole_window};
use self::routing::key_focus::Take;
use self::subscribers::WindowSubscribers;

/// The backend a [`WindowCapability`] runs, chosen by its composer.
pub enum WindowParams {
    /// The winit desktop manager. Its boot value has no public constructor:
    /// only [`DesktopWindowSlot::boot`](desktop::DesktopWindowSlot::boot)
    /// mints one, so the desktop backend always boots pumped on the
    /// application thread whose host turns realize its native work.
    #[cfg(feature = "desktop")]
    Desktop(desktop::DesktopWindowBoot),
    /// The deterministic in-memory manager harness tests drive.
    #[cfg(feature = "synthetic")]
    Synthetic,
}

/// Runtime state for [`WindowCapability`]: the backend it runs.
pub struct WindowCapabilityState {
    backend: WindowBackend,
}

/// Each backend's state is boxed: the two differ in size by the desktop's
/// native-window maps, and one state lives for the manager's whole life.
enum WindowBackend {
    #[cfg(feature = "desktop")]
    Desktop(Box<desktop::DesktopWindows>),
    #[cfg(feature = "synthetic")]
    Synthetic(Box<synthetic::SyntheticWindows>),
}

/// The context a staged window child carries into its task completion
/// (ADR-0243 §9): the window's path, which keys its pending create in either
/// backend.
#[aether_data::kind(name = "aether.window.spawn_key")]
struct WindowSpawnKey {
    path: ErasedActorPath,
}

impl WindowCapabilityState {
    /// The manager's subscription table, for a test to read.
    #[cfg(all(test, feature = "synthetic"))]
    fn subscribers(&self) -> &WindowSubscribers {
        match &self.backend {
            #[cfg(feature = "desktop")]
            WindowBackend::Desktop(windows) => &windows.subscribers,
            #[cfg(feature = "synthetic")]
            WindowBackend::Synthetic(windows) => &windows.subscribers,
        }
    }

    /// The manager's subscription table, mutably.
    fn subscribers_mut(&mut self) -> &mut WindowSubscribers {
        match &mut self.backend {
            #[cfg(feature = "desktop")]
            WindowBackend::Desktop(windows) => &mut windows.subscribers,
            #[cfg(feature = "synthetic")]
            WindowBackend::Synthetic(windows) => &mut windows.subscribers,
        }
    }

    /// Every window a root-addressed command may be routed to — the same set
    /// `aether.window.list` enumerates, so the count a refusal reports is the
    /// count the caller can see. Per-window liveness stays the endpoint's
    /// answer, not a reason to hide a window from the root's arithmetic.
    fn routable_windows(&self) -> Vec<RoutableWindow> {
        match &self.backend {
            #[cfg(feature = "desktop")]
            WindowBackend::Desktop(windows) => windows.routable_windows(),
            #[cfg(feature = "synthetic")]
            WindowBackend::Synthetic(windows) => windows.routable_windows(),
        }
    }
}

/// Root-addressed per-window commands (iamacoffeepot/aether#5505): the root
/// routes each to the sole window when the engine has exactly one — the
/// overwhelmingly common case, and the one the documented surface assumes —
/// and otherwise answers the command's `Err` variant naming the situation.
/// Which windows are routable is the backend's call; the routing and the
/// refusals are not.
#[runtime]
impl NativeActor for WindowCapability {
    type State = WindowCapabilityState;
    type Config = ();
    type Params = WindowParams;

    const NAMESPACE: &'static str = crate::WINDOW_NAMESPACE;

    fn init((): (), params: WindowParams, _ctx: &mut NativeInitCtx<'_>) -> Result<WindowCapabilityState, BootError> {
        let backend = match params {
            #[cfg(feature = "desktop")]
            WindowParams::Desktop(boot) => WindowBackend::Desktop(Box::new(desktop::DesktopWindows::new(boot))),
            #[cfg(feature = "synthetic")]
            WindowParams::Synthetic => WindowBackend::Synthetic(Box::new(synthetic::SyntheticWindows::new())),
        };
        Ok(WindowCapabilityState { backend })
    }

    #[handler::request]
    fn on_list(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListWindows) -> ListWindowsResult {
        let windows = match &state.backend {
            #[cfg(feature = "desktop")]
            WindowBackend::Desktop(windows) => windows.list(),
            #[cfg(feature = "synthetic")]
            WindowBackend::Synthetic(windows) => windows.list(),
        };
        ListWindowsResult::Ok { windows }
    }

    #[handler::request]
    fn on_create(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: CreateWindow) -> Pending<CreateWindowResult> {
        let (pending, held) = ctx.hold::<CreateWindowResult>();
        match &mut state.backend {
            #[cfg(feature = "desktop")]
            WindowBackend::Desktop(windows) => windows.create(ctx, mail.spec, held),
            #[cfg(feature = "synthetic")]
            WindowBackend::Synthetic(windows) => windows.create(ctx, mail.spec, held),
        }
        pending
    }

    /// Complete a staged window child's birth, keyed by the path its context
    /// carries.
    #[handler(task)]
    fn on_window_child_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome<WindowInstance>>,
    ) {
        let Some(WindowSpawnKey { path }) = ctx.take_context() else {
            return;
        };
        let outcome = done.into_output();
        match &mut state.backend {
            #[cfg(feature = "desktop")]
            WindowBackend::Desktop(windows) => windows.finish_window_child_spawn(ctx, &path, &outcome),
            #[cfg(feature = "synthetic")]
            WindowBackend::Synthetic(windows) => windows.finish_window_child_spawn(ctx, &path, outcome),
        }
    }

    /// Apply one per-window command a live window child forwarded, at the
    /// window that child is. The answer rides the child's held reply: at once
    /// for every command the backend applies on this turn, later for a
    /// desktop close, which is answered once its native window is detached,
    /// and a desktop presentation change, answered once render has taken or
    /// refused it.
    #[handler::request]
    fn on_apply_command(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: ApplyWindowCommand,
    ) -> Pending<ApplyWindowCommandResult> {
        let (pending, held) = ctx.hold::<ApplyWindowCommandResult>();
        match &mut state.backend {
            #[cfg(feature = "desktop")]
            WindowBackend::Desktop(windows) => windows.apply_command(ctx, mail.command, held),
            #[cfg(feature = "synthetic")]
            WindowBackend::Synthetic(windows) => windows.apply_command(ctx, mail.command, held),
        }
        pending
    }

    /// Subscribe an explicitly named actor to one kind for one selector.
    ///
    /// The subscriber's path reached this handler only because its decode
    /// proved the route there, live or closed, handles the kind silently
    /// (ADR-0231 §3); it is proven live here, at receipt, and the table keeps
    /// the `ProtocolRef<Subscriber<K>>` that proof returns. A path that did
    /// not prove at decode, or whose actor has gone, answers
    /// `Err(Subscriber(..))` naming it.
    #[handler::request]
    fn on_subscribe(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SubscribeWindow) -> SubscribeWindowResult {
        match state.subscribers_mut().subscribe_path(ctx, mail.selector, &mail.subscription) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => PathRefused::from(error).into(),
        }
    }

    /// Subscribe the calling actor to one kind for one selector.
    #[handler::request]
    fn on_subscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        match state.subscribers_mut().subscribe_self(ctx, mail.selector, mail.kind) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::rejected(error),
        }
    }

    /// Drop an explicitly named actor's subscription to one kind for one
    /// selector. The path is proven live at receipt and its key removed; a
    /// path whose actor has gone answers `Err(Subscriber(..))` naming it.
    #[handler::request]
    fn on_unsubscribe(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: UnsubscribeWindow,
    ) -> SubscribeWindowResult {
        match state.subscribers_mut().unsubscribe_path(ctx, mail.selector, &mail.subscription) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => PathRefused::from(error).into(),
        }
    }

    /// Drop the calling actor's subscription to one kind for one selector.
    #[handler::request]
    fn on_unsubscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: UnsubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        match state.subscribers_mut().unsubscribe_self(ctx, mail.selector, mail.kind) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::rejected(error),
        }
    }

    /// Close the sole window.
    #[handler::unchecked(reason = "forwards to the sole window with the requester's reply pinned")]
    fn on_close(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Anyone, Unchecked>, mail: CloseWindow) {
        if let Err(error) = route_to_sole_window(&state.routable_windows(), ctx, &mail) {
            ctx.reply(&CloseWindowResult::Err { error });
        }
    }

    /// Change the sole window's presentation mode.
    #[handler::unchecked(reason = "forwards to the sole window with the requester's reply pinned")]
    fn on_set_mode(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Anyone, Unchecked>, mail: SetWindowMode) {
        if let Err(error) = route_to_sole_window(&state.routable_windows(), ctx, &mail) {
            ctx.reply(&SetWindowModeResult::Err { error });
        }
    }

    /// Change how the sole window presents its frames.
    #[handler::unchecked(reason = "forwards to the sole window with the requester's reply pinned")]
    fn on_set_presentation(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Erased, Anyone, Unchecked>,
        mail: SetWindowPresentation,
    ) {
        if let Err(error) = route_to_sole_window(&state.routable_windows(), ctx, &mail) {
            ctx.reply(&SetWindowPresentationResult::Err { error });
        }
    }

    /// Change the sole window's title.
    #[handler::unchecked(reason = "forwards to the sole window with the requester's reply pinned")]
    fn on_set_title(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Anyone, Unchecked>, mail: SetWindowTitle) {
        if let Err(error) = route_to_sole_window(&state.routable_windows(), ctx, &mail) {
            ctx.reply(&SetWindowTitleResult::Err { error });
        }
    }

    /// Install the sole window's native menu bar.
    #[handler::unchecked(reason = "forwards to the sole window with the requester's reply pinned")]
    fn on_set_menu(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Anyone, Unchecked>, mail: SetWindowMenu) {
        if let Err(error) = route_to_sole_window(&state.routable_windows(), ctx, &mail) {
            ctx.reply(&SetWindowMenuResult::Err { error });
        }
    }

    /// Set the sole window's pointer shape.
    #[handler::unchecked(reason = "forwards to the sole window with the requester's reply pinned")]
    fn on_set_cursor(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Erased, Anyone, Unchecked>,
        mail: SetWindowCursor,
    ) {
        if let Err(error) = route_to_sole_window(&state.routable_windows(), ctx, &mail) {
            ctx.reply(&SetWindowCursorResult::Err { error });
        }
    }

    /// Bring the sole window to the foreground.
    #[handler::unchecked(reason = "forwards to the sole window with the requester's reply pinned")]
    fn on_focus(state: &mut Self::State, ctx: &mut NativeCtx<'_, Erased, Anyone, Unchecked>, mail: FocusWindow) {
        if let Err(error) = route_to_sole_window(&state.routable_windows(), ctx, &mail) {
            ctx.reply(&FocusWindowResult::Err { error });
        }
    }

    /// Schedule the sole window for redraw.
    #[handler::unchecked(reason = "forwards to the sole window with the requester's reply pinned")]
    fn on_request_redraw(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Erased, Anyone, Unchecked>,
        mail: RequestWindowRedraw,
    ) {
        if let Err(error) = route_to_sole_window(&state.routable_windows(), ctx, &mail) {
            ctx.reply(&RequestWindowRedrawResult::Err { error });
        }
    }

    /// Publish an injected event as the published kind it names, through the
    /// running backend's subscription table, routed as an event the backend
    /// raised itself is. A kind the window does not publish, or a payload
    /// that does not decode as the kind, warns and sends nothing.
    #[cfg(feature = "synthetic")]
    #[handler::tell]
    fn on_inject(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: crate::InjectWindowEvent) {
        if let Err(error) = state.subscribers_mut().publish_encoded(ctx, &mail.window, mail.kind, &mail.payload) {
            tracing::warn!(target: "aether_window", window = %mail.window, %error, "injected window event not published");
        }
    }

    /// Give the sending actor key focus in the window the mail names
    /// (ADR-0248 §9). The latest take wins: the actor it replaces is sent
    /// `KeyFocusLost` and the sender `KeyFocusGained`, both naming the
    /// window. A take by the window's holder changes its scope and sends
    /// nothing. The mail's `window` is an `ActorPath<WindowInstance>`, so a
    /// path that cannot name a window fails the mail's decode and never
    /// reaches this handler. The type says nothing about liveness: any window
    /// path is accepted, open or not, so the take cannot fail, and a take for
    /// a name no window ever opens under holds a slot until its holder
    /// releases it or departs.
    ///
    /// The ctx's sender is the requirement (ADR-0231 §11): an actor sends
    /// this kind only when it covers [`KeyFocusHolder`], and the engine casts
    /// the sender before this handler runs, so a slot's holder always handles
    /// both notices.
    ///
    /// # Agent
    /// No reply. Mail with no actor sender, which is what an MCP `send_mail`
    /// is, and mail from a sender that does not handle both notices are
    /// refused before this handler runs; mail the actor that should hold the
    /// keys and let it take. Taking key focus does not raise or focus the
    /// window: `aether.window.focus` does that.
    #[handler::tell]
    fn on_take_key_focus(state: &mut Self::State, ctx: &mut NativeCtx<'_, Self, KeyFocusHolder>, mail: TakeKeyFocus) {
        let holder = ctx.sender();
        let TakeKeyFocus { window, scope } = mail;

        if let Take::Gained { replaced } = state.subscribers_mut().take_key_focus(ctx, &window, holder, scope) {
            if let Some(replaced) = replaced {
                ctx.send_to(replaced, &KeyFocusLost { window: window.clone() });
            }
            ctx.send_to(holder, &KeyFocusGained { window });
        }
    }

    /// Empty the key focus slot of the window the mail names, when the
    /// sending actor holds it, and send it `KeyFocusLost` naming the window.
    /// Nothing is handed back. A release from any other actor changes nothing
    /// and sends nothing.
    ///
    /// # Agent
    /// No reply. Refused before this handler runs as a take is.
    #[handler::tell]
    fn on_release_key_focus(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_, Self, KeyFocusHolder>,
        mail: ReleaseKeyFocus,
    ) {
        let sender = ctx.sender();

        if state.subscribers_mut().release_key_focus(&mail.window, sender.erase()) {
            ctx.send_to(sender, &KeyFocusLost { window: mail.window });
        }
    }

    /// A monitored actor departed: a window child, whose window the backend
    /// retires, or a watched actor, whose every row is dropped and whose
    /// every key focus slot is emptied.
    #[handler::event]
    fn on_monitor_notice(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        let Some(departed) = ctx.sender() else {
            return;
        };
        match &mut state.backend {
            #[cfg(feature = "desktop")]
            WindowBackend::Desktop(windows) => windows.child_departed(departed),
            #[cfg(feature = "synthetic")]
            WindowBackend::Synthetic(windows) => windows.child_departed(ctx, departed),
        }
        state.subscribers_mut().unsubscribe_all(departed);
    }
}
