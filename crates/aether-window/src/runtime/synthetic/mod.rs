//! Deterministic in-memory `aether.window` backend for substrate harnesses.

use std::collections::{BTreeMap, HashMap};

use aether_actor::{ActorRef, ErasedActorRef, ProtocolRef};
use aether_data::ErasedActorPath;
use aether_substrate::actor::native::{Held, NativeCtx, SpawnOutcome};
use aether_substrate::{MonitorHandle, Subname};

use super::WindowSpawnKey;
use super::manager::{RoutableWindow, WindowCommands};
use super::subscribers::{Published, WindowSubscribers};
use crate::{
    ApplyWindowCommandResult, CloseWindowResult, CreateWindowResult, FocusWindowResult, RequestWindowRedrawResult,
    RetireWindow, SetWindowCursorResult, SetWindowMenuResult, SetWindowModeResult, SetWindowTitleResult,
    WindowCapability, WindowClosed, WindowCommand, WindowInfo, WindowInstance, WindowMode, WindowOpened, WindowSpec,
};

const DEFAULT_WIDTH: u32 = 800;
const DEFAULT_HEIGHT: u32 = 600;

/// A window whose child actor is staged but not yet authoritatively applied.
///
/// It is keyed by the window's path, which names a reservation, not a live
/// actor, so the window stays out of `windows` — and therefore out of
/// `ListWindows`, every subscriber fan-out, and `WindowOpened` — until the
/// owner completes the birth. The reservation still participates in
/// duplicate-name detection, and it holds the caller's reply so exactly one
/// `CreateWindowResult` is ever sent.
struct PendingWindowCreate {
    spec: WindowSpec,
    /// Taken by whichever path settles the reservation, so the caller sees
    /// exactly one `CreateWindowResult`. `Option` mirrors the desktop
    /// manager's `PendingCreate`, whose boot window has no caller to answer.
    held: Option<Held<CreateWindowResult>>,
}

struct SyntheticWindow {
    info: WindowInfo,
    commands: ProtocolRef<WindowCommands>,
}

/// The synthetic backend's state: every window, staged create, and child it
/// supervises, and the subscription table its events fan out through.
pub struct SyntheticWindows {
    windows: BTreeMap<ErasedActorPath, SyntheticWindow>,
    /// Staged creates keyed by window path, the key each birth carries back
    /// as its completion context.
    pending_creates: HashMap<ErasedActorPath, PendingWindowCreate>,
    /// Each live child's window and retained monitor, keyed by the child's
    /// reference. The same reverse index identifies a command sender and a
    /// departing child's `MonitorNotice` (ADR-0230).
    child_monitors: HashMap<ErasedActorRef, (ErasedActorPath, MonitorHandle)>,
    pub(super) subscribers: WindowSubscribers,
}

impl SyntheticWindows {
    pub(super) fn new() -> Self {
        Self {
            windows: BTreeMap::new(),
            pending_creates: HashMap::new(),
            child_monitors: HashMap::new(),
            subscribers: WindowSubscribers::new(),
        }
    }

    fn window_mut(&mut self, window: &ErasedActorPath) -> Result<&mut WindowInfo, String> {
        self.windows.get_mut(window).map(|window| &mut window.info).ok_or_else(|| format!("unknown window {window}"))
    }

    /// Validate one create request against the live windows and the reserved
    /// names no `ListWindows` reply can see yet, answering the window's path.
    fn check_create(&self, spec: &WindowSpec) -> Result<ErasedActorPath, String> {
        let path = crate::window_path(&crate::window_name(&spec.name)?);
        if self.windows.values().any(|window| window.info.name == spec.name) || self.pending_creates.contains_key(&path)
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

    /// Promote an authoritatively applied child into the live window set and
    /// send the reservation's reply. A child that closed before this ran is
    /// published all the same, and its notice, which arrives after this
    /// handler returns, closes the window.
    fn publish_applied_window<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        path: ErasedActorPath,
        child: ActorRef<WindowInstance>,
        pending: PendingWindowCreate,
    ) {
        let PendingWindowCreate { spec, held } = pending;
        // The child's path needs no check against a prediction: the window
        // identities read the shared window namespace consts, so the child's
        // name is the canonical path `window_path` wrote.
        let window = Self::describe(spec, path.clone());
        self.child_monitors.insert(child.erase(), (path.clone(), ctx.monitor(child.erase())));
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

impl SyntheticWindows {
    /// Every applied window, in path order.
    pub(super) fn list(&self) -> Vec<WindowInfo> {
        self.windows.values().map(|window| window.info.clone()).collect()
    }

    /// Reserve a create and stage its window child, keyed by the window's
    /// path; `held` is answered once the birth is authoritative.
    pub(super) fn create(
        &mut self,
        ctx: &mut NativeCtx<'_, WindowCapability>,
        spec: WindowSpec,
        held: Held<CreateWindowResult>,
    ) {
        let path = match self.check_create(&spec) {
            Ok(path) => path,
            Err(error) => return held.answer(ctx, &CreateWindowResult::Err { error }),
        };
        if let Err((error, _)) = ctx
            .spawn_child::<WindowInstance>(Subname::Named(&spec.name), (), ())
            .stage_with(WindowSpawnKey { path: path.clone() })
        {
            return held
                .answer(ctx, &CreateWindowResult::Err { error: format!("failed to spawn window child: {error:?}") });
        }
        let replaced = self.pending_creates.insert(path, PendingWindowCreate { spec, held: Some(held) });
        debug_assert!(replaced.is_none(), "a window path is reserved exactly once");
    }

    /// Apply the authoritative result of the staged child for `path`. A child
    /// whose reservation is gone is retired.
    pub(super) fn finish_window_child_spawn<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        path: &ErasedActorPath,
        outcome: SpawnOutcome<WindowInstance>,
    ) {
        let Some(pending) = self.pending_creates.remove(path) else {
            if let Ok(child) = &outcome.result {
                ctx.send_to(child, &RetireWindow);
            }
            return;
        };
        match outcome.result {
            Err(error) => answer(
                ctx,
                pending.held,
                &CreateWindowResult::Err { error: format!("failed to spawn window child: {error:?}") },
            ),
            Ok(child) => self.publish_applied_window(ctx, path.clone(), child, pending),
        }
    }

    /// Apply a command the stamped sender forwarded, at the window that
    /// sender is, answering at once.
    pub(super) fn apply_command<A>(
        &mut self,
        ctx: &mut NativeCtx<'_, A>,
        command: WindowCommand,
        held: Held<ApplyWindowCommandResult>,
    ) {
        let window = ctx.sender().and_then(|sender| self.child_monitors.get(&sender).map(|(path, _)| path.clone()));
        let result = match window {
            Some(window) => self.apply_at_window(ctx, &window, command),
            None => command.refused("window command from an actor that is not a live window child".to_owned()),
        };
        held.answer(ctx, &result);
    }

    /// A monitored actor departed: when it is a window child, its window
    /// closes.
    pub(super) fn child_departed<A>(&mut self, ctx: &mut NativeCtx<'_, A>, departed: ErasedActorRef) {
        if let Some((path, _monitor)) = self.child_monitors.remove(&departed)
            && self.windows.remove(&path).is_some()
        {
            self.publish(ctx, &path, &WindowClosed { window: path.clone() });
        }
    }

    /// Every window this backend enumerates is applied and routable — a
    /// reservation is not a window here until its child's birth is
    /// authoritative, and it is absent from `windows` until then.
    pub(super) fn routable_windows(&self) -> Vec<RoutableWindow> {
        self.windows
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
    use crate::{InjectWindowEvent, SubscribeWindow, SubscribeWindowResult, WindowSelector, WindowSubscription};

    fn test_state() -> SyntheticWindows {
        SyntheticWindows::new()
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

    /// An explicit subscribe carries a subscriber path its decode proves
    /// live (ADR-0231 §3), so a path with no actor behind it never reaches
    /// the table: it is refused and adds no route. Fails if an unproven path
    /// can be held as a subscriber.
    #[test]
    fn explicit_subscriptions_validate_before_mutating_routes() {
        let mut rig = Rig::synthetic();

        rig.send(&SubscribeWindow {
            selector: WindowSelector::All,
            subscription: WindowSubscription::Key(watcher("unknown").narrow()),
        });

        assert!(
            !rig.replies::<SubscribeWindowResult>().iter().any(|reply| matches!(reply, SubscribeWindowResult::Ok)),
            "an unproven subscriber is never accepted",
        );
        let held = rig.driver.read_state(|state| recipients::<Key>(state.subscribers(), &main_path()));
        assert_eq!(held, Some(BTreeSet::new()), "a refused subscribe adds no route");
    }

    /// A published event continues the causal chain that caused it and is
    /// stamped with the manager as its sender: the injected event's tracked
    /// root is the subscriber's envelope root, and its sender is the manager.
    /// Fails if the fan-out starts a fresh chain (settlement and tracing lose
    /// the event) or is sent under another identity.
    #[test]
    fn direct_publication_preserves_source_and_causal_lineage() {
        let mut rig = Rig::synthetic();
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
        state
            .pending_creates
            .insert(window_path("palette"), PendingWindowCreate { spec: spec("palette", "Tools"), held: None });
        assert!(state.check_create(&spec("palette", "Other tools")).is_err());
        assert!(!state.windows.values().any(|window| window.info.name == "palette"));
    }
}
