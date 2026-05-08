//! `aether.input` cap. Owns the ADR-0021 publish/subscribe routing
//! table for substrate input streams (`Tick`, `Key`, `MouseMove`,
//! `MouseButton`, `WindowSize`). Pre-issue-638 the `subscribe_input` /
//! `unsubscribe_input` kinds rode `aether.control`; Phase 2 of the
//! split rehomed them to their real domain so the chassis-internal
//! component-host cap (`aether.component` post-issue-638-Phase-3,
//! formerly `aether.control`) only carries component-lifecycle
//! concerns.
//!
//! The subscriber table itself is genuinely cross-thread shared — the
//! platform thread reads it on every published input event while this
//! cap mutates it on subscribe/unsubscribe. The substrate creates one
//! `InputSubscribers: Arc<RwLock<HashMap<KindId, BTreeSet<MailboxId>>>>`
//! at boot and clones it into both the cap config and every chassis
//! driver. `ComponentHostCapability` also holds a clone for its
//! synchronous load-time auto-subscribe and drop-time cleanup paths
//! (issue 634 will retire those into mail).

use aether_kinds::{SubscribeInput, SubscribeInputResult, UnsubscribeInput};

#[cfg(not(target_arch = "wasm32"))]
pub use native::InputConfig;

#[aether_actor::bridge(singleton)]
mod native {
    use super::{SubscribeInput, SubscribeInputResult, UnsubscribeInput};
    use aether_actor::actor;
    use aether_actor::actor::ctx::OutboundReply;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::input::InputSubscribers;
    use aether_substrate::mail::MailboxId;
    use aether_substrate::mail::registry::{MailboxEntry, Registry};
    use std::sync::Arc;

    /// Configuration for [`InputCapability`]. The `input_subscribers`
    /// table is the same `Arc<RwLock<HashMap<...>>>` `SubstrateBoot`
    /// minted at boot, cloned into every reader (platform thread) and
    /// every other writer (`ComponentHostCapability` for load/drop
    /// synchronous mutations).
    pub struct InputConfig {
        pub input_subscribers: InputSubscribers,
    }

    /// `aether.input` cap. Handles `SubscribeInput` / `UnsubscribeInput`
    /// mail by inserting / removing the carried `mailbox` from the
    /// subscriber set keyed on `kind`. Replies with
    /// `SubscribeInputResult::Ok` on success or `Err { error }` if
    /// `mailbox` doesn't name a live component (unknown / sink /
    /// dropped).
    pub struct InputCapability {
        registry: Arc<Registry>,
        subscribers: InputSubscribers,
    }

    #[actor]
    impl NativeActor for InputCapability {
        type Config = InputConfig;
        const NAMESPACE: &'static str = "aether.input";
        const SCHEDULING: Scheduling = Scheduling::Dedicated;

        fn init(config: InputConfig, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            let registry = Arc::clone(ctx.mailer().registry());
            Ok(Self {
                registry,
                subscribers: config.input_subscribers,
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
                        .write()
                        .unwrap()
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
                    if let Some(set) = self.subscribers.write().unwrap().get_mut(&payload.kind) {
                        set.remove(&payload.mailbox);
                    }
                    SubscribeInputResult::Ok
                }
                Err(error) => SubscribeInputResult::Err { error },
            };
            ctx.reply(&result);
        }
    }

    impl InputCapability {
        /// Test-support constructor. Builds the cap with the supplied
        /// `registry` and shared `subscribers` table without going
        /// through the chassis builder. Cross-crate integration tests
        /// (the `aether-substrate-bundle` `input_subscriptions.rs`
        /// suite) reach for this when they want to drive
        /// subscribe/unsubscribe synchronously alongside a separately
        /// booted `ComponentHostCapability::for_test` — both caps hold
        /// clones of the same `InputSubscribers` Arc.
        #[doc(hidden)]
        pub fn for_test(registry: Arc<Registry>, subscribers: InputSubscribers) -> Self {
            Self {
                registry,
                subscribers,
            }
        }

        /// Test-support: dispatch a typed `SubscribeInput` payload through
        /// the cap's subscribe path synchronously. Mirrors the
        /// `aether.input.subscribe` mail end-to-end minus the dispatcher
        /// thread hop, so unit tests can assert on the result without
        /// booting a full chassis.
        #[doc(hidden)]
        pub fn subscribe_for_test(&self, payload: SubscribeInput) -> SubscribeInputResult {
            match validate_subscriber_mailbox(&self.registry, payload.mailbox) {
                Ok(()) => {
                    self.subscribers
                        .write()
                        .unwrap()
                        .entry(payload.kind)
                        .or_default()
                        .insert(payload.mailbox);
                    SubscribeInputResult::Ok
                }
                Err(error) => SubscribeInputResult::Err { error },
            }
        }

        /// Test-support counterpart of [`Self::subscribe_for_test`].
        #[doc(hidden)]
        pub fn unsubscribe_for_test(&self, payload: UnsubscribeInput) -> SubscribeInputResult {
            match validate_subscriber_mailbox(&self.registry, payload.mailbox) {
                Ok(()) => {
                    if let Some(set) = self.subscribers.write().unwrap().get_mut(&payload.kind) {
                        set.remove(&payload.mailbox);
                    }
                    SubscribeInputResult::Ok
                }
                Err(error) => SubscribeInputResult::Err { error },
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
