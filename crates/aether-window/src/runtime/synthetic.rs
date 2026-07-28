//! Deterministic in-memory `aether.window` runtime for substrate harnesses.

// The scoped runtime layout keeps this established manager file while its
// concrete child runtime lives beneath the matching module directory.
#![allow(clippy::self_named_module_files)]

mod instance;

use std::collections::{BTreeMap, HashMap};

use aether_actor::runtime;
use aether_kinds::MonitorNotice;
use aether_substrate::{MonitorHandle, Subname};

use super::subscribers::{WindowSubscribers, validate_subscriber_mailbox};
use crate::{
    ApplyWindowCommand, ApplyWindowCommandResult, CloseWindowResult, CreateWindow, CreateWindowResult,
    FocusWindowResult, InjectWindowEvent, ListWindows, ListWindowsResult, RequestWindowRedrawResult, RetireWindow,
    SetWindowModeResult, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult, SubscribeWindowSelf,
    SyntheticWindowCapability, SyntheticWindowInstance, UnsubscribeAllWindows, UnsubscribeWindow,
    UnsubscribeWindowSelf, WindowCapability, WindowClosed, WindowCommand, WindowId, WindowInfo, WindowInstance,
    WindowMode, WindowOpened,
};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

const DEFAULT_WIDTH: u32 = 800;
const DEFAULT_HEIGHT: u32 = 600;

pub struct SyntheticWindowCapabilityState {
    windows: BTreeMap<WindowId, WindowInfo>,
    child_monitors: HashMap<WindowId, MonitorHandle>,
    subscribers: WindowSubscribers,
}

impl SyntheticWindowCapabilityState {
    fn window_mut(&mut self, window: WindowId) -> Result<&mut WindowInfo, String> {
        self.windows.get_mut(&window).ok_or_else(|| format!("unknown window {window:?}"))
    }

    fn publish<K: aether_data::Kind>(&self, ctx: &mut NativeCtx<'_>, window: WindowId, event: &K) {
        ctx.fanout(self.subscribers.recipients(window, K::ID), event);
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
            child_monitors: HashMap::new(),
            subscribers: WindowSubscribers::new(),
        })
    }

    #[handler::single]
    fn on_list(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListWindows) -> ListWindowsResult {
        ListWindowsResult::Ok { windows: state.windows.values().cloned().collect() }
    }

    #[handler::single]
    fn on_create(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: CreateWindow) -> CreateWindowResult {
        if let Err(error) = crate::validate_window_name(&mail.spec.name) {
            return CreateWindowResult::Err { error };
        }
        if state.windows.values().any(|window| window.name == mail.spec.name) {
            return CreateWindowResult::Err { error: format!("window name `{}` is already in use", mail.spec.name) };
        }
        let predicted = ctx.actor::<WindowCapability>().resolve::<WindowInstance>(&mail.spec.name).mailbox_id();
        let id = WindowId(predicted.0);
        let spawned = match ctx
            .spawn_child::<WindowCapability, SyntheticWindowInstance>(Subname::Named(&mail.spec.name), (), ())
            .finish()
        {
            Ok(spawned) => spawned,
            Err(error) => return CreateWindowResult::Err { error: format!("failed to spawn window child: {error:?}") },
        };
        if spawned != predicted {
            ctx.actor_at::<SyntheticWindowInstance>(spawned).send(&RetireWindow);
            return CreateWindowResult::Err {
                error: format!("spawned window child {spawned:?} did not match predicted mailbox {predicted:?}"),
            };
        }
        let monitor = match ctx.monitor(spawned) {
            Ok(monitor) => monitor,
            Err(error) => {
                ctx.actor_at::<SyntheticWindowInstance>(spawned).send(&RetireWindow);
                return CreateWindowResult::Err { error: format!("failed to monitor window child: {error:?}") };
            }
        };
        let (width, height) = mail.spec.size.map_or((DEFAULT_WIDTH, DEFAULT_HEIGHT), |size| (size.width, size.height));
        let window = WindowInfo {
            id,
            name: mail.spec.name,
            title: mail.spec.title,
            mode: mail.spec.mode,
            width,
            height,
            focused: false,
            occluded: width == 0 || height == 0,
        };
        state.child_monitors.insert(id, monitor);
        state.windows.insert(id, window.clone());
        state.publish(ctx, id, &WindowOpened { window: window.clone() });
        CreateWindowResult::Ok { window }
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

    use aether_actor::Addressable;
    use aether_data::Kind;
    use aether_harness_substrate::{HarnessOp, SubstrateHarness};
    use aether_kinds::Key;
    use aether_substrate::Registry;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::MailDispatch;
    use aether_substrate::mail::{MailId, Source};

    use super::*;

    fn test_state() -> SyntheticWindowCapabilityState {
        SyntheticWindowCapabilityState {
            windows: BTreeMap::new(),
            child_monitors: HashMap::new(),
            subscribers: WindowSubscribers::new(),
        }
    }

    fn test_ctx() -> (Arc<NativeBinding>, Arc<Mailer>) {
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), aether_data::MailboxId(1)));
        (binding, mailer)
    }

    fn spec(name: &str, title: &str) -> crate::WindowSpec {
        crate::WindowSpec { name: name.to_owned(), title: title.to_owned(), mode: WindowMode::Windowed, size: None }
    }

    #[test]
    fn explicit_subscriptions_validate_before_mutating_routes() {
        let (binding, mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let mut state = test_state();
        let unknown = aether_data::MailboxId(0xBAD);

        assert!(matches!(
            SyntheticWindowCapability::on_subscribe(
                &mut state,
                &mut ctx,
                SubscribeWindow { selector: crate::WindowSelector::All, kind: Key::ID, mailbox: unknown },
            ),
            SubscribeWindowResult::Err { error } if error == "unknown mailbox id 0x0000000000000bad"
        ));
        assert!(state.subscribers.recipients(WindowId(1), Key::ID).is_empty());

        let dropped =
            mailer.registry().register_inline("test.synthetic.dropped", Arc::new(|_dispatch: MailDispatch<'_>| {}));
        state.subscribers.subscribe(&mut ctx, crate::WindowSelector::All, Key::ID, dropped);
        mailer.registry().drop_mailbox(dropped).expect("drop subscriber mailbox");

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

    #[test]
    fn invalid_names_are_rejected_before_child_spawn() {
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);

        for name in ["", "two words", "bad:name"] {
            let mut state = test_state();
            assert!(matches!(
                SyntheticWindowCapability::on_create(
                    &mut state,
                    &mut ctx,
                    CreateWindow { spec: spec(name, "Invalid") },
                ),
                CreateWindowResult::Err { .. }
            ));
            assert!(state.windows.is_empty());
        }
    }

    #[test]
    fn duplicate_live_names_are_rejected_and_distinct_names_predict_distinct_children() {
        let (binding, _mailer) = test_ctx();
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let mut state = test_state();
        state.windows.insert(
            WindowId(7),
            WindowInfo {
                id: WindowId(7),
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
            SyntheticWindowCapability::on_create(
                &mut state,
                &mut ctx,
                CreateWindow { spec: spec("main", "Other title") },
            ),
            CreateWindowResult::Err { .. }
        ));
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
        let ListWindowsResult::Ok { windows } = harness
            .execute(vec![("listed", HarnessOp::send_and_await(WindowCapability::NAMESPACE, &ListWindows))])
            .expect("process child monitor notice")
            .reply::<ListWindowsResult>("listed")
            .expect("surviving sibling list reply")
        else {
            panic!("synthetic list succeeds");
        };
        assert_eq!(windows.iter().map(|window| window.id).collect::<Vec<_>>(), [second]);
    }
}
