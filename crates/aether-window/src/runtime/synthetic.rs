//! Deterministic in-memory `aether.window` runtime for substrate harnesses.

// The scoped runtime layout keeps this established manager file while its
// concrete child runtime lives beneath the matching module directory.
#![allow(clippy::self_named_module_files)]

mod instance;

use std::collections::{BTreeMap, HashMap};

use aether_actor::{Manual, runtime};
use aether_data::MailboxId;
use aether_kinds::MonitorNotice;
use aether_substrate::{InboundMail, MonitorHandle, Subname};

use super::subscribers::{WindowSubscribers, validate_subscriber_mailbox};
use crate::{
    ApplyWindowCommand, ApplyWindowCommandResult, CloseWindowResult, CreateWindow, CreateWindowResult,
    FocusWindowResult, InjectWindowEvent, ListWindows, ListWindowsResult, RequestWindowRedrawResult, RetireWindow,
    SetWindowModeResult, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult, SubscribeWindowSelf,
    SyntheticWindowCapability, SyntheticWindowInstance, UnsubscribeAllWindows, UnsubscribeWindow,
    UnsubscribeWindowSelf, WindowCapability, WindowClosed, WindowCommand, WindowId, WindowInfo, WindowInstance,
    WindowMode, WindowOpened, WindowSpec,
};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx, SpawnOutcome, TaskDone};
pub use aether_substrate::chassis::error::BootError;

const DEFAULT_WIDTH: u32 = 800;
const DEFAULT_HEIGHT: u32 = 600;

/// A window whose child actor is staged but not yet authoritatively applied.
///
/// Its [`WindowId`] names a reservation, not a live actor, so the window stays
/// out of `windows` — and therefore out of `ListWindows`, every subscriber
/// fan-out, and `WindowOpened` — until the owner completes the birth. The
/// reservation still participates in duplicate-name detection, and it owns the
/// caller's reply so exactly one `CreateWindowResult` is ever sent.
struct PendingWindowCreate {
    window: WindowInfo,
    /// Taken by whichever path settles the reservation, so the caller sees
    /// exactly one `CreateWindowResult`. `Option` mirrors the desktop
    /// manager's `PendingCreate`, whose boot window has no caller to answer.
    reply: Option<Box<InboundMail>>,
}

pub struct SyntheticWindowCapabilityState {
    windows: BTreeMap<WindowId, WindowInfo>,
    pending_creates: HashMap<WindowId, PendingWindowCreate>,
    child_monitors: HashMap<WindowId, MonitorHandle>,
    subscribers: WindowSubscribers,
}

impl SyntheticWindowCapabilityState {
    fn window_mut(&mut self, window: WindowId) -> Result<&mut WindowInfo, String> {
        self.windows.get_mut(&window).ok_or_else(|| format!("unknown window {window:?}"))
    }

    /// Validate one create request against the live windows and the reserved
    /// names no `ListWindows` reply can see yet.
    fn check_create(&self, spec: &WindowSpec) -> Result<(), String> {
        crate::validate_window_name(&spec.name)?;
        if self.windows.values().any(|window| window.name == spec.name)
            || self.pending_creates.values().any(|pending| pending.window.name == spec.name)
        {
            return Err(format!("window name `{}` is already in use", spec.name));
        }
        Ok(())
    }

    fn describe(spec: WindowSpec, id: WindowId) -> WindowInfo {
        let (width, height) = spec.size.map_or((DEFAULT_WIDTH, DEFAULT_HEIGHT), |size| (size.width, size.height));
        WindowInfo {
            id,
            name: spec.name,
            title: spec.title,
            mode: spec.mode,
            width,
            height,
            focused: false,
            occluded: width == 0 || height == 0,
        }
    }

    fn publish<K: aether_data::Kind>(&self, ctx: &mut NativeCtx<'_>, window: WindowId, event: &K) {
        ctx.fanout(self.subscribers.recipients(window, K::ID), event);
    }

    /// Promote an authoritatively applied child into the live window set, or
    /// retire it and report why it could not become live. Either way the
    /// reservation's reply is sent exactly once.
    fn publish_applied_window(&mut self, ctx: &mut NativeCtx<'_>, child: MailboxId, pending: PendingWindowCreate) {
        let PendingWindowCreate { window, mut reply } = pending;
        let monitor = match ctx.monitor(child) {
            Ok(monitor) => monitor,
            Err(error) => {
                ctx.actor_at::<SyntheticWindowInstance>(child).send(&RetireWindow);
                answer(
                    &mut reply,
                    &CreateWindowResult::Err { error: format!("failed to monitor window child: {error:?}") },
                );
                return;
            }
        };
        let id = window.id;
        self.child_monitors.insert(id, monitor);
        self.windows.insert(id, window.clone());
        self.publish(ctx, id, &WindowOpened { window: window.clone() });
        answer(&mut reply, &CreateWindowResult::Ok { window });
    }
}

/// Discharge a reservation's deferred reply, if it owes one.
fn answer(reply: &mut Option<Box<InboundMail>>, result: &CreateWindowResult) {
    if let Some(reply) = reply.take() {
        reply.reply(result);
    }
}

#[runtime]
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

    /// Settle every reservation the manager still owes a reply for. The staged
    /// child may not have been applied yet, so the retirement rides the ordered
    /// tail its reserved route already parks.
    fn unwire(state: &mut Self::State, ctx: &mut NativeCtx<'_>) {
        for (id, mut pending) in state.pending_creates.drain() {
            ctx.actor_at::<SyntheticWindowInstance>(MailboxId(id.0)).send(&RetireWindow);
            answer(&mut pending.reply, &CreateWindowResult::Err { error: "window manager shutting down".to_owned() });
        }
    }

    #[handler::single]
    fn on_list(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListWindows) -> ListWindowsResult {
        ListWindowsResult::Ok { windows: state.windows.values().cloned().collect() }
    }

    #[handler::manual]
    fn on_create(state: &mut Self::State, ctx: &mut NativeCtx<'_, Manual>, mail: CreateWindow) {
        let reply = ctx.take_inbound();
        if let Err(error) = state.check_create(&mail.spec) {
            reply.reply(&CreateWindowResult::Err { error });
            return;
        }
        let predicted = ctx.actor::<WindowCapability>().resolve::<WindowInstance>(&mail.spec.name).mailbox_id();
        let receipt = match ctx
            .spawn_child::<WindowCapability, SyntheticWindowInstance>(Subname::Named(&mail.spec.name), (), ())
            .stage()
        {
            Ok(receipt) => receipt,
            Err(error) => {
                reply.reply(&CreateWindowResult::Err { error: format!("failed to spawn window child: {error:?}") });
                return;
            }
        };
        // The reservation is keyed by the id consumers address, so a divergent
        // deterministic id dooms it rather than publishing a window nobody can
        // reach: answer now and reserve nothing, and the completion's
        // no-reservation arm retires the child the owner still applies.
        if receipt.mailbox_id != predicted {
            reply.reply(&CreateWindowResult::Err {
                error: format!(
                    "spawned window child {:?} did not match predicted mailbox {predicted:?}",
                    receipt.mailbox_id
                ),
            });
            return;
        }

        let id = WindowId(predicted.0);
        let window = SyntheticWindowCapabilityState::describe(mail.spec, id);
        let replaced = state.pending_creates.insert(id, PendingWindowCreate { window, reply: Some(Box::new(reply)) });
        debug_assert!(replaced.is_none(), "a window name is reserved exactly once");
    }

    #[handler(task)]
    fn on_window_child_spawn_done(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done: TaskDone<SpawnOutcome, ()>) {
        // The birth names itself on both arms, so the reservation key comes
        // straight off the outcome rather than a context struct carrying it.
        let child = done.output().mailbox_id;
        let Some(mut pending) = state.pending_creates.remove(&WindowId(child.0)) else {
            if done.output().result.is_ok() {
                ctx.actor_at::<SyntheticWindowInstance>(child).send(&RetireWindow);
            }
            done.release_no_reply();
            return;
        };
        match &done.output().result {
            Err(error) => answer(
                &mut pending.reply,
                &CreateWindowResult::Err { error: format!("failed to spawn window child: {error:?}") },
            ),
            Ok(()) => state.publish_applied_window(ctx, child, pending),
        }
        done.release_no_reply();
    }

    #[handler::single]
    fn on_apply_command(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: ApplyWindowCommand,
    ) -> ApplyWindowCommandResult {
        match mail.command {
            WindowCommand::Close => {
                if state.windows.remove(&mail.window).is_none() {
                    return ApplyWindowCommandResult::Close(CloseWindowResult::Err {
                        error: format!("unknown window {:?}", mail.window),
                    });
                }
                state.publish(ctx, mail.window, &WindowClosed { window: mail.window });
                ApplyWindowCommandResult::Close(CloseWindowResult::Ok)
            }
            WindowCommand::SetMode { mode, width, height } => {
                let window = match state.window_mut(mail.window) {
                    Ok(window) => window,
                    Err(error) => {
                        return ApplyWindowCommandResult::SetMode(SetWindowModeResult::Err { error });
                    }
                };
                window.mode.clone_from(&mode);
                if matches!(mode, WindowMode::Windowed)
                    && let (Some(width), Some(height)) = (width, height)
                {
                    window.width = width;
                    window.height = height;
                    window.occluded = width == 0 || height == 0;
                }
                ApplyWindowCommandResult::SetMode(SetWindowModeResult::Ok {
                    mode,
                    width: window.width,
                    height: window.height,
                })
            }
            WindowCommand::SetTitle { title } => {
                let window = match state.window_mut(mail.window) {
                    Ok(window) => window,
                    Err(error) => {
                        return ApplyWindowCommandResult::SetTitle(SetWindowTitleResult::Err { error });
                    }
                };
                window.title.clone_from(&title);
                ApplyWindowCommandResult::SetTitle(SetWindowTitleResult::Ok { title })
            }
            WindowCommand::Focus => {
                if !state.windows.contains_key(&mail.window) {
                    return ApplyWindowCommandResult::Focus(FocusWindowResult::Err {
                        error: format!("unknown window {:?}", mail.window),
                    });
                }
                for (id, window) in &mut state.windows {
                    window.focused = *id == mail.window;
                }
                ApplyWindowCommandResult::Focus(FocusWindowResult::Ok)
            }
            WindowCommand::RequestRedraw => match state.windows.get(&mail.window) {
                Some(_) => ApplyWindowCommandResult::RequestRedraw(RequestWindowRedrawResult::Ok),
                None => ApplyWindowCommandResult::RequestRedraw(RequestWindowRedrawResult::Err {
                    error: format!("unknown window {:?}", mail.window),
                }),
            },
        }
    }

    #[handler::single]
    fn on_subscribe(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: SubscribeWindow) -> SubscribeWindowResult {
        if let Err(error) = validate_subscriber_mailbox(ctx, mail.mailbox) {
            return SubscribeWindowResult::Err { error };
        }
        state.subscribers.subscribe(ctx, mail.selector, mail.kind, mail.mailbox);
        SubscribeWindowResult::Ok
    }

    #[handler::single]
    fn on_subscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: SubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        match state.subscribers.subscribe_self(ctx, mail.selector, mail.kind) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::Err { error },
        }
    }

    #[handler::single]
    fn on_unsubscribe(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: UnsubscribeWindow,
    ) -> SubscribeWindowResult {
        if let Err(error) = validate_subscriber_mailbox(ctx, mail.mailbox) {
            return SubscribeWindowResult::Err { error };
        }
        state.subscribers.unsubscribe(mail.selector, mail.kind, mail.mailbox);
        SubscribeWindowResult::Ok
    }

    #[handler::single]
    fn on_unsubscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        mail: UnsubscribeWindowSelf,
    ) -> SubscribeWindowResult {
        match state.subscribers.unsubscribe_self(ctx, mail.selector, mail.kind) {
            Ok(()) => SubscribeWindowResult::Ok,
            Err(error) => SubscribeWindowResult::Err { error },
        }
    }

    #[handler::single]
    fn on_unsubscribe_all(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: UnsubscribeAllWindows) {
        state.subscribers.unsubscribe_all(mail.mailbox);
    }

    #[handler::single]
    fn on_inject(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: InjectWindowEvent) {
        for recipient in state.subscribers.recipients(mail.window, mail.kind) {
            let _ = ctx.send_envelope_tracked(recipient, mail.kind, &mail.payload);
        }
    }

    #[handler::single]
    fn on_monitor_notice(state: &mut Self::State, ctx: &mut NativeCtx<'_>, notice: MonitorNotice) {
        let id = WindowId(notice.target.0);
        if state.child_monitors.remove(&id).is_some() && state.windows.remove(&id).is_some() {
            state.publish(ctx, id, &WindowClosed { window: id });
        }
        state.subscribers.purge_departed(notice.target);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use aether_actor::Addressable;
    use aether_data::Kind;
    use aether_harness_substrate::{HarnessOp, SubstrateHarness};
    use aether_kinds::Key;
    use aether_substrate::Registry;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::MailDispatch;
    use aether_substrate::mail::{MailId, Source};
    use aether_substrate::testing::boot_authority;

    use super::*;

    fn test_state() -> SyntheticWindowCapabilityState {
        SyntheticWindowCapabilityState {
            windows: BTreeMap::new(),
            pending_creates: HashMap::new(),
            child_monitors: HashMap::new(),
            subscribers: WindowSubscribers::new(),
        }
    }

    fn test_ctx() -> (Arc<NativeBinding>, Arc<Mailer>) {
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), MailboxId(1)));
        (binding, mailer)
    }

    fn spec(name: &str, title: &str) -> WindowSpec {
        WindowSpec { name: name.to_owned(), title: title.to_owned(), mode: WindowMode::Windowed, size: None }
    }

    #[test]
    fn explicit_subscriptions_validate_before_mutating_routes() {
        let (binding, mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let mut state = test_state();
        let unknown = MailboxId(0xBAD);

        assert!(matches!(
            SyntheticWindowCapability::on_subscribe(
                &mut state,
                &mut ctx,
                SubscribeWindow { selector: crate::WindowSelector::All, kind: Key::ID, mailbox: unknown },
            ),
            SubscribeWindowResult::Err { error } if error == "unknown mailbox id 0x0000000000000bad"
        ));
        assert!(state.subscribers.recipients(WindowId(1), Key::ID).is_empty());

        let dropped = mailer.registry().register_inline(
            &boot_authority(),
            "test.synthetic.dropped",
            Arc::new(|_dispatch: MailDispatch<'_>| {}),
        );
        state.subscribers.subscribe(&mut ctx, crate::WindowSelector::All, Key::ID, dropped);
        mailer.registry().drop_mailbox(&boot_authority(), dropped).expect("drop subscriber mailbox");

        assert!(matches!(
            SyntheticWindowCapability::on_unsubscribe(
                &mut state,
                &mut ctx,
                UnsubscribeWindow { selector: crate::WindowSelector::All, kind: Key::ID, mailbox: dropped },
            ),
            SubscribeWindowResult::Err { error } if error == format!("mailbox {dropped:?} already dropped")
        ));
        assert_eq!(state.subscribers.recipients(WindowId(1), Key::ID), BTreeSet::from([dropped]));
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
    fn duplicate_live_and_reserved_names_are_rejected_and_distinct_names_predict_distinct_children() {
        let mut state = test_state();
        state.windows.insert(WindowId(7), SyntheticWindowCapabilityState::describe(spec("main", "Game"), WindowId(7)));

        assert!(state.check_create(&spec("main", "Other title")).is_err());
        assert!(state.check_create(&spec("palette", "Tools")).is_ok());

        // A reserved-but-not-yet-live name is invisible to `ListWindows` and
        // still blocks a second create for the same name.
        state.pending_creates.insert(
            WindowId(9),
            PendingWindowCreate {
                window: SyntheticWindowCapabilityState::describe(spec("palette", "Tools"), WindowId(9)),
                reply: None,
            },
        );
        assert!(state.check_create(&spec("palette", "Other tools")).is_err());
        assert!(!state.windows.contains_key(&WindowId(9)));

        assert_ne!(
            WindowInstance::resolve(WindowCapability::resolve(0, ()).0, "main"),
            WindowInstance::resolve(WindowCapability::resolve(0, ()).0, "palette"),
        );
    }

    #[test]
    fn name_is_stable_after_title_mutation() {
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let mut state = test_state();
        let id = WindowId(7);
        state.windows.insert(
            id,
            WindowInfo {
                id,
                name: "main".to_owned(),
                title: "Game".to_owned(),
                mode: WindowMode::Windowed,
                width: DEFAULT_WIDTH,
                height: DEFAULT_HEIGHT,
                focused: false,
                occluded: false,
            },
        );

        assert!(matches!(
            SyntheticWindowCapability::on_apply_command(
                &mut state,
                &mut ctx,
                ApplyWindowCommand { window: id, command: WindowCommand::SetTitle { title: "Renamed".to_owned() } },
            ),
            ApplyWindowCommandResult::SetTitle(SetWindowTitleResult::Ok { .. })
        ));

        assert_eq!(state.windows[&id].name, "main");
        assert_eq!(state.windows[&id].title, "Renamed");
    }

    /// Scheduler-backed. `CreateWindow` now stages its child and answers from a
    /// later turn, so this pins the ordering the staged path has to preserve:
    /// by the time the caller sees `Ok`, the window is already enumerable and
    /// its child already owns the name a second create is refused for. A
    /// promotion that replied before publishing would list an empty set here.
    #[test]
    fn create_replies_only_after_the_staged_child_is_live() {
        let mut harness = SubstrateHarness::start().expect("boot synthetic harness");
        let report = harness
            .execute(vec![
                (
                    "created",
                    HarnessOp::send_and_await(
                        WindowCapability::NAMESPACE,
                        &CreateWindow { spec: spec("main", "Main") },
                    ),
                ),
                ("listed", HarnessOp::send_and_await(WindowCapability::NAMESPACE, &ListWindows)),
                (
                    "duplicate",
                    HarnessOp::send_and_await(
                        WindowCapability::NAMESPACE,
                        &CreateWindow { spec: spec("main", "Second") },
                    ),
                ),
            ])
            .expect("staged window creation settles");

        let Ok(CreateWindowResult::Ok { window }) = report.reply::<CreateWindowResult>("created") else {
            panic!("staged create succeeds");
        };
        assert_eq!(window.id, WindowId(WindowInstance::resolve(WindowCapability::resolve(0, ()).0, "main").0));

        let Ok(ListWindowsResult::Ok { windows }) = report.reply::<ListWindowsResult>("listed") else {
            panic!("synthetic list succeeds");
        };
        assert_eq!(windows.iter().map(|window| window.id).collect::<Vec<_>>(), [window.id]);

        assert!(matches!(report.reply::<CreateWindowResult>("duplicate"), Ok(CreateWindowResult::Err { .. })));
    }

    #[test]
    fn unexpected_child_departure_closes_only_its_window() {
        let mut harness = SubstrateHarness::start().expect("boot synthetic harness");
        harness
            .execute(vec![
                (
                    "first",
                    HarnessOp::send_and_await(
                        WindowCapability::NAMESPACE,
                        &CreateWindow { spec: spec("first", "First") },
                    ),
                ),
                (
                    "second",
                    HarnessOp::send_and_await(
                        WindowCapability::NAMESPACE,
                        &CreateWindow { spec: spec("second", "Second") },
                    ),
                ),
                (
                    "depart-first",
                    HarnessOp::send_mail(
                        format!("{}/{}:first", WindowCapability::NAMESPACE, WindowInstance::NAMESPACE),
                        &RetireWindow,
                    ),
                ),
                ("remaining", HarnessOp::send_and_await(WindowCapability::NAMESPACE, &ListWindows)),
            ])
            .expect("unexpected child departure settles");

        let second = WindowId(WindowInstance::resolve(WindowCapability::resolve(0, ()).0, "second").0);

        // The departure's own chain settles with `RetireWindow`, but the
        // `MonitorNotice` that prunes the capability's list (ADR-0079 §8)
        // reaches the parent on a chain the caller never joins — so there is
        // nothing here to await, only a state to observe becoming true
        // (iamacoffeepot/aether#4184). A fixed round-trip count stood in for
        // that wait and held only while the box was fast enough; poll to a
        // deadline instead, so the test measures the outcome rather than the
        // runner.
        let deadline = Instant::now() + Duration::from_secs(5);
        let listed = loop {
            let ListWindowsResult::Ok { windows } = harness
                .execute(vec![("listed", HarnessOp::send_and_await(WindowCapability::NAMESPACE, &ListWindows))])
                .expect("process child monitor notice")
                .reply::<ListWindowsResult>("listed")
                .expect("surviving sibling list reply")
            else {
                panic!("synthetic list succeeds");
            };
            let ids = windows.iter().map(|window| window.id).collect::<Vec<_>>();
            if ids == [second] {
                break ids;
            }
            assert!(
                Instant::now() < deadline,
                "the retired child's window is still listed after 5s: {ids:?}, expected only {second:?}"
            );
        };
        assert_eq!(listed, [second]);
    }
}
