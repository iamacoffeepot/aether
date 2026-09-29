//! Deterministic in-memory `aether.window` runtime for substrate harnesses.

mod instance;

use std::collections::{BTreeMap, HashMap};

use aether_actor::{ActorRef, ErasedActorRef, ProtocolRef, runtime};
use aether_data::ErasedActorPath;
use aether_kinds::MonitorNotice;
use aether_substrate::actor::native::{Held, Pending};
use aether_substrate::{MonitorHandle, Subname};

use super::manager::{RoutableWindow, WindowCommands, WindowManagerSurface};
use super::subscribers::{Published, WindowSubscribers};
use crate::{
    ApplyWindowCommand, ApplyWindowCommandResult, CloseWindowResult, CreateWindow, CreateWindowResult,
    FocusWindowResult, InjectWindowEvent, ListWindows, ListWindowsResult, RequestWindowRedrawResult, RetireWindow,
    SetWindowCursorResult, SetWindowMenuResult, SetWindowModeResult, SetWindowTitleResult, SyntheticWindowCapability,
    SyntheticWindowInstance, WindowClosed, WindowCommand, WindowInfo, WindowMode, WindowOpened, WindowSpec,
};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SpawnOutcome, TaskDone};
pub use aether_substrate::chassis::error::BootError;

const DEFAULT_WIDTH: u32 = 800;
const DEFAULT_HEIGHT: u32 = 600;

/// A window whose child actor is staged but not yet authoritatively applied.
///
/// It is keyed by the window's name, which names a reservation, not a live
/// actor, so the window stays out of `windows` — and therefore out of
/// `ListWindows`, every subscriber fan-out, and `WindowOpened` — until the
/// owner completes the birth. The reservation still participates in
/// duplicate-name detection, and it holds the caller's reply so exactly one
/// `CreateWindowResult` is ever sent.
struct PendingWindowCreate {
    spec: WindowSpec,
    /// The window's canonical path, written from its validated name.
    path: ErasedActorPath,
    /// Taken by whichever path settles the reservation, so the caller sees
    /// exactly one `CreateWindowResult`. `Option` mirrors the desktop
    /// manager's `PendingCreate`, whose boot window has no caller to answer.
    held: Option<Held<CreateWindowResult>>,
}

/// The context a staged window child carries into its task completion
/// (ADR-0243 §9): the window's name, which keys its [`PendingWindowCreate`].
#[aether_data::kind(name = "aether.window.synthetic.spawn_key")]
struct WindowSpawnKey {
    name: String,
}

struct SyntheticWindow {
    info: WindowInfo,
    commands: ProtocolRef<WindowCommands>,
}

pub struct SyntheticWindowCapabilityState {
    windows: BTreeMap<ErasedActorPath, SyntheticWindow>,
    /// Staged creates keyed by window name, the key each birth carries back
    /// as its completion context.
    pending_creates: HashMap<String, PendingWindowCreate>,
    /// Each live child's window and retained monitor, keyed by the child's
    /// reference. The same reverse index identifies a command sender and a
    /// departing child's `MonitorNotice` (ADR-0230).
    child_monitors: HashMap<ErasedActorRef, (ErasedActorPath, MonitorHandle)>,
    pub(super) subscribers: WindowSubscribers,
}

impl SyntheticWindowCapabilityState {
    fn window_mut(&mut self, window: &ErasedActorPath) -> Result<&mut WindowInfo, String> {
        self.windows.get_mut(window).map(|window| &mut window.info).ok_or_else(|| format!("unknown window {window}"))
    }

    /// Validate one create request against the live windows and the reserved
    /// names no `ListWindows` reply can see yet, answering the window's path.
    fn check_create(&self, spec: &WindowSpec) -> Result<ErasedActorPath, String> {
        let path = crate::window_path(&crate::window_name(&spec.name)?);
        if self.windows.values().any(|window| window.info.name == spec.name)
            || self.pending_creates.contains_key(&spec.name)
        {
            return Err(format!("window name `{}` is already in use", spec.name));
        }
        Ok(path)
    }

    fn describe(spec: WindowSpec, path: ErasedActorPath) -> WindowInfo {
        let (width, height) = spec.size.map_or((DEFAULT_WIDTH, DEFAULT_HEIGHT), |size| (size.width, size.height));
        WindowInfo {
            path,
            name: spec.name,
            title: spec.title,
            mode: spec.mode,
            width,
            height,
            focused: false,
            occluded: width == 0 || height == 0,
        }
    }

    fn publish<K: Published, A>(&self, ctx: &mut NativeCtx<'_, A>, window: &ErasedActorPath, event: &K) {
        ctx.fanout(self.subscribers.recipients::<K>(window), event);
    }

    /// Promote an authoritatively applied child into the live window set, or
    /// retire it and report why it could not become live. Either way the
    /// reservation's reply is sent exactly once.
    fn publish_applied_window<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        child: ActorRef<SyntheticWindowInstance>,
        pending: PendingWindowCreate,
    ) {
        let PendingWindowCreate { spec, path, held } = pending;
        let monitor = match ctx.monitor(child.erase()) {
            Ok(monitor) => monitor,
            Err(error) => {
                ctx.send_to(child, &RetireWindow);
                answer(
                    ctx,
                    held,
                    &CreateWindowResult::Err { error: format!("failed to monitor window child: {error:?}") },
                );
                return;
            }
        };
        // The child's path needs no check against a prediction: the synthetic
        // identities read the shared window namespace consts, so the child's
        // name is the canonical path `window_path` wrote.
        let window = Self::describe(spec, path.clone());
        self.child_monitors.insert(child.erase(), (path.clone(), monitor));
        self.windows
            .insert(path.clone(), SyntheticWindow { info: window.clone(), commands: child.narrow::<WindowCommands>() });
        self.publish(ctx, &path, &WindowOpened { window: window.clone() });
        answer(ctx, held, &CreateWindowResult::Ok { window });
    }

    /// Apply one forwarded command to `window`, which the handler resolved
    /// from the forwarding child's stamped sender.
    fn apply_at_window<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        window: &ErasedActorPath,
        command: WindowCommand,
    ) -> ApplyWindowCommandResult {
        match command {
            WindowCommand::Close => {
                if self.windows.remove(window).is_none() {
                    return ApplyWindowCommandResult::Close(CloseWindowResult::Err {
                        error: format!("unknown window {window}"),
                    });
                }
                self.publish(ctx, window, &WindowClosed { window: window.clone() });
                ApplyWindowCommandResult::Close(CloseWindowResult::Ok)
            }
            WindowCommand::SetMode { mode, width, height } => {
                let info = match self.window_mut(window) {
                    Ok(info) => info,
                    Err(error) => {
                        return ApplyWindowCommandResult::SetMode(SetWindowModeResult::Err { error });
                    }
                };
                info.mode.clone_from(&mode);
                if matches!(mode, WindowMode::Windowed)
                    && let (Some(width), Some(height)) = (width, height)
                {
                    info.width = width;
                    info.height = height;
                    info.occluded = width == 0 || height == 0;
                }
                ApplyWindowCommandResult::SetMode(SetWindowModeResult::Ok {
                    mode,
                    width: info.width,
                    height: info.height,
                })
            }
            WindowCommand::SetTitle { title } => {
                let info = match self.window_mut(window) {
                    Ok(info) => info,
                    Err(error) => {
                        return ApplyWindowCommandResult::SetTitle(SetWindowTitleResult::Err { error });
                    }
                };
                info.title.clone_from(&title);
                ApplyWindowCommandResult::SetTitle(SetWindowTitleResult::Ok { title })
            }
            // A deterministic runtime has no bar to install and no pointer to
            // shape, so both accept for a live window and refuse for an
            // unknown one — the liveness half is the whole of what a scenario
            // can observe, and storing menus nothing reads back would be state
            // with no reader.
            WindowCommand::SetMenu { .. } if self.windows.contains_key(window) => {
                ApplyWindowCommandResult::SetMenu(SetWindowMenuResult::Ok)
            }
            WindowCommand::SetCursor { .. } if self.windows.contains_key(window) => {
                ApplyWindowCommandResult::SetCursor(SetWindowCursorResult::Ok)
            }
            command @ (WindowCommand::SetMenu { .. } | WindowCommand::SetCursor { .. }) => {
                command.refused(format!("unknown window {window}"))
            }
            WindowCommand::Focus => {
                if !self.windows.contains_key(window) {
                    return ApplyWindowCommandResult::Focus(FocusWindowResult::Err {
                        error: format!("unknown window {window}"),
                    });
                }
                for (path, entry) in &mut self.windows {
                    entry.info.focused = path == window;
                }
                ApplyWindowCommandResult::Focus(FocusWindowResult::Ok)
            }
            WindowCommand::RequestRedraw => match self.windows.get(window) {
                Some(_) => ApplyWindowCommandResult::RequestRedraw(RequestWindowRedrawResult::Ok),
                None => ApplyWindowCommandResult::RequestRedraw(RequestWindowRedrawResult::Err {
                    error: format!("unknown window {window}"),
                }),
            },
        }
    }
}

/// Answer a reservation's held reply, if it owes one.
fn answer<A>(ctx: &mut NativeCtx<'_, A>, held: Option<Held<CreateWindowResult>>, result: &CreateWindowResult) {
    if let Some(held) = held {
        held.answer(ctx, result);
    }
}

#[runtime(handler_set(WindowManagerSurface))]
impl NativeActor for SyntheticWindowCapability {
    type State = SyntheticWindowCapabilityState;
    type Config = ();

    const NAMESPACE: &'static str = crate::WINDOW_NAMESPACE;

    fn init(_config: (), _ctx: &mut NativeInitCtx<'_>) -> Result<SyntheticWindowCapabilityState, BootError> {
        Ok(SyntheticWindowCapabilityState {
            windows: BTreeMap::new(),
            pending_creates: HashMap::new(),
            child_monitors: HashMap::new(),
            subscribers: WindowSubscribers::new(),
        })
    }

    #[handler::single]
    fn on_list(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListWindows) -> ListWindowsResult {
        ListWindowsResult::Ok { windows: state.windows.values().map(|window| window.info.clone()).collect() }
    }

    #[handler::single]
    fn on_create(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: CreateWindow) -> Pending<CreateWindowResult> {
        let (pending, held) = ctx.hold::<CreateWindowResult>();
        let path = match state.check_create(&mail.spec) {
            Ok(path) => path,
            Err(error) => {
                held.answer(ctx, &CreateWindowResult::Err { error });
                return pending;
            }
        };
        // The birth carries the window's name as its completion context, since
        // that name is what the reservation is keyed by.
        if let Err((error, _)) = ctx
            .spawn_child::<SyntheticWindowInstance>(Subname::Named(&mail.spec.name), (), ())
            .stage_with(WindowSpawnKey { name: mail.spec.name.clone() })
        {
            held.answer(ctx, &CreateWindowResult::Err { error: format!("failed to spawn window child: {error:?}") });
            return pending;
        }
        let replaced = state
            .pending_creates
            .insert(mail.spec.name.clone(), PendingWindowCreate { spec: mail.spec, path, held: Some(held) });
        debug_assert!(replaced.is_none(), "a window name is reserved exactly once");
        pending
    }

    #[handler(task)]
    fn on_window_child_spawn_done(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        done: TaskDone<SpawnOutcome<SyntheticWindowInstance>>,
    ) {
        let Some(WindowSpawnKey { name }) = ctx.take_context() else {
            return;
        };
        let result = done.into_output().result;
        let Some(pending) = state.pending_creates.remove(&name) else {
            if let Ok(child) = &result {
                ctx.send_to(child, &RetireWindow);
            }
            return;
        };
        match result {
            Err(error) => answer(
                ctx,
                pending.held,
                &CreateWindowResult::Err { error: format!("failed to spawn window child: {error:?}") },
            ),
            Ok(child) => state.publish_applied_window(ctx, child, pending),
        }
    }

    #[handler::single]
    fn on_apply_command(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: ApplyWindowCommand,
    ) -> ApplyWindowCommandResult {
        let Some(window) =
            ctx.sender().and_then(|sender| state.child_monitors.get(&sender).map(|(path, _)| path.clone()))
        else {
            return mail.command.refused("window command from an actor that is not a live window child".to_owned());
        };
        state.apply_at_window(ctx, &window, mail.command)
    }

    /// Fan an injected event out as the published kind it names, through
    /// the typed set for that kind. A kind the window does not publish, or a
    /// payload that does not decode as the kind, warns and sends nothing.
    #[handler::single]
    fn on_inject(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: InjectWindowEvent) {
        if let Err(error) = state.subscribers.publish_encoded(ctx, &mail.window, mail.kind, &mail.payload) {
            tracing::warn!(target: "aether_window", window = %mail.window, %error, "injected window event not published");
        }
    }

    #[handler::single]
    fn on_monitor_notice(state: &mut Self::State, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        let Some(departed) = ctx.sender() else {
            return;
        };
        if let Some((path, _monitor)) = state.child_monitors.remove(&departed)
            && state.windows.remove(&path).is_some()
        {
            state.publish(ctx, &path, &WindowClosed { window: path.clone() });
        }
        state.subscribers.unsubscribe_all(departed);
    }
}

impl WindowManagerSurface for SyntheticWindowCapability {
    type State = SyntheticWindowCapabilityState;

    fn subscribers(state: &mut Self::State) -> &mut WindowSubscribers {
        &mut state.subscribers
    }

    /// Every window this runtime enumerates is applied and routable — a
    /// reservation is not a window here until its child's birth is
    /// authoritative, and it is absent from `windows` until then.
    fn routable_windows(state: &Self::State) -> Vec<RoutableWindow> {
        state
            .windows
            .iter()
            .map(|(path, window)| RoutableWindow { path: path.clone(), target: Some(window.commands) })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use aether_data::Kind;
    use aether_kinds::Key;
    use std::collections::BTreeSet;

    use super::*;
    use crate::runtime::subscribers::fixture::{Rig, receivers, recipients, watcher};
    // The subscription request kinds moved to the `WindowManagerSurface` set,
    // so the manager module no longer imports them for `use super::*` to carry.
    use crate::{SubscribeWindow, SubscribeWindowResult, UnsubscribeWindow, WindowSelector, WindowSubscription};

    fn test_state() -> SyntheticWindowCapabilityState {
        SyntheticWindowCapabilityState {
            windows: BTreeMap::new(),
            pending_creates: HashMap::new(),
            child_monitors: HashMap::new(),
            subscribers: WindowSubscribers::new(),
        }
    }

    fn window_path(name: &str) -> ErasedActorPath {
        crate::window_path(&aether_data::LoadName::new(name).expect("fixture window name"))
    }

    fn main_path() -> ErasedActorPath {
        window_path("main")
    }

    fn spec(name: &str, title: &str) -> WindowSpec {
        WindowSpec { name: name.to_owned(), title: title.to_owned(), mode: WindowMode::Windowed, size: None }
    }

    /// ADR-0169: an adopted set's kinds have to reach the adopter's
    /// *advertised* surface, not only its dispatch table. The `HandlesKind`
    /// half of that is compile-checked by every typed send, but a set whose
    /// rows never merge into `capabilities()` strips those kinds from
    /// `describe_handlers` and from the per-handler cost table with no
    /// behavioural symptom at all — the actor still answers the mail.
    ///
    /// Both adoption shapes are exercised: the manager merges an inherited
    /// block onto its own handlers, and the endpoint inherits its entire
    /// receive surface, which is the shape a naive "handlers are empty"
    /// check would report as receiving nothing.
    #[test]
    fn adopted_set_kinds_reach_the_advertised_receive_surface() {
        use aether_substrate::actor::native::Dispatch;

        use crate::runtime::instance::WindowInstanceState;

        let manager = <SyntheticWindowCapability as Dispatch<SyntheticWindowCapabilityState>>::capabilities();
        let advertised = manager.handlers.iter().map(|handler| handler.id).collect::<BTreeSet<_>>();
        assert!(advertised.contains(&SubscribeWindow::ID), "inherited kinds join the manager's advertised surface");
        assert!(advertised.contains(&UnsubscribeWindow::ID), "every inherited kind joins, not just the first");
        assert!(advertised.contains(&InjectWindowEvent::ID), "the manager's own handlers survive the merge");
        // Wider than `advertised` by the ADR-0093 `TaskCompletionWake`, which
        // is dispatched but not addressable (iamacoffeepot/aether#4266).
        let measured = <SyntheticWindowCapability as Dispatch<SyntheticWindowCapabilityState>>::measured_kinds()
            .into_iter()
            .collect::<BTreeSet<_>>();
        assert!(measured.is_superset(&advertised), "the cost table measures every kind the actor advertises");

        let endpoint = <SyntheticWindowInstance as Dispatch<WindowInstanceState>>::capabilities();
        let inherited = endpoint.handlers.iter().map(|handler| handler.id).collect::<BTreeSet<_>>();
        assert!(
            inherited.contains(&crate::CloseWindow::ID) && inherited.contains(&RetireWindow::ID),
            "an endpoint whose whole block lives in the set still advertises it",
        );
    }

    /// An explicit subscribe carries a subscriber path its decode proves
    /// live (ADR-0231 §3), so a path with no actor behind it never reaches
    /// the table: it is refused and adds no route. Fails if an unproven path
    /// can be held as a subscriber.
    #[test]
    fn explicit_subscriptions_validate_before_mutating_routes() {
        let mut rig = Rig::<SyntheticWindowCapability>::boot(());

        rig.send(&SubscribeWindow {
            selector: WindowSelector::All,
            subscription: WindowSubscription::Key(watcher("unknown").narrow()),
        });

        assert!(
            !rig.replies::<SubscribeWindowResult>().iter().any(|reply| matches!(reply, SubscribeWindowResult::Ok)),
            "an unproven subscriber is never accepted",
        );
        let held = rig.driver.read_state(|state| recipients::<Key>(&state.subscribers, &main_path()));
        assert_eq!(held, Some(BTreeSet::new()), "a refused subscribe adds no route");
    }

    /// A published event continues the causal chain that caused it and is
    /// stamped with the manager as its sender: the injected event's tracked
    /// root is the subscriber's envelope root, and its sender is the manager.
    /// Fails if the fan-out starts a fresh chain (settlement and tracing lose
    /// the event) or is sent under another identity.
    #[test]
    fn direct_publication_preserves_source_and_causal_lineage() {
        let mut rig = Rig::<SyntheticWindowCapability>::boot(());
        rig.watcher("direct");
        let subscription = WindowSubscription::Key(watcher("direct").narrow());
        assert!(matches!(rig.subscribe(WindowSelector::All, subscription), SubscribeWindowResult::Ok));

        let main = main_path();
        let event = Key { window: main.clone(), code: 41 };
        let inject = InjectWindowEvent { window: main, kind: Key::ID, payload: event.encode_into_bytes() };
        let (root, receipts) = rig.send_to(rig.manager(), &inject);

        assert_eq!(receivers(&receipts), ["direct"]);
        let receipt = &receipts[0];
        assert_eq!(receipt.root, Some(root), "the subscriber's envelope is in the injecting chain");
        assert_eq!(receipt.sender, Some(rig.manager().erase()), "the manager is the stamped sender");
        assert_eq!(receipt.event::<Key>(), Some(event));
    }

    /// Reducer-only: drives `check_create` directly rather than the handler,
    /// because staging a child needs a chassis-built binding this fixture has
    /// no way to supply.
    #[test]
    fn invalid_names_are_rejected_before_child_spawn() {
        let state = test_state();

        for name in ["", "two words", "bad:name"] {
            assert!(state.check_create(&spec(name, "Invalid")).is_err());
        }
        assert!(state.windows.is_empty());
        assert!(state.pending_creates.is_empty());
    }

    /// Reducer-only, for the same reason as above: it proves the reservation
    /// set participates in duplicate detection, not the staged spawn path.
    #[test]
    fn duplicate_reserved_names_are_rejected() {
        let mut state = test_state();
        assert!(state.check_create(&spec("palette", "Tools")).is_ok());

        // A reserved-but-not-yet-live name is invisible to `ListWindows` and
        // still blocks a second create for the same name.
        state.pending_creates.insert(
            "palette".to_owned(),
            PendingWindowCreate { spec: spec("palette", "Tools"), path: window_path("palette"), held: None },
        );
        assert!(state.check_create(&spec("palette", "Other tools")).is_err());
        assert!(!state.windows.values().any(|window| window.info.name == "palette"));
    }
}
