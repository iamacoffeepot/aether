//! Sender-side facade for the `aether.lifecycle` driver (ADR-0082 §7,
//! §12). The lifecycle analogue of [`crate::input::InputMailboxExt`]:
//! lets an actor subscribe a mailbox to a lifecycle *stage* broadcast
//! (e.g. `Render`) so that stage's per-advance payload fans out to it.
//!
//! **Why this is name-addressed, not type-addressed.** The receive-side
//! driver is `LifecycleDriverCapability<C>` — generic over the chassis
//! context `C` (`LifecycleDriverCapability<DesktopCtx>`,
//! `LifecycleDriverCapability<HeadlessCtx>`, ...). A wasm guest doesn't
//! know `C`, so it can't name the driver through
//! `ctx.actor::<LifecycleDriverCapability<C>>()` the way the input path
//! names [`InputCapability`](crate::input::InputCapability). Instead this
//! module declares a tiny non-generic marker, [`LifecycleMailbox`], whose
//! `NAMESPACE` is the driver's fixed mailbox name `"aether.lifecycle"`.
//! `ctx.actor::<LifecycleMailbox>()` resolves that name to a mailbox id
//! the same way every other `ctx.actor::<R>()` does — so the subscribe is
//! chassis-context-agnostic and works from an FFI guest that has no
//! handle on the driver's `C`.
//!
//! The marker carries `HandlesKind<LifecycleSubscribe>` /
//! `HandlesKind<LifecycleUnsubscribe>` so the typed-send gate still
//! rejects wrong-kind sends at the call site, exactly like a
//! macro-emitted cap. It is a pure addressing token — it is never
//! registered or booted as an actor (the generic
//! [`LifecycleDriverCapability`] is the real receiver); it only names
//! the mailbox.
//!
//! Call site, mirroring the input subscribe in a component's `wire`:
//!
//! ```ignore
//! let me = MailboxId(ctx.mailbox_id());
//! ctx.actor::<LifecycleMailbox>().subscribe(Render::ID, me);
//! ```
//!
//! The driver's `on_subscribe` validates the stage against the chassis
//! graph and replies [`LifecycleSubscribeResult`] — `Err` fail-fast on a
//! chassis that doesn't declare that stage (ADR-0082 §7). Reply handling
//! stays on the caller.
//!
//! [`LifecycleDriverCapability`]: aether_substrate::LifecycleDriverCapability
//! [`LifecycleSubscribeResult`]: aether_kinds::LifecycleSubscribeResult

use aether_actor::{Actor, FfiActorMailbox, HandlesKind, Singleton};
use aether_data::{KindId, MailboxId};
use aether_kinds::{LifecycleSubscribe, LifecycleUnsubscribe};
#[cfg(not(target_arch = "wasm32"))]
use aether_substrate::actor::native::NativeActorMailbox;

/// Non-generic addressing marker for the `aether.lifecycle` driver.
///
/// The real receiver is the generic
/// [`LifecycleDriverCapability<C>`](aether_substrate::LifecycleDriverCapability),
/// which a wasm guest can't name (it doesn't know `C`). This ZST stands
/// in purely so callers can address the lifecycle mailbox *by name*
/// through the ordinary `ctx.actor::<R>()` path: its [`Actor::NAMESPACE`]
/// is the driver's fixed mailbox name and its [`Singleton`] /
/// [`HandlesKind`] markers let `ctx.actor::<LifecycleMailbox>()` return a
/// typed sender with the wrong-kind compile gate intact.
///
/// It is never instantiated, registered, or booted — there is no `init`,
/// no handler dispatch. The substrate registers the generic driver at the
/// same `NAMESPACE`; this marker only resolves the name.
pub struct LifecycleMailbox;

impl Actor for LifecycleMailbox {
    /// The driver's fixed mailbox name. Hardcoded rather than aliased
    /// from `LifecycleDriverCapability::NAMESPACE` because that type lives
    /// in the native-only `aether-substrate` crate, while this marker
    /// must resolve on `wasm32` too. A native-gated test pins this literal
    /// to the driver's constant so the two can't drift.
    const NAMESPACE: &'static str = "aether.lifecycle";
}

impl Singleton for LifecycleMailbox {}
impl HandlesKind<LifecycleSubscribe> for LifecycleMailbox {}
impl HandlesKind<LifecycleUnsubscribe> for LifecycleMailbox {}

/// Sender-side facade for callers addressing the lifecycle driver via
/// `ctx.actor::<LifecycleMailbox>()`.
///
/// Lifts the stage-subscribe operations one indirection above the raw
/// `.send(&LifecycleSubscribe { .. })` so component code stops
/// reconstructing the kind struct (and the `.0` field unwraps) at every
/// call site — same shape and rationale as
/// [`InputMailboxExt`](crate::input::InputMailboxExt).
///
/// Impl'd for both transports `ctx.actor::<LifecycleMailbox>()` can
/// return:
///
/// - [`FfiActorMailbox<LifecycleMailbox>`] — always-on, for wasm-component
///   callers (the §12 stage-subscribe site).
/// - [`NativeActorMailbox<'_, LifecycleMailbox>`] — native cap-to-cap
///   sends, gated on `#[cfg(not(target_arch = "wasm32"))]`.
///
/// All methods are fire-and-forget. `subscribe` / `unsubscribe` reply
/// via `aether.lifecycle.subscribe_result`; reply handling stays on the
/// caller. The driver fail-fasts (`Err`) on a stage its chassis graph
/// doesn't declare (ADR-0082 §7).
///
/// The generic escape hatch is unaffected: `mailbox.send(&LifecycleSubscribe { .. })`
/// still works, since `send` is an inherent method on the underlying
/// mailbox type.
pub trait LifecycleMailboxExt {
    /// Mail `aether.lifecycle.subscribe { stage, mailbox }` to the driver.
    /// Add `mailbox` to the subscriber set for the lifecycle stage `stage`
    /// (a stage kind's [`KindId`], e.g. `Render::ID`). Idempotent.
    fn subscribe(&self, stage: KindId, mailbox: MailboxId);

    /// Mail `aether.lifecycle.unsubscribe { stage, mailbox }` to the
    /// driver. Remove `mailbox` from the subscriber set for `stage`.
    /// Idempotent on "not currently subscribed."
    fn unsubscribe(&self, stage: KindId, mailbox: MailboxId);
}

impl LifecycleMailboxExt for FfiActorMailbox<LifecycleMailbox> {
    fn subscribe(&self, stage: KindId, mailbox: MailboxId) {
        self.send(&LifecycleSubscribe {
            stage: stage.0,
            mailbox: mailbox.0,
        });
    }
    fn unsubscribe(&self, stage: KindId, mailbox: MailboxId) {
        self.send(&LifecycleUnsubscribe {
            stage: stage.0,
            mailbox: mailbox.0,
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl LifecycleMailboxExt for NativeActorMailbox<'_, LifecycleMailbox> {
    fn subscribe(&self, stage: KindId, mailbox: MailboxId) {
        self.send(&LifecycleSubscribe {
            stage: stage.0,
            mailbox: mailbox.0,
        });
    }
    fn unsubscribe(&self, stage: KindId, mailbox: MailboxId) {
        self.send(&LifecycleUnsubscribe {
            stage: stage.0,
            mailbox: mailbox.0,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Name-addressing pins to the real driver. The marker hardcodes
    /// `"aether.lifecycle"` so it resolves on `wasm32` (where
    /// `aether-substrate` is absent); this native-gated check asserts that
    /// literal still matches `LifecycleDriverCapability`'s `NAMESPACE`, so
    /// the two can't silently drift and leave guest subscribes routing to
    /// a dead mailbox id.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn marker_namespace_matches_the_driver() {
        use aether_substrate::LifecycleDriverCapability;
        assert_eq!(
            <LifecycleMailbox as Actor>::NAMESPACE,
            <LifecycleDriverCapability<()> as Actor>::NAMESPACE,
        );
        assert_eq!(<LifecycleMailbox as Actor>::NAMESPACE, "aether.lifecycle");
    }

    /// The typed-send gate the ext relies on: `ctx.actor::<LifecycleMailbox>()`
    /// can only `.send()` the two lifecycle subscribe kinds. A compile-time
    /// assertion — if a future edit drops a `HandlesKind` impl, this stops
    /// building.
    #[test]
    fn marker_handles_the_subscribe_kinds() {
        fn assert_handles<R: HandlesKind<K>, K: aether_data::Kind>() {}
        fn assert_singleton<R: Singleton>() {}
        assert_handles::<LifecycleMailbox, LifecycleSubscribe>();
        assert_handles::<LifecycleMailbox, LifecycleUnsubscribe>();
        assert_singleton::<LifecycleMailbox>();
    }
}
