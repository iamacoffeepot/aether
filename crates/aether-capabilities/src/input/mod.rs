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

pub mod kinds;
pub mod subscription;

pub use kinds::*;
pub use subscription::InputCapability;

#[cfg(not(target_arch = "wasm32"))]
pub use subscription::InputConfig;

use aether_actor::WasmActorMailbox;
use aether_data::{Kind, MailboxId};
#[cfg(not(target_arch = "wasm32"))]
use aether_substrate::actor::native::NativeActorMailbox;

/// Sender-side facade for callers addressing [`InputCapability`] via
/// `ctx.actor::<InputCapability>()`.
///
/// Lifts the cap-shaped operations (`subscribe::<K>()`,
/// `subscribe_for::<K>(mailbox)`, the `unsubscribe` twins,
/// `unsubscribe_all(mailbox)`) one indirection above the raw
/// `.send(&SubscribeInput { .. })` so component code stops
/// reconstructing the kind struct at every call site. Same shape and
/// rationale as [`crate::fs::FsMailboxExt`]
/// (issue 580) and [`crate::component::ComponentHostWasmExt`] (issue
/// 654) — the cap module owns receive-side ([`InputCapability`]) AND
/// send-side ([`InputMailboxExt`]) so future kind additions land both
/// surfaces in one place.
///
/// Impl'd for both transports `ctx.actor::<InputCapability>()` can
/// return:
///
/// - [`WasmActorMailbox<InputCapability>`] — always-on, for
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

impl InputMailboxExt for WasmActorMailbox<'_, InputCapability> {
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
