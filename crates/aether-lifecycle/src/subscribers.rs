//! Sender side + fan-out for the lifecycle cap. Holds the
//! [`LifecycleMailboxExt`] facade callers reach through
//! `ctx.actor::<LifecycleCapability>()` (always-on, both transports) and
//! the native [`broadcast_to_subscribers`] fan-out the receive side calls
//! once per advance.

use aether_actor::{MailboxForward, Publishes};
use aether_data::{Kind, MailboxId};
use aether_kinds::{
    InitCaps, InitComponents, LifecycleSubscribe, LifecycleSubscribeSelf, LifecycleUnsubscribe,
    LifecycleUnsubscribeSelf, Present, Render, Shutdown, Tick,
};

use super::LifecycleCapability;

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_actor::ReplyMode;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_data::KindId;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::NativeCtx;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::mail::MailboxId as SubstrateMailboxId;
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use std::collections::{BTreeMap, BTreeSet};

// The stage kinds this cap broadcasts to its subscriber set, one
// `Publishes` impl each — the compile-time gate on
// `LifecycleMailboxExt::subscribe`. The list is the ADR-0082 stage
// vocabulary a chassis lifecycle graph can declare as a state; the
// runtime still fail-fasts on a stage *this* chassis's graph omits
// (ADR-0082 §7), so the marker states what the cap can ever emit and
// the reply states what it does emit here.
//
// `Quit` and `LifecycleAdvance` are absent on purpose: they travel
// *into* the cap as signals, never out of it as a broadcast.
impl Publishes<Tick> for LifecycleCapability {}
impl Publishes<InitCaps> for LifecycleCapability {}
impl Publishes<InitComponents> for LifecycleCapability {}
impl Publishes<Render> for LifecycleCapability {}
impl Publishes<Present> for LifecycleCapability {}
impl Publishes<Shutdown> for LifecycleCapability {}

/// Sender-side facade for callers addressing [`LifecycleCapability`]
/// via `ctx.actor::<LifecycleCapability>()` (ADR-0082 §7, §12).
///
/// Lifts the stage-subscribe operations one indirection above the raw
/// `.send(&LifecycleSubscribe { .. })` so component code stops
/// reconstructing the kind struct (and the `.0` field unwraps) at every
/// call site — same shape and rationale as
/// `InputMailboxExt` on the `aether.input` cap.
///
/// Blanket-impl'd over [`MailboxForward<LifecycleCapability>`], so it reaches
/// every handle `ctx.actor::<LifecycleCapability>()` can return — the §12
/// wasm-component stage-subscribe site and native cap-to-cap sends alike.
///
/// All methods are fire-and-forget. `subscribe` / `unsubscribe` reply
/// via `aether.lifecycle.subscribe_result`; reply handling stays on the
/// caller. The cap fail-fasts (`Err`) on a stage its chassis graph
/// doesn't declare (ADR-0082 §7).
///
/// The generic escape hatch is unaffected: `mailbox.send(&LifecycleSubscribe { .. })`
/// still works, since `send` is an inherent method on the underlying
/// mailbox type.
pub trait LifecycleMailboxExt: MailboxForward<LifecycleCapability> {
    /// Mail `aether.lifecycle.subscribe_self { stage }` to the cap —
    /// subscribe the *calling* actor to the lifecycle stage `K` (a
    /// stage kind, e.g. `Tick` / `Render`). The cap resolves the
    /// subscriber from the inbound's host-stamped `Source` (ADR-0083),
    /// so the call site spells out neither the stage id nor its own
    /// mailbox. This is the common form. Idempotent.
    ///
    /// `K` is gated on `LifecycleCapability: Publishes<K>`, so a kind
    /// this cap never broadcasts — a window device event, say — is a
    /// compile error naming the capability that does publish it.
    fn subscribe<K: Kind>(&self)
    where
        LifecycleCapability: Publishes<K>,
    {
        self.forward(&LifecycleSubscribeSelf { stage: K::ID.0 });
    }

    /// Mail `aether.lifecycle.subscribe { stage, mailbox }` to the cap.
    /// Add an *explicit* `mailbox` to the subscriber set for stage `K`.
    /// The rare cross-mailbox form; [`subscribe`](Self::subscribe)
    /// covers the self case. Idempotent.
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId)
    where
        LifecycleCapability: Publishes<K>,
    {
        self.forward(&LifecycleSubscribe { stage: K::ID.0, mailbox: mailbox.0 });
    }

    /// Mail `aether.lifecycle.unsubscribe_self { stage }` to the cap —
    /// unsubscribe the *calling* actor from stage `K`. Reflexive twin
    /// of [`subscribe`](Self::subscribe). Idempotent on "not currently
    /// subscribed."
    fn unsubscribe<K: Kind>(&self)
    where
        LifecycleCapability: Publishes<K>,
    {
        self.forward(&LifecycleUnsubscribeSelf { stage: K::ID.0 });
    }

    /// Mail `aether.lifecycle.unsubscribe { stage, mailbox }` to the
    /// cap. Remove an *explicit* `mailbox` from the subscriber set for
    /// stage `K`. Idempotent on "not currently subscribed."
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId)
    where
        LifecycleCapability: Publishes<K>,
    {
        self.forward(&LifecycleUnsubscribe { stage: K::ID.0, mailbox: mailbox.0 });
    }
}

impl<T: MailboxForward<LifecycleCapability>> LifecycleMailboxExt for T {}

/// Push the current stage payload to each subscriber as an untyped envelope.
/// Uses the runtime-id `send_envelope_tracked` path because the broadcast
/// kind is chosen at runtime (the current state's), not a compile-site `K`;
/// the path preserves the inbound `(parent, root)` lineage so settlement
/// counts each child against the root (ADR-0080 §6).
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
pub fn broadcast_to_subscribers<M: ReplyMode>(
    ctx: &mut NativeCtx<'_, M>,
    subscribers: &BTreeMap<KindId, BTreeSet<MailboxId>>,
    stage: KindId,
    payload: &[u8],
) {
    let Some(set) = subscribers.get(&stage) else {
        return;
    };
    for mailbox in set {
        let _ = ctx.send_envelope_tracked(SubstrateMailboxId(mailbox.0), stage, payload);
    }
}

#[cfg(test)]
mod tests {
    use super::{LifecycleCapability, LifecycleMailboxExt, LifecycleSubscribeSelf};
    use aether_actor::{WasmActorMailbox, WasmActorMailboxWithContext};
    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    use aether_substrate::actor::native::{NativeActorMailbox, NativeActorMailboxWithContext};

    fn assert_facade<T: LifecycleMailboxExt>() {}

    #[test]
    fn facade_is_available_to_wasm_senders() {
        assert_facade::<WasmActorMailbox<'static, LifecycleCapability>>();
        assert_facade::<WasmActorMailboxWithContext<'static, 'static, LifecycleCapability, LifecycleSubscribeSelf>>();
    }

    #[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
    #[test]
    fn facade_is_available_to_native_senders() {
        assert_facade::<NativeActorMailbox<'static, LifecycleCapability>>();
        assert_facade::<NativeActorMailboxWithContext<'static, 'static, LifecycleCapability, LifecycleSubscribeSelf>>();
    }
}
