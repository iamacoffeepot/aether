//! Sender side + fan-out for the lifecycle cap. Holds the
//! [`LifecycleMailboxExt`] facade callers reach through
//! `ctx.actor::<LifecycleCapability>()` (always-on, both transports) and
//! the native [`broadcast_to_subscribers`] fan-out the receive side calls
//! once per advance.

use aether_actor::{HandlesKind, WasmActorMailbox, WasmActorMailboxWithContext};
use aether_data::{Kind, MailboxId};
use aether_kinds::{LifecycleSubscribe, LifecycleSubscribeSelf, LifecycleUnsubscribe, LifecycleUnsubscribeSelf};

use super::LifecycleCapability;

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_actor::ReplyMode;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_data::KindId;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::{NativeActorMailbox, NativeActorMailboxWithContext, NativeCtx};
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::mail::MailboxId as SubstrateMailboxId;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use std::collections::{BTreeMap, BTreeSet};

/// Sender-side facade for callers addressing [`LifecycleCapability`]
/// via `ctx.actor::<LifecycleCapability>()` (ADR-0082 §7, §12).
///
/// Lifts the stage-subscribe operations one indirection above the raw
/// `.send(&LifecycleSubscribe { .. })` so component code stops
/// reconstructing the kind struct (and the `.0` field unwraps) at every
/// call site — same shape and rationale as
/// `InputMailboxExt` on the `aether.input` cap.
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
trait LifecycleMailboxForward {
    fn forward<K>(&self, payload: &K)
    where
        LifecycleCapability: HandlesKind<K>,
        K: Kind;
}

#[allow(private_bounds)]
pub trait LifecycleMailboxExt: LifecycleMailboxForward {
    /// Mail `aether.lifecycle.subscribe_self { stage }` to the cap —
    /// subscribe the *calling* actor to the lifecycle stage `K` (a
    /// stage kind, e.g. `Tick` / `Render`). The cap resolves the
    /// subscriber from the inbound's host-stamped `Source` (ADR-0083),
    /// so the call site spells out neither the stage id nor its own
    /// mailbox. This is the common form. Idempotent.
    fn subscribe<K: Kind>(&self) {
        self.forward(&LifecycleSubscribeSelf { stage: K::ID.0 });
    }

    /// Mail `aether.lifecycle.subscribe { stage, mailbox }` to the cap.
    /// Add an *explicit* `mailbox` to the subscriber set for stage `K`.
    /// The rare cross-mailbox form; [`subscribe`](Self::subscribe)
    /// covers the self case. Idempotent.
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.forward(&LifecycleSubscribe { stage: K::ID.0, mailbox: mailbox.0 });
    }

    /// Mail `aether.lifecycle.unsubscribe_self { stage }` to the cap —
    /// unsubscribe the *calling* actor from stage `K`. Reflexive twin
    /// of [`subscribe`](Self::subscribe). Idempotent on "not currently
    /// subscribed."
    fn unsubscribe<K: Kind>(&self) {
        self.forward(&LifecycleUnsubscribeSelf { stage: K::ID.0 });
    }

    /// Mail `aether.lifecycle.unsubscribe { stage, mailbox }` to the
    /// cap. Remove an *explicit* `mailbox` from the subscriber set for
    /// stage `K`. Idempotent on "not currently subscribed."
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.forward(&LifecycleUnsubscribe { stage: K::ID.0, mailbox: mailbox.0 });
    }
}

impl<T: LifecycleMailboxForward> LifecycleMailboxExt for T {}

impl LifecycleMailboxForward for WasmActorMailbox<'_, LifecycleCapability> {
    fn forward<K>(&self, payload: &K)
    where
        LifecycleCapability: HandlesKind<K>,
        K: Kind,
    {
        self.send(payload);
    }
}

impl<C: Kind> LifecycleMailboxForward for WasmActorMailboxWithContext<'_, '_, LifecycleCapability, C> {
    fn forward<K>(&self, payload: &K)
    where
        LifecycleCapability: HandlesKind<K>,
        K: Kind,
    {
        let _ = self.send(payload);
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl LifecycleMailboxForward for NativeActorMailbox<'_, LifecycleCapability> {
    fn forward<K>(&self, payload: &K)
    where
        LifecycleCapability: HandlesKind<K>,
        K: Kind,
    {
        self.send(payload);
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl<C: Kind> LifecycleMailboxForward for NativeActorMailboxWithContext<'_, '_, LifecycleCapability, C> {
    fn forward<K>(&self, payload: &K)
    where
        LifecycleCapability: HandlesKind<K>,
        K: Kind,
    {
        let _ = self.send(payload);
    }
}

/// Push the current stage's empty signal to each subscriber as an
/// untyped envelope. Uses the runtime-id `send_envelope_tracked` path
/// because the broadcast kind is chosen at runtime (the current
/// state's), not a compile-site `K`; the path preserves the inbound
/// `(parent, root)` lineage so settlement counts each child against
/// the root (ADR-0080 §6).
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub fn broadcast_to_subscribers<M: ReplyMode>(
    ctx: &mut NativeCtx<'_, M>,
    subscribers: &BTreeMap<KindId, BTreeSet<MailboxId>>,
    stage: KindId,
) {
    let Some(set) = subscribers.get(&stage) else {
        return;
    };
    for mailbox in set {
        let _ = ctx.send_envelope_tracked(SubstrateMailboxId(mailbox.0), stage, &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LifecycleCapability, LifecycleMailboxExt, LifecycleSubscribeSelf, WasmActorMailbox, WasmActorMailboxWithContext,
    };

    fn assert_facade<T: LifecycleMailboxExt>() {}

    #[test]
    fn facade_is_available_to_wasm_senders() {
        assert_facade::<WasmActorMailbox<'static, LifecycleCapability>>();
        assert_facade::<WasmActorMailboxWithContext<'static, 'static, LifecycleCapability, LifecycleSubscribeSelf>>();
    }

    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    #[test]
    fn facade_is_available_to_native_senders() {
        assert_facade::<super::NativeActorMailbox<'static, LifecycleCapability>>();
        assert_facade::<
            super::NativeActorMailboxWithContext<'static, 'static, LifecycleCapability, LifecycleSubscribeSelf>,
        >();
    }
}
