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

use aether_kinds::{
    Key, KeyRelease, MouseButton, MouseMove, SubscribeInput, Tick, UnsubscribeAll,
    UnsubscribeInput, WindowSize,
};
#[cfg(not(target_arch = "wasm32"))]
use aether_kinds::SubscribeInputResult;

#[cfg(not(target_arch = "wasm32"))]
pub use native::InputConfig;

#[aether_actor::bridge(singleton)]
mod native {
    use super::{
        Key, KeyRelease, MouseButton, MouseMove, SubscribeInput, SubscribeInputResult, Tick,
        UnsubscribeAll, UnsubscribeInput, WindowSize,
    };
    use aether_actor::actor;
    use aether_actor::actor::ctx::OutboundReply;
    use aether_data::{Kind, KindId, encode};
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::registry::{MailboxEntry, Registry};
    use aether_substrate::mail::{Mail, MailboxId};
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
        mailer: Arc<Mailer>,
        subscribers: HashMap<KindId, BTreeSet<MailboxId>>,
    }

    #[actor]
    impl NativeActor for InputCapability {
        type Config = InputConfig;
        const NAMESPACE: &'static str = "aether.input";

        fn init(_config: InputConfig, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let mailer = ctx.mailer();
            let registry = Arc::clone(mailer.registry());
            Ok(Self {
                registry,
                mailer,
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
        fn on_tick(&mut self, _ctx: &mut NativeCtx<'_>, payload: Tick) {
            self.fanout(Tick::ID, &payload);
        }

        /// Key-press fan-out.
        #[handler]
        fn on_key(&mut self, _ctx: &mut NativeCtx<'_>, payload: Key) {
            self.fanout(Key::ID, &payload);
        }

        /// Key-release fan-out (paired with [`Key`] for hold-to-act
        /// semantics).
        #[handler]
        fn on_key_release(&mut self, _ctx: &mut NativeCtx<'_>, payload: KeyRelease) {
            self.fanout(KeyRelease::ID, &payload);
        }

        /// Cursor-move fan-out.
        #[handler]
        fn on_mouse_move(&mut self, _ctx: &mut NativeCtx<'_>, payload: MouseMove) {
            self.fanout(MouseMove::ID, &payload);
        }

        /// Mouse-press fan-out. Empty payload.
        #[handler]
        fn on_mouse_button(&mut self, _ctx: &mut NativeCtx<'_>, payload: MouseButton) {
            self.fanout(MouseButton::ID, &payload);
        }

        /// Window-resize fan-out.
        #[handler]
        fn on_window_size(&mut self, _ctx: &mut NativeCtx<'_>, payload: WindowSize) {
            self.fanout(WindowSize::ID, &payload);
        }
    }

    impl InputCapability {
        /// Push one mail per subscriber for `kind`. Re-encodes
        /// `payload` once and clones the `Vec<u8>` per recipient — the
        /// payloads are tiny (cast-shaped, ≤ 8 bytes) so per-subscriber
        /// allocation churn is negligible at 60Hz tick / 1kHz mouse
        /// move cadence.
        fn fanout<K: Kind + bytemuck::NoUninit>(&self, kind: KindId, payload: &K) {
            let Some(subs) = self.subscribers.get(&kind) else {
                return;
            };
            if subs.is_empty() {
                return;
            }
            let bytes = encode(payload);
            for mbox in subs {
                self.mailer.push(Mail::new(*mbox, kind, bytes.clone(), 1));
            }
        }
    }

    /// Shared validation: the mailbox id must name a live (non-dropped)
    /// closure-bound mailbox. Issue 634 Phase 4 collapsed Component
    /// and chassis-bound mailboxes into a single `Closure` variant —
    /// trampolines and chassis caps both pass this check today.
    /// Production callers (the input stream fan-out) only address
    /// trampoline mailboxes here; chassis caps don't subscribe to
    /// themselves.
    fn validate_subscriber_mailbox(registry: &Registry, id: MailboxId) -> Result<(), String> {
        match registry.entry(id) {
            Some(MailboxEntry::Closure(_)) => Ok(()),
            Some(MailboxEntry::Dropped) => Err(format!("mailbox {:?} already dropped", id)),
            None => Err(format!("unknown mailbox id {:?}", id)),
        }
    }
}
