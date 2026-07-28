//! Deterministic in-memory `aether.window` runtime for substrate harnesses.

use std::collections::BTreeMap;

use aether_actor::runtime;
use aether_kinds::MonitorNotice;

use super::subscribers::{WindowSubscribers, validate_subscriber_mailbox};
use crate::{
    CloseWindow, CloseWindowResult, CreateWindow, CreateWindowResult, FocusWindow, FocusWindowResult,
    InjectWindowEvent, ListWindows, ListWindowsResult, RequestWindowRedraw, RequestWindowRedrawResult, SetWindowMode,
    SetWindowModeResult, SetWindowTitle, SetWindowTitleResult, SubscribeWindow, SubscribeWindowResult,
    SubscribeWindowSelf, SyntheticWindowCapability, UnsubscribeAllWindows, UnsubscribeWindow, UnsubscribeWindowSelf,
    WindowClosed, WindowId, WindowInfo, WindowMode, WindowOpened,
};

pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;

const DEFAULT_WIDTH: u32 = 800;
const DEFAULT_HEIGHT: u32 = 600;

pub struct SyntheticWindowCapabilityState {
    next_window_id: u64,
    windows: BTreeMap<WindowId, WindowInfo>,
    subscribers: WindowSubscribers,
}

impl SyntheticWindowCapabilityState {
    fn allocate_window_id(&mut self) -> Result<WindowId, String> {
        let id = self.next_window_id;
        if id == 0 {
            return Err("window identity space exhausted".to_owned());
        }
        self.next_window_id = self.next_window_id.wrapping_add(1);
        Ok(WindowId(id))
    }

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
            next_window_id: 1,
            windows: BTreeMap::new(),
            subscribers: WindowSubscribers::new(),
        })
    }

    #[handler::single]
    fn on_list(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, _mail: ListWindows) -> ListWindowsResult {
        ListWindowsResult::Ok { windows: state.windows.values().cloned().collect() }
    }

    #[handler::single]
    fn on_create(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: CreateWindow) -> CreateWindowResult {
        let id = match state.allocate_window_id() {
            Ok(id) => id,
            Err(error) => return CreateWindowResult::Err { error },
        };
        let (width, height) = mail.spec.size.map_or((DEFAULT_WIDTH, DEFAULT_HEIGHT), |size| (size.width, size.height));
        let window = WindowInfo {
            id,
            title: mail.spec.title,
            mode: mail.spec.mode,
            width,
            height,
            focused: false,
            occluded: width == 0 || height == 0,
        };
        state.windows.insert(id, window.clone());
        state.publish(ctx, id, &WindowOpened { window: window.clone() });
        CreateWindowResult::Ok { window }
    }

    #[handler::single]
    fn on_close(state: &mut Self::State, ctx: &mut NativeCtx<'_>, mail: CloseWindow) -> CloseWindowResult {
        if state.windows.remove(&mail.window).is_none() {
            return CloseWindowResult::Err { window: mail.window, error: format!("unknown window {:?}", mail.window) };
        }
        state.publish(ctx, mail.window, &WindowClosed { window: mail.window });
        CloseWindowResult::Ok { window: mail.window }
    }

    #[handler::single]
    fn on_set_mode(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: SetWindowMode) -> SetWindowModeResult {
        let window = match state.window_mut(mail.window) {
            Ok(window) => window,
            Err(error) => return SetWindowModeResult::Err { window: mail.window, error },
        };
        window.mode.clone_from(&mail.mode);
        if matches!(mail.mode, WindowMode::Windowed)
            && let (Some(width), Some(height)) = (mail.width, mail.height)
        {
            window.width = width;
            window.height = height;
            window.occluded = width == 0 || height == 0;
        }
        SetWindowModeResult::Ok { window: mail.window, mode: mail.mode, width: window.width, height: window.height }
    }

    #[handler::single]
    fn on_set_title(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: SetWindowTitle) -> SetWindowTitleResult {
        let window = match state.window_mut(mail.window) {
            Ok(window) => window,
            Err(error) => return SetWindowTitleResult::Err { window: mail.window, error },
        };
        window.title.clone_from(&mail.title);
        SetWindowTitleResult::Ok { window: mail.window, title: mail.title }
    }

    #[handler::single]
    fn on_focus(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, mail: FocusWindow) -> FocusWindowResult {
        if !state.windows.contains_key(&mail.window) {
            return FocusWindowResult::Err { window: mail.window, error: format!("unknown window {:?}", mail.window) };
        }
        for (id, window) in &mut state.windows {
            window.focused = *id == mail.window;
        }
        FocusWindowResult::Ok { window: mail.window }
    }

    #[handler::single]
    fn on_request_redraw(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        mail: RequestWindowRedraw,
    ) -> RequestWindowRedrawResult {
        match state.windows.get(&mail.window) {
            Some(_) => RequestWindowRedrawResult::Ok { window: mail.window },
            None => RequestWindowRedrawResult::Err {
                window: mail.window,
                error: format!("unknown window {:?}", mail.window),
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
    fn on_monitor_notice(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, notice: MonitorNotice) {
        state.subscribers.purge_departed(notice.target);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use aether_data::Kind;
    use aether_kinds::Key;
    use aether_substrate::Registry;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::MailDispatch;
    use aether_substrate::mail::{MailId, Source};

    use super::*;

    #[test]
    fn explicit_subscriptions_validate_before_mutating_routes() {
        let mailer = Arc::new(Mailer::new(Arc::new(Registry::new())));
        let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), aether_data::MailboxId(1)));
        let mut ctx = NativeCtx::new(&binding, Source::NONE, MailId::NONE, MailId::NONE);
        let mut state = SyntheticWindowCapabilityState {
            next_window_id: 1,
            windows: BTreeMap::new(),
            subscribers: WindowSubscribers::new(),
        };
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
}
