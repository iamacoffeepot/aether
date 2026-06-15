//! `aether.input` cap. Owns the ADR-0021 publish/subscribe routing
//! table for substrate input streams (`Key`, `KeyRelease`,
//! `MouseMove`, `MouseButton`, `WindowSize`).
//!
//! `Tick` is not an input stream: it is a frame-lifecycle stage
//! (`aether.lifecycle.tick`) a component subscribes directly on
//! `aether.lifecycle` via `ctx.actor::<LifecycleCapability>()`
//! (ADR-0082). The input cap carries only genuine input interrupts.
//!
//! Issue 640 collapsed the last `Arc<RwLock<HashMap<...>>>` cross-thread
//! share. The cap is the sole owner of the subscriber table, held as a
//! plain field on `&mut self` (single-threaded — every handler runs on
//! the cap's dispatcher thread). Drivers don't read the table; they push
//! input events as mail to `aether.input` and the cap fans out one mail
//! per subscriber via `Mailer::push`. `ComponentHostCapability` mails
//! `SubscribeInput` (one per stream-shaped handler the loaded wasm
//! declares) on load and `UnsubscribeAll` on drop, so cap-state mutation
//! is also mail-driven.
//!
//! Pre-issue-638 the `subscribe_input` / `unsubscribe_input` kinds rode
//! `aether.control`; Phase 2 of the split rehomed them to their real
//! domain so the chassis-internal component-host cap (`aether.component`,
//! formerly `aether.control`) only carries component-lifecycle concerns.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

use aether_actor::FfiActorMailbox;
use aether_data::{Kind, MailboxId};
#[cfg(not(target_arch = "wasm32"))]
use aether_kinds::SubscribeInputResult;
use aether_kinds::{
    Key, KeyRelease, MouseButton, MouseMove, SubscribeInput, SubscribeInputSelf, UnsubscribeAll,
    UnsubscribeInput, UnsubscribeInputSelf, WindowSize,
};
#[cfg(not(target_arch = "wasm32"))]
use aether_substrate::actor::native::NativeActorMailbox;

#[cfg(not(target_arch = "wasm32"))]
pub use native::InputConfig;

/// Sender-side facade for callers addressing [`InputCapability`] via
/// `ctx.actor::<InputCapability>()`.
///
/// Lifts the cap-shaped operations (`subscribe::<K>()`,
/// `subscribe_for::<K>(mailbox)`, the `unsubscribe` twins,
/// `unsubscribe_all(mailbox)`) one indirection above the raw
/// `.send(&SubscribeInput { .. })` so component code stops
/// reconstructing the kind struct at every call site. Same shape and
/// rationale as [`crate::fs::FsMailboxExt`]
/// (issue 580) and [`crate::component::ComponentHostFfiExt`] (issue
/// 654) — the cap module owns receive-side ([`InputCapability`]) AND
/// send-side ([`InputMailboxExt`]) so future kind additions land both
/// surfaces in one place.
///
/// Impl'd for both transports `ctx.actor::<InputCapability>()` can
/// return:
///
/// - [`FfiActorMailbox<InputCapability>`] — always-on, for
///   wasm-component callers.
/// - [`NativeActorMailbox<'_, InputCapability>`] — native cap-to-cap
///   sends, gated on `#[cfg(not(target_arch = "wasm32"))]`.
///
/// All methods are fire-and-forget. `subscribe` / `unsubscribe` reply
/// via `aether.input.subscribe_result`; reply handling stays on the
/// caller. `unsubscribe_all` has no reply (issued by the trampoline on
/// drop, when nobody's listening).
///
/// The generic escape hatch is unaffected: `mailbox.send(&SubscribeInput { .. })`
/// still works for any `K` the cap declares via `HandlesKind<K>`,
/// since `send` is an inherent method on the underlying mailbox type.
pub trait InputMailboxExt {
    /// Mail `aether.input.subscribe_self { kind }` to the cap —
    /// subscribe the *calling* actor to the input stream for `K` (e.g.
    /// `Key` / `MouseMove` / `WindowSize`). The cap resolves the
    /// subscriber from the inbound's host-stamped `Source` (ADR-0083),
    /// so the call site spells out neither the kind id nor its own
    /// mailbox. This is the common form. Idempotent.
    fn subscribe<K: Kind>(&self);

    /// Mail `aether.input.subscribe { kind, mailbox }` to the cap. Add
    /// an *explicit* `mailbox` to the subscriber set for `K`. The rare
    /// cross-mailbox form; [`subscribe`](Self::subscribe) covers the
    /// self case. Idempotent.
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId);

    /// Mail `aether.input.unsubscribe_self { kind }` to the cap —
    /// unsubscribe the *calling* actor from the input stream for `K`.
    /// Reflexive twin of [`subscribe`](Self::subscribe). Idempotent.
    fn unsubscribe<K: Kind>(&self);

    /// Mail `aether.input.unsubscribe { kind, mailbox }` to the cap.
    /// Remove an *explicit* `mailbox` from the subscriber set for `K`.
    /// Idempotent.
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId);

    /// Mail `aether.input.unsubscribe_all { mailbox }` to the cap.
    /// Remove `mailbox` from every input stream's subscriber set;
    /// used by the trampoline on drop. Idempotent; fire-and-forget.
    fn unsubscribe_all(&self, mailbox: MailboxId);
}

impl InputMailboxExt for FfiActorMailbox<InputCapability> {
    fn subscribe<K: Kind>(&self) {
        self.send(&SubscribeInputSelf { kind: K::ID });
    }
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&SubscribeInput {
            kind: K::ID,
            mailbox,
        });
    }
    fn unsubscribe<K: Kind>(&self) {
        self.send(&UnsubscribeInputSelf { kind: K::ID });
    }
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeInput {
            kind: K::ID,
            mailbox,
        });
    }
    fn unsubscribe_all(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeAll { mailbox });
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl InputMailboxExt for NativeActorMailbox<'_, InputCapability> {
    fn subscribe<K: Kind>(&self) {
        self.send(&SubscribeInputSelf { kind: K::ID });
    }
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&SubscribeInput {
            kind: K::ID,
            mailbox,
        });
    }
    fn unsubscribe<K: Kind>(&self) {
        self.send(&UnsubscribeInputSelf { kind: K::ID });
    }
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeInput {
            kind: K::ID,
            mailbox,
        });
    }
    fn unsubscribe_all(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeAll { mailbox });
    }
}

#[aether_actor::bridge(singleton)]
mod native {
    use super::{
        Key, KeyRelease, MouseButton, MouseMove, SubscribeInput, SubscribeInputResult,
        SubscribeInputSelf, UnsubscribeAll, UnsubscribeInput, UnsubscribeInputSelf, WindowSize,
    };
    use aether_actor::actor;
    use aether_data::{Kind, KindId};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::MailboxId;
    use aether_substrate::mail::registry::{MailboxEntry, Registry};
    use std::collections::{BTreeSet, HashMap};
    use std::sync::Arc;

    /// Configuration for [`InputCapability`]. Empty today — the cap
    /// builds its subscriber table from scratch and reaches for
    /// `Mailer` / `Registry` through `NativeInitCtx`. Kept as a struct
    /// so the chassis composes the cap with the same
    /// `Builder::with_actor::<InputCapability>(InputConfig {})` shape
    /// as every other cap and a future config knob (e.g. ring caps,
    /// per-stream gates) lands without API churn.
    #[derive(Default)]
    pub struct InputConfig {}

    /// `aether.input` cap. The single owner of the input-stream
    /// subscriber table. Handles three classes of mail:
    ///
    /// 1. **Subscribe / Unsubscribe / `UnsubscribeAll`** — mutates the
    ///    table on `&mut self`. Reply target: the original sender.
    ///
    /// 2. **Input events** (`Key`, `KeyRelease`, `MouseMove`,
    ///    `MouseButton`, `WindowSize`) — pushed by the chassis driver
    ///    after each platform event; the cap fans out one mail per
    ///    subscriber. Fire-and-forget; no reply.
    ///
    /// Plain-field shape (ADR-0078) — single-threaded, every handler
    /// runs on the cap's dispatcher thread.
    pub struct InputCapability {
        registry: Arc<Registry>,
        subscribers: HashMap<KindId, BTreeSet<MailboxId>>,
    }

    #[actor]
    impl NativeActor for InputCapability {
        type Config = InputConfig;
        const NAMESPACE: &'static str = "aether.input";

        fn init(_config: InputConfig, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let registry = Arc::clone(ctx.mailer().registry());
            Ok(Self {
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
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            payload: SubscribeInput,
        ) -> SubscribeInputResult {
            match validate_subscriber_mailbox(&self.registry, payload.mailbox) {
                Ok(()) => {
                    self.subscribers
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
            &mut self,
            ctx: &mut NativeCtx<'_>,
            payload: SubscribeInputSelf,
        ) -> SubscribeInputResult {
            match ctx.source_mailbox() {
                Some(mailbox) => {
                    self.subscribers
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
            &mut self,
            _ctx: &mut NativeCtx<'_>,
            payload: UnsubscribeInput,
        ) -> SubscribeInputResult {
            match validate_subscriber_mailbox(&self.registry, payload.mailbox) {
                Ok(()) => {
                    if let Some(set) = self.subscribers.get_mut(&payload.kind) {
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
            &mut self,
            ctx: &mut NativeCtx<'_>,
            payload: UnsubscribeInputSelf,
        ) -> SubscribeInputResult {
            match ctx.source_mailbox() {
                Some(mailbox) => {
                    if let Some(set) = self.subscribers.get_mut(&payload.kind) {
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
        fn on_unsubscribe_all(&mut self, _ctx: &mut NativeCtx<'_>, payload: UnsubscribeAll) {
            for set in self.subscribers.values_mut() {
                set.remove(&payload.mailbox);
            }
        }

        /// Key-press fan-out.
        #[handler]
        fn on_key(&mut self, ctx: &mut NativeCtx<'_>, payload: Key) {
            self.fanout(ctx, &payload);
        }

        /// Key-release fan-out (paired with [`Key`] for hold-to-act
        /// semantics).
        #[handler]
        fn on_key_release(&mut self, ctx: &mut NativeCtx<'_>, payload: KeyRelease) {
            self.fanout(ctx, &payload);
        }

        /// Cursor-move fan-out.
        #[handler]
        fn on_mouse_move(&mut self, ctx: &mut NativeCtx<'_>, payload: MouseMove) {
            self.fanout(ctx, &payload);
        }

        /// Mouse-press fan-out. Empty payload.
        #[handler]
        fn on_mouse_button(&mut self, ctx: &mut NativeCtx<'_>, payload: MouseButton) {
            self.fanout(ctx, &payload);
        }

        /// Window-resize fan-out.
        #[handler]
        fn on_window_size(&mut self, ctx: &mut NativeCtx<'_>, payload: WindowSize) {
            self.fanout(ctx, &payload);
        }
    }

    impl InputCapability {
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
    fn validate_subscriber_mailbox(registry: &Registry, id: MailboxId) -> Result<(), String> {
        match registry.entry(id) {
            Some(MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)) => Ok(()),
            Some(MailboxEntry::Dropped) => Err(format!("mailbox {id:?} already dropped")),
            None => Err(format!("unknown mailbox id {id:?}")),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use aether_substrate::actor::native::binding::NativeBinding;
        use aether_substrate::handle_store::HandleStore;
        use aether_substrate::mail::mailer::Mailer;
        use aether_substrate::mail::{MailId, Source, SourceAddr};

        fn test_cap() -> InputCapability {
            InputCapability {
                registry: Arc::new(Registry::new()),
                subscribers: HashMap::new(),
            }
        }

        fn test_mailer() -> Arc<Mailer> {
            Arc::new(Mailer::new(
                Arc::new(Registry::new()),
                Arc::new(HandleStore::new(1024)),
            ))
        }

        /// A `subscribe_self` carrying a `Component` source lands *that*
        /// mailbox in the stream set (ADR-0083: the cap reads the
        /// subscriber off the host-stamped envelope, not a payload field).
        #[test]
        fn subscribe_self_subscribes_the_component_source() {
            let mut cap = test_cap();
            let key = <Key as Kind>::ID;
            let sender = MailboxId(0x00C0_FFEE);

            let transport = Arc::new(NativeBinding::new_for_test(test_mailer(), MailboxId(0)));
            let source = Source::to(SourceAddr::Component(sender));
            let mut ctx = NativeCtx::new(&transport, source, MailId::NONE, MailId::NONE);
            cap.on_subscribe_self(&mut ctx, SubscribeInputSelf { kind: key });

            assert!(
                cap.subscribers
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

            let mut cap = test_cap();
            let key = <Key as Kind>::ID;

            let transport = Arc::new(NativeBinding::new_for_test(test_mailer(), MailboxId(0)));
            let source = Source::to(SourceAddr::Session(SessionToken(Uuid::from_u128(0xFEED))));
            let mut ctx = NativeCtx::new(&transport, source, MailId::NONE, MailId::NONE);
            cap.on_subscribe_self(&mut ctx, SubscribeInputSelf { kind: key });

            assert!(
                cap.subscribers.get(&key).is_none_or(BTreeSet::is_empty),
                "a non-Component source subscribes nothing"
            );
        }
    }
}
