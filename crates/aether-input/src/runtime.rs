//! The `aether.input` runtime half (ADR-0122 identity/runtime split):
//! the `aether_substrate`-typed imports, the state struct + its `fanout`
//! helper, and the shared mailbox-validation fn, gated once by this module
//! rather than per-import. The `#[actor] impl` reaches them through the
//! single `use runtime::*` glob in the parent.

use super::{
    ImePreedit, InputCapability, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel,
    SubscribeInput, SubscribeInputSelf, TextInput, UnsubscribeAll, UnsubscribeInput, UnsubscribeInputSelf, WindowSize,
};
use aether_actor::runtime;

#[cfg(not(target_family = "wasm"))]
use super::SubscribeInputResult;

use aether_kinds::MonitorNotice;
use aether_substrate::actor::monitor::MonitorHandle;

pub use aether_data::{Kind, KindId};
pub use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
pub use aether_substrate::chassis::error::BootError;
pub use aether_substrate::mail::MailboxId;
pub use aether_substrate::mail::registry::{MailboxEntry, Registry};
pub use std::collections::{BTreeSet, HashMap};
pub use std::sync::Arc;

pub use crate::config::InputConfig;

/// `aether.input` runtime state (ADR-0021). Owns the substrate registry
/// handle (for subscriber-mailbox validation) plus the subscriber table
/// keyed by stream kind id. Plain-field shape (ADR-0078) — single-
/// threaded, every handler runs on the cap's dispatcher thread, so no
/// `Mutex` / `Arc<Atomic*>` is needed. The addressing identity is the
/// distinct ZST `InputCapability`.
pub struct InputCapabilityState {
    pub registry: Arc<Registry>,
    pub subscribers: HashMap<KindId, BTreeSet<MailboxId>>,
    /// One monitor per subscribing mailbox (ADR-0079 §8 amended),
    /// registered on its first subscription and released when its
    /// [`MonitorNotice`] purges the mailbox. The handle's `Drop`
    /// deregisters, so the map is both the dedup guard and the RAII
    /// anchor.
    pub monitors: HashMap<MailboxId, MonitorHandle>,
}

impl InputCapabilityState {
    /// Monitor `mailbox` on its first subscription so the cap purges
    /// the mailbox's rows itself when the occupant departs — vacate or
    /// close, whichever comes first (ADR-0079 §8 amended). An `Err`
    /// (an actor outside the registry, or a spawner-less test binding)
    /// means "not monitorable": the rows then live until substrate
    /// teardown, exactly as they would for a mailbox that never goes
    /// away.
    pub fn watch<M: aether_actor::ReplyMode>(&mut self, ctx: &mut NativeCtx<'_, M>, mailbox: MailboxId) {
        if !self.monitors.contains_key(&mailbox)
            && let Ok(handle) = ctx.monitor(mailbox)
        {
            self.monitors.insert(mailbox, handle);
        }
    }

    /// Push one mail per subscriber for `K`. Routes through
    /// [`NativeCtx::fanout`] so each subscriber-bound copy carries
    /// the inbound `(mail_id, root)` as `parent_mail` +
    /// `inherited_root` — the trace observer sees N children
    /// fanning out under the same parent edge (ADR-0080 §6,
    /// issue iamacoffeepot/aether#723).
    pub fn fanout<K: Kind>(&self, ctx: &mut NativeCtx<'_>, payload: &K) {
        let Some(subs) = self.subscribers.get(&K::ID) else {
            return;
        };
        ctx.fanout(subs.iter().copied(), payload);
    }
}

/// Shared validation: the mailbox id must name a live (non-dropped)
/// dispatchable mailbox. Issue 634 Phase 4 collapsed Component
/// and chassis-bound mailboxes into a single `Closure` variant —
/// trampolines and chassis caps both pass this check today.
/// Issue 838 added a `Sink` variant (synchronous-handler
/// mailboxes); production callers (the input stream fan-out)
/// only address trampoline mailboxes here, but accepting `Sink`
/// too keeps the check from rejecting legitimate sync-handler
/// subscribers if any future driver wants one.
pub fn validate_subscriber_mailbox(registry: &Registry, id: MailboxId) -> Result<(), String> {
    match registry.entry(id) {
        Some(MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)) => Ok(()),
        Some(MailboxEntry::Dropped) => Err(format!("mailbox {id:?} already dropped")),
        None => Err(format!("unknown mailbox id {id:?}")),
    }
}

#[runtime]
impl NativeActor for InputCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// registry handle + subscriber table.
    type State = InputCapabilityState;

    type Config = InputConfig;
    const NAMESPACE: &'static str = "aether.input";

    fn init(_config: InputConfig, ctx: &mut NativeInitCtx<'_>) -> Result<InputCapabilityState, BootError> {
        let registry = Arc::clone(ctx.mailer().registry());
        Ok(InputCapabilityState { registry, subscribers: HashMap::new(), monitors: HashMap::new() })
    }

    /// Subscribe a mailbox to an input stream (ADR-0021).
    ///
    /// # Agent
    /// `SubscribeInput { kind, mailbox }`. Component mailboxes only —
    /// sinks and dropped mailboxes are rejected.
    #[handler::single]
    fn on_subscribe(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: SubscribeInput) -> SubscribeInputResult {
        match validate_subscriber_mailbox(&state.registry, payload.mailbox) {
            Ok(()) => {
                state.subscribers.entry(payload.kind).or_default().insert(payload.mailbox);
                state.watch(ctx, payload.mailbox);
                SubscribeInputResult::Ok
            }
            Err(error) => SubscribeInputResult::Err { error },
        }
    }

    /// Subscribe the *sending* actor to an input stream (ADR-0021,
    /// ADR-0083). Resolves the subscriber from the inbound
    /// envelope's host-stamped `Source` via
    /// [`source_mailbox`](NativeCtx::source_mailbox) rather than a
    /// caller-supplied mailbox, so the subscriber cannot be forged
    /// and the reflexive op is gated to in-process actors by
    /// construction — a sender with no local mailbox (an external
    /// session or another engine) gets an `Err` reply and is
    /// subscribed to nothing. The host stamp already names a live
    /// component mailbox, so no [`validate_subscriber_mailbox`]
    /// pass is needed on this path.
    ///
    /// # Agent
    /// `SubscribeInputSelf { kind }`.
    #[handler::single]
    fn on_subscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: SubscribeInputSelf,
    ) -> SubscribeInputResult {
        match ctx.source_mailbox() {
            Some(mailbox) => {
                state.subscribers.entry(payload.kind).or_default().insert(mailbox);
                state.watch(ctx, mailbox);
                SubscribeInputResult::Ok
            }
            None => SubscribeInputResult::Err {
                error: "aether.input.subscribe_self requires a local component sender; an \
                        external session or remote engine must use aether.input.subscribe \
                        with an explicit mailbox"
                    .to_string(),
            },
        }
    }

    /// Unsubscribe a mailbox from an input stream (ADR-0021).
    ///
    /// # Agent
    /// `UnsubscribeInput { kind, mailbox }`. Idempotent on
    /// "not currently subscribed"; rejects unknown / sink mailboxes.
    #[handler::single]
    fn on_unsubscribe(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        payload: UnsubscribeInput,
    ) -> SubscribeInputResult {
        match validate_subscriber_mailbox(&state.registry, payload.mailbox) {
            Ok(()) => {
                if let Some(set) = state.subscribers.get_mut(&payload.kind) {
                    set.remove(&payload.mailbox);
                }
                SubscribeInputResult::Ok
            }
            Err(error) => SubscribeInputResult::Err { error },
        }
    }

    /// Unsubscribe the *sending* actor from an input stream
    /// (ADR-0021, ADR-0083). Resolves the subscriber from the
    /// inbound's host-stamped `Source`, mirroring
    /// [`Self::on_subscribe_self`]. `None` (no local sender) replies
    /// `Err`. Idempotent on "not currently subscribed."
    ///
    /// # Agent
    /// `UnsubscribeInputSelf { kind }`.
    #[handler::single]
    fn on_unsubscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: UnsubscribeInputSelf,
    ) -> SubscribeInputResult {
        match ctx.source_mailbox() {
            Some(mailbox) => {
                if let Some(set) = state.subscribers.get_mut(&payload.kind) {
                    set.remove(&mailbox);
                }
                SubscribeInputResult::Ok
            }
            None => SubscribeInputResult::Err {
                error: "aether.input.unsubscribe_self requires a local component sender; an \
                        external session or remote engine must use aether.input.unsubscribe \
                        with an explicit mailbox"
                    .to_string(),
            },
        }
    }

    /// Remove `mailbox` from every input stream's subscriber set in
    /// one shot. The externally sendable bulk form — drop-time cleanup
    /// happens through [`Self::on_monitor_notice`] instead, so nothing
    /// mails this on the component path anymore. No
    /// mailbox-validation: the target may already be torn down; we
    /// accept any id and purge it from the table.
    ///
    /// # Agent
    /// `UnsubscribeAll { mailbox }`. Idempotent.
    #[handler::single]
    fn on_unsubscribe_all(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, payload: UnsubscribeAll) {
        for set in state.subscribers.values_mut() {
            set.remove(&payload.mailbox);
        }
    }

    /// Purge a departed mailbox (ADR-0079 §8 amended). The substrate
    /// fires one notice per [`InputCapabilityState::watch`]ed mailbox
    /// when it vacates (the wasm trampoline on `DropComponent`) or
    /// closes, so a dropped component's streams stop fanning at its
    /// mailbox without any drop-time fan-out from the component host.
    /// Releasing the handle keeps the monitor map bounded by live
    /// subscribers; a later occupant of the same mailbox re-registers
    /// through its own subscribe.
    #[handler::single]
    fn on_monitor_notice(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, notice: MonitorNotice) {
        state.monitors.remove(&notice.target);
        for set in state.subscribers.values_mut() {
            set.remove(&notice.target);
        }
    }

    /// Key-press fan-out.
    #[handler::single]
    fn on_key(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: Key) {
        state.fanout(ctx, &payload);
    }

    /// Key-release fan-out (paired with [`Key`] for hold-to-act
    /// semantics).
    #[handler::single]
    fn on_key_release(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: KeyRelease) {
        state.fanout(ctx, &payload);
    }

    /// Cursor-move fan-out.
    #[handler::single]
    fn on_mouse_move(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: MouseMove) {
        state.fanout(ctx, &payload);
    }

    /// Mouse-press fan-out.
    #[handler::single]
    fn on_mouse_button(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: MouseButton) {
        state.fanout(ctx, &payload);
    }

    /// Mouse-release fan-out (paired with [`MouseButton`] for
    /// press-move-release drag).
    #[handler::single]
    fn on_mouse_button_release(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: MouseButtonRelease) {
        state.fanout(ctx, &payload);
    }

    /// Mouse-wheel fan-out.
    #[handler::single]
    fn on_mouse_wheel(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: MouseWheel) {
        state.fanout(ctx, &payload);
    }

    /// Window-resize fan-out.
    #[handler::single]
    fn on_window_size(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: WindowSize) {
        state.fanout(ctx, &payload);
    }

    /// Committed text-input fan-out (layout/IME-resolved characters).
    #[handler::single]
    fn on_text_input(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: TextInput) {
        state.fanout(ctx, &payload);
    }

    /// In-flight IME-composition fan-out.
    #[handler::single]
    fn on_ime_preedit(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: ImePreedit) {
        state.fanout(ctx, &payload);
    }

    /// Modifier-state fan-out (latest-wins chord state).
    #[handler::single]
    fn on_modifiers(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: Modifiers) {
        state.fanout(ctx, &payload);
    }
}

#[cfg(all(test, feature = "runtime"))]
mod tests {
    use super::*;
    use aether_substrate::actor::native::binding::NativeBinding;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::{MailId, Source, SourceAddr};

    fn test_state() -> InputCapabilityState {
        InputCapabilityState {
            registry: Arc::new(Registry::new()),
            subscribers: HashMap::new(),
            monitors: HashMap::new(),
        }
    }

    fn test_mailer() -> Arc<Mailer> {
        Arc::new(Mailer::new(Arc::new(Registry::new())))
    }

    /// A `subscribe_self` carrying a `Component` source lands *that*
    /// mailbox in the stream set (ADR-0083: the cap reads the
    /// subscriber off the host-stamped envelope, not a payload field).
    #[test]
    fn subscribe_self_subscribes_the_component_source() {
        let mut state = test_state();
        let key = <Key as Kind>::ID;
        let sender = MailboxId(0x00C0_FFEE);

        let transport = Arc::new(NativeBinding::new_for_test(test_mailer(), MailboxId(0)));
        let source = Source::to(SourceAddr::Component(sender));
        let mut ctx = NativeCtx::new(&transport, source, MailId::NONE, MailId::NONE);
        InputCapability::on_subscribe_self(&mut state, &mut ctx, SubscribeInputSelf { kind: key });

        assert!(
            state.subscribers.get(&key).is_some_and(|s| s.contains(&sender)),
            "a Component-source subscribe_self lands that mailbox in the stream set"
        );
    }

    /// A `subscribe_self` from a non-`Component` source (an external
    /// session) replies `Err` and subscribes nothing — the reflexive
    /// form is gated to in-process actors by construction.
    #[test]
    fn subscribe_self_rejects_non_component_source() {
        use aether_data::{SessionToken, Uuid};

        let mut state = test_state();
        let key = <Key as Kind>::ID;

        let transport = Arc::new(NativeBinding::new_for_test(test_mailer(), MailboxId(0)));
        let source = Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xFEED))));
        let mut ctx = NativeCtx::new(&transport, source, MailId::NONE, MailId::NONE);
        InputCapability::on_subscribe_self(&mut state, &mut ctx, SubscribeInputSelf { kind: key });

        assert!(
            state.subscribers.get(&key).is_none_or(BTreeSet::is_empty),
            "a non-Component source subscribes nothing"
        );
    }

    /// A `MonitorNotice` purges its target from every stream's set
    /// while co-subscribers survive — the ADR-0079 vacate/close purge
    /// that replaced the component host's drop-time `UnsubscribeAll`
    /// fan-out (issue 3741). Regresses if the notice handler forgets a
    /// stream table or purges more than its target.
    #[test]
    fn monitor_notice_purges_target_from_every_stream() {
        let mut state = test_state();
        let departed = MailboxId(0xDEAD);
        let survivor = MailboxId(0xBEEF);
        let key = <Key as Kind>::ID;
        let wheel = <MouseWheel as Kind>::ID;
        state.subscribers.entry(key).or_default().insert(departed);
        state.subscribers.entry(key).or_default().insert(survivor);
        state.subscribers.entry(wheel).or_default().insert(departed);

        let transport = Arc::new(NativeBinding::new_for_test(test_mailer(), MailboxId(0)));
        let mut ctx = NativeCtx::new(&transport, Source::NONE, MailId::NONE, MailId::NONE);
        InputCapability::on_monitor_notice(&mut state, &mut ctx, MonitorNotice { target: departed });

        assert!(!state.subscribers[&key].contains(&departed), "departed mailbox must leave the Key stream");
        assert!(!state.subscribers[&wheel].contains(&departed), "departed mailbox must leave the MouseWheel stream");
        assert!(state.subscribers[&key].contains(&survivor), "co-subscribers must survive the purge");
    }
}
