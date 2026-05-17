//! `aether.input` cap. Owns the ADR-0021 publish/subscribe routing
//! table for substrate input streams (`Tick`, `Key`, `KeyRelease`,
//! `MouseMove`, `MouseButton`, `WindowSize`).
//!
//! Issue 640 collapsed the last `Arc<RwLock<HashMap<...>>>` cross-thread
//! share. The cap is the sole owner of the subscriber table, held as a
//! plain field on `&mut self` (single-threaded — every handler runs on
//! the cap's dispatcher thread). Drivers don't read the table; they push
//! input events as mail to `aether.input` and the cap fans out one mail
//! per subscriber via [`Mailer::push`]. `ComponentHostCapability` mails
//! `SubscribeInput` (one per stream-shaped handler the loaded wasm
//! declares) on load and `UnsubscribeAll` on drop, so cap-state mutation
//! is also mail-driven.
//!
//! Pre-issue-638 the `subscribe_input` / `unsubscribe_input` kinds rode
//! `aether.control`; Phase 2 of the split rehomed them to their real
//! domain so the chassis-internal component-host cap (`aether.component`,
//! formerly `aether.control`) only carries component-lifecycle concerns.

use aether_actor::FfiActorMailbox;
use aether_data::{KindId, MailboxId};
#[cfg(not(target_arch = "wasm32"))]
use aether_kinds::SubscribeInputResult;
use aether_kinds::{
    Key, KeyRelease, MouseButton, MouseMove, SubscribeInput, Tick, UnsubscribeAll,
    UnsubscribeInput, WindowSize,
};
#[cfg(not(target_arch = "wasm32"))]
use aether_substrate::actor::native::NativeActorMailbox;

#[cfg(not(target_arch = "wasm32"))]
pub use native::InputConfig;

/// Sender-side facade for callers addressing [`InputCapability`] via
/// `ctx.actor::<InputCapability>()`.
///
/// Lifts the cap-shaped operations (`subscribe(kind, mailbox)`,
/// `unsubscribe(kind, mailbox)`, `unsubscribe_all(mailbox)`) one
/// indirection above the raw `.send(&SubscribeInput { .. })` so
/// component code stops reconstructing the kind struct at every call
/// site. Same shape and rationale as [`crate::fs::FsMailboxExt`]
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
    /// Mail `aether.input.subscribe { kind, mailbox }` to the cap.
    /// Add `mailbox` to the subscriber set for `kind`. Idempotent.
    fn subscribe(&self, kind: KindId, mailbox: MailboxId);

    /// Mail `aether.input.unsubscribe { kind, mailbox }` to the cap.
    /// Remove `mailbox` from the subscriber set for `kind`. Idempotent.
    fn unsubscribe(&self, kind: KindId, mailbox: MailboxId);

    /// Mail `aether.input.unsubscribe_all { mailbox }` to the cap.
    /// Remove `mailbox` from every input stream's subscriber set;
    /// used by the trampoline on drop. Idempotent; fire-and-forget.
    fn unsubscribe_all(&self, mailbox: MailboxId);
}

impl InputMailboxExt for FfiActorMailbox<InputCapability> {
    fn subscribe(&self, kind: KindId, mailbox: MailboxId) {
        self.send(&SubscribeInput { kind, mailbox });
    }
    fn unsubscribe(&self, kind: KindId, mailbox: MailboxId) {
        self.send(&UnsubscribeInput { kind, mailbox });
    }
    fn unsubscribe_all(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeAll { mailbox });
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl<'a> InputMailboxExt for NativeActorMailbox<'a, InputCapability> {
    fn subscribe(&self, kind: KindId, mailbox: MailboxId) {
        self.send(&SubscribeInput { kind, mailbox });
    }
    fn unsubscribe(&self, kind: KindId, mailbox: MailboxId) {
        self.send(&UnsubscribeInput { kind, mailbox });
    }
    fn unsubscribe_all(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeAll { mailbox });
    }
}

#[aether_actor::bridge(singleton)]
mod native {
    use super::{
        Key, KeyRelease, MouseButton, MouseMove, SubscribeInput, SubscribeInputResult, Tick,
        UnsubscribeAll, UnsubscribeInput, WindowSize,
    };
    use aether_actor::actor;
    use aether_actor::actor::ctx::OutboundReply;
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
    /// 1. **Subscribe / Unsubscribe / UnsubscribeAll** — mutates the
    ///    table on `&mut self`. Reply target: the original sender.
    ///
    /// 2. **Input events** (`Tick`, `Key`, `KeyRelease`, `MouseMove`,
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
        fn on_subscribe(&mut self, ctx: &mut NativeCtx<'_>, payload: SubscribeInput) {
            let result = match validate_subscriber_mailbox(&self.registry, payload.mailbox) {
                Ok(()) => {
                    self.subscribers
                        .entry(payload.kind)
                        .or_default()
                        .insert(payload.mailbox);
                    SubscribeInputResult::Ok
                }
                Err(error) => SubscribeInputResult::Err { error },
            };
            ctx.reply(&result);
        }

        /// Unsubscribe a mailbox from an input stream (ADR-0021).
        ///
        /// # Agent
        /// `UnsubscribeInput { kind, mailbox }`. Idempotent on
        /// "not currently subscribed"; rejects unknown / sink mailboxes.
        #[handler]
        fn on_unsubscribe(&mut self, ctx: &mut NativeCtx<'_>, payload: UnsubscribeInput) {
            let result = match validate_subscriber_mailbox(&self.registry, payload.mailbox) {
                Ok(()) => {
                    if let Some(set) = self.subscribers.get_mut(&payload.kind) {
                        set.remove(&payload.mailbox);
                    }
                    SubscribeInputResult::Ok
                }
                Err(error) => SubscribeInputResult::Err { error },
            };
            ctx.reply(&result);
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

        /// Per-frame tick fan-out (ADR-0021). Empty payload.
        #[handler]
        fn on_tick(&mut self, ctx: &mut NativeCtx<'_>, payload: Tick) {
            self.fanout(ctx, &payload);
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
            Some(MailboxEntry::Inbox(_)) | Some(MailboxEntry::Inline(_)) => Ok(()),
            Some(MailboxEntry::Dropped) => Err(format!("mailbox {:?} already dropped", id)),
            None => Err(format!("unknown mailbox id {:?}", id)),
        }
    }
}
