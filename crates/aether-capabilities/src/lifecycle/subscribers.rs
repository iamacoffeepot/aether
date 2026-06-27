//! Sender side + fan-out for the lifecycle cap. Holds the
//! [`LifecycleMailboxExt`] facade callers reach through
//! `ctx.actor::<LifecycleCapability>()` (always-on, both transports) and
//! the native [`broadcast_to_subscribers`] fan-out the receive side calls
//! once per advance.

use aether_actor::WasmActorMailbox;
use aether_data::{Kind, MailboxId};
use aether_kinds::{
    LifecycleSubscribe, LifecycleSubscribeSelf, LifecycleUnsubscribe, LifecycleUnsubscribeSelf,
};

use super::LifecycleCapability;

#[cfg(not(target_family = "wasm"))]
use aether_actor::ReplyMode;
#[cfg(not(target_family = "wasm"))]
use aether_data::KindId;
#[cfg(not(target_family = "wasm"))]
use aether_substrate::actor::native::{NativeActorMailbox, NativeCtx};
#[cfg(not(target_family = "wasm"))]
use aether_substrate::mail::MailboxId as SubstrateMailboxId;
#[cfg(not(target_family = "wasm"))]
use std::collections::{BTreeMap, BTreeSet};

/// Sender-side facade for callers addressing [`LifecycleCapability`]
/// via `ctx.actor::<LifecycleCapability>()` (ADR-0082 §7, §12).
///
/// Lifts the stage-subscribe operations one indirection above the raw
/// `.send(&LifecycleSubscribe { .. })` so component code stops
/// reconstructing the kind struct (and the `.0` field unwraps) at every
/// call site — same shape and rationale as
/// [`InputMailboxExt`](crate::input::InputMailboxExt).
///
/// Impl'd for both transports `ctx.actor::<LifecycleCapability>()` can
/// return:
///
/// - [`WasmActorMailbox<LifecycleCapability>`] — always-on, for the §12
///   wasm-component stage-subscribe site.
/// - [`NativeActorMailbox<'_, LifecycleCapability>`] — native cap-to-cap
///   sends, gated on `#[cfg(not(target_family = "wasm"))]`.
///
/// All methods are fire-and-forget. `subscribe` / `unsubscribe` reply
/// via `aether.lifecycle.subscribe_result`; reply handling stays on the
/// caller. The cap fail-fasts (`Err`) on a stage its chassis graph
/// doesn't declare (ADR-0082 §7).
///
/// The generic escape hatch is unaffected: `mailbox.send(&LifecycleSubscribe { .. })`
/// still works, since `send` is an inherent method on the underlying
/// mailbox type.
pub trait LifecycleMailboxExt {
    /// Mail `aether.lifecycle.subscribe_self { stage }` to the cap —
    /// subscribe the *calling* actor to the lifecycle stage `K` (a
    /// stage kind, e.g. `Tick` / `Render`). The cap resolves the
    /// subscriber from the inbound's host-stamped `Source` (ADR-0083),
    /// so the call site spells out neither the stage id nor its own
    /// mailbox. This is the common form. Idempotent.
    fn subscribe<K: Kind>(&self);

    /// Mail `aether.lifecycle.subscribe { stage, mailbox }` to the cap.
    /// Add an *explicit* `mailbox` to the subscriber set for stage `K`.
    /// The rare cross-mailbox form; [`subscribe`](Self::subscribe)
    /// covers the self case. Idempotent.
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId);

    /// Mail `aether.lifecycle.unsubscribe_self { stage }` to the cap —
    /// unsubscribe the *calling* actor from stage `K`. Reflexive twin
    /// of [`subscribe`](Self::subscribe). Idempotent on "not currently
    /// subscribed."
    fn unsubscribe<K: Kind>(&self);

    /// Mail `aether.lifecycle.unsubscribe { stage, mailbox }` to the
    /// cap. Remove an *explicit* `mailbox` from the subscriber set for
    /// stage `K`. Idempotent on "not currently subscribed."
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId);
}

impl LifecycleMailboxExt for WasmActorMailbox<'_, LifecycleCapability> {
    fn subscribe<K: Kind>(&self) {
        self.send(&LifecycleSubscribeSelf { stage: K::ID.0 });
    }
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&LifecycleSubscribe {
            stage: K::ID.0,
            mailbox: mailbox.0,
        });
    }
    fn unsubscribe<K: Kind>(&self) {
        self.send(&LifecycleUnsubscribeSelf { stage: K::ID.0 });
    }
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&LifecycleUnsubscribe {
            stage: K::ID.0,
            mailbox: mailbox.0,
        });
    }
}

#[cfg(not(target_family = "wasm"))]
impl LifecycleMailboxExt for NativeActorMailbox<'_, LifecycleCapability> {
    fn subscribe<K: Kind>(&self) {
        self.send(&LifecycleSubscribeSelf { stage: K::ID.0 });
    }
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&LifecycleSubscribe {
            stage: K::ID.0,
            mailbox: mailbox.0,
        });
    }
    fn unsubscribe<K: Kind>(&self) {
        self.send(&LifecycleUnsubscribeSelf { stage: K::ID.0 });
    }
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&LifecycleUnsubscribe {
            stage: K::ID.0,
            mailbox: mailbox.0,
        });
    }
}

/// Push the current stage's empty signal to each subscriber as an
/// untyped envelope. Uses the runtime-id `send_envelope_traced` path
/// because the broadcast kind is chosen at runtime (the current
/// state's), not a compile-site `K`; the path preserves the inbound
/// `(parent, root)` lineage so settlement counts each child against
/// the root (ADR-0080 §6).
#[cfg(not(target_family = "wasm"))]
pub fn broadcast_to_subscribers<M: ReplyMode>(
    ctx: &mut NativeCtx<'_, M>,
    subscribers: &BTreeMap<KindId, BTreeSet<MailboxId>>,
    stage: KindId,
) {
    let Some(set) = subscribers.get(&stage) else {
        return;
    };
    for mailbox in set {
        let _ = ctx.send_envelope_traced(SubstrateMailboxId(mailbox.0), stage, &[]);
    }
}
