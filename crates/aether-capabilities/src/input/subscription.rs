//! Native subscriber-table cap for `aether.input`.

use aether_kinds::{Key, KeyRelease, MouseButton, MouseMove, WindowSize};

// Handler-signature kinds must be importable at module root because
// `#[actor]` emits `impl HandlesKind<K> for InputCapability {}` markers
// always-on, outside the `feature = "runtime"` gate. The reply kind
// (`SubscribeInputResult`) is named only by the gated handler bodies, so
// it rides the runtime gate below.
use super::kinds::{
    SubscribeInput, SubscribeInputSelf, UnsubscribeAll, UnsubscribeInput, UnsubscribeInputSelf,
};

use aether_actor::actor;

/// `aether.input` cap **identity** (ADR-0122 identity/runtime split). A
/// ZST carrying only the addressing — the `Addressable` / `HandlesKind`
/// markers and the name-inventory entry, all emitted always-on by
/// `#[actor]`. The state-bearing runtime ([`InputCapabilityState`],
/// holding the substrate registry handle + the subscriber table) lives
/// behind the one `feature = "runtime"` gate, so a transport-only build
/// never names it nor pulls `aether_substrate` through this cap.
///
/// The single owner of the input-stream subscriber table. Handles two
/// classes of mail:
///
/// 1. **Subscribe / Unsubscribe / `UnsubscribeAll`** — mutates the
///    table on the runtime state. Reply target: the original sender.
///
/// 2. **Input events** (`Key`, `KeyRelease`, `MouseMove`,
///    `MouseButton`, `WindowSize`) — pushed by the chassis driver
///    after each platform event; the cap fans out one mail per
///    subscriber. Fire-and-forget; no reply.
pub struct InputCapability;

// The reply kind rides the native gate (not `runtime`): the `#[actor]`
// macro's ADR-0109 `HandlerEntry` inventory submission — emitted on every
// native build, runtime or not — names each handler's reply kind `::ID`,
// so a transport-only build must still see it. The rest of the runtime
// half (the `aether_substrate`-typed imports, the state struct + its
// `fanout` helper, and the shared mailbox-validation fn) sits behind the
// one `feature = "runtime"` gate.
#[cfg(not(target_arch = "wasm32"))]
use super::kinds::SubscribeInputResult;
#[cfg(feature = "runtime")]
use aether_data::{Kind, KindId};
#[cfg(feature = "runtime")]
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
#[cfg(feature = "runtime")]
use aether_substrate::chassis::error::BootError;
#[cfg(feature = "runtime")]
use aether_substrate::mail::MailboxId;
#[cfg(feature = "runtime")]
use aether_substrate::mail::registry::{MailboxEntry, Registry};
#[cfg(feature = "runtime")]
use std::collections::{BTreeSet, HashMap};
#[cfg(feature = "runtime")]
use std::sync::Arc;

#[cfg(feature = "runtime")]
use crate::input::config::InputConfig;

/// `aether.input` runtime state (ADR-0021). Owns the substrate registry
/// handle (for subscriber-mailbox validation) plus the subscriber table
/// keyed by stream kind id. Plain-field shape (ADR-0078) — single-
/// threaded, every handler runs on the cap's dispatcher thread, so no
/// `Mutex` / `Arc<Atomic*>` is needed. The addressing identity is the
/// distinct ZST `InputCapability`.
#[cfg(feature = "runtime")]
pub struct InputCapabilityState {
    registry: Arc<Registry>,
    subscribers: HashMap<KindId, BTreeSet<MailboxId>>,
}

#[actor(singleton)]
impl NativeActor for InputCapability {
    /// The runtime state this identity boots into (ADR-0122 split): the
    /// registry handle + subscriber table.
    type State = InputCapabilityState;

    type Config = InputConfig;
    const NAMESPACE: &'static str = "aether.input";

    fn init(
        _config: InputConfig,
        ctx: &mut NativeInitCtx<'_>,
    ) -> Result<InputCapabilityState, BootError> {
        let registry = Arc::clone(ctx.mailer().registry());
        Ok(InputCapabilityState {
            registry,
            subscribers: HashMap::new(),
        })
    }

    /// Subscribe a mailbox to an input stream (ADR-0021).
    ///
    /// # Agent
    /// `SubscribeInput { kind, mailbox }`. Component mailboxes only —
    /// sinks and dropped mailboxes are rejected.
    #[handler]
    fn on_subscribe(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        payload: SubscribeInput,
    ) -> SubscribeInputResult {
        match validate_subscriber_mailbox(&state.registry, payload.mailbox) {
            Ok(()) => {
                state
                    .subscribers
                    .entry(payload.kind)
                    .or_default()
                    .insert(payload.mailbox);
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
    #[handler]
    fn on_subscribe_self(
        state: &mut Self::State,
        ctx: &mut NativeCtx<'_>,
        payload: SubscribeInputSelf,
    ) -> SubscribeInputResult {
        match ctx.source_mailbox() {
            Some(mailbox) => {
                state
                    .subscribers
                    .entry(payload.kind)
                    .or_default()
                    .insert(mailbox);
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
    #[handler]
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
    #[handler]
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

    /// Remove `mailbox` from every input stream's subscriber set.
    /// Issued by `ComponentHostCapability` on `DropComponent` so a
    /// dropped trampoline doesn't keep receiving fan-out mail.
    /// No mailbox-validation: the trampoline's mailbox is already
    /// torn down by the time this fires; we accept any id and
    /// purge it from the table.
    ///
    /// # Agent
    /// `UnsubscribeAll { mailbox }`. Idempotent.
    #[handler]
    fn on_unsubscribe_all(
        state: &mut Self::State,
        _ctx: &mut NativeCtx<'_>,
        payload: UnsubscribeAll,
    ) {
        for set in state.subscribers.values_mut() {
            set.remove(&payload.mailbox);
        }
    }

    /// Key-press fan-out.
    #[handler]
    fn on_key(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: Key) {
        state.fanout(ctx, &payload);
    }

    /// Key-release fan-out (paired with [`Key`] for hold-to-act
    /// semantics).
    #[handler]
    fn on_key_release(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: KeyRelease) {
        state.fanout(ctx, &payload);
    }

    /// Cursor-move fan-out.
    #[handler]
    fn on_mouse_move(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: MouseMove) {
        state.fanout(ctx, &payload);
    }

    /// Mouse-press fan-out. Empty payload.
    #[handler]
    fn on_mouse_button(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: MouseButton) {
        state.fanout(ctx, &payload);
    }

    /// Window-resize fan-out.
    #[handler]
    fn on_window_size(state: &mut Self::State, ctx: &mut NativeCtx<'_>, payload: WindowSize) {
        state.fanout(ctx, &payload);
    }
}

#[cfg(feature = "runtime")]
impl InputCapabilityState {
    /// Push one mail per subscriber for `K`. Routes through
    /// [`NativeCtx::fanout`] so each subscriber-bound copy carries
    /// the inbound `(mail_id, root)` as `parent_mail` +
    /// `inherited_root` — the trace observer sees N children
    /// fanning out under the same parent edge (ADR-0080 §6,
    /// issue iamacoffeepot/aether#723).
    fn fanout<K: Kind>(&self, ctx: &mut NativeCtx<'_>, payload: &K) {
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
#[cfg(feature = "runtime")]
fn validate_subscriber_mailbox(registry: &Registry, id: MailboxId) -> Result<(), String> {
    match registry.entry(id) {
        Some(MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)) => Ok(()),
        Some(MailboxEntry::Dropped) => Err(format!("mailbox {id:?} already dropped")),
        None => Err(format!("unknown mailbox id {id:?}")),
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
            state
                .subscribers
                .get(&key)
                .is_some_and(|s| s.contains(&sender)),
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
}
