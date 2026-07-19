//! `aether.input` cap. Owns the ADR-0021 publish/subscribe routing
//! table for substrate input streams (`Key`, `KeyRelease`,
//! `MouseMove`, `MouseButton`, `MouseButtonRelease`, `MouseWheel`,
//! `WindowSize`, `TextInput`, `ImePreedit`, `Modifiers`).
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

#[cfg(feature = "runtime")]
mod config;
pub mod kinds;

pub use kinds::*;

#[cfg(feature = "runtime")]
pub use config::InputConfig;

use aether_actor::WasmActorMailbox;
use aether_data::{Kind, MailboxId};
#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
use aether_substrate::actor::native::NativeActorMailbox;

use aether_actor::actor;

// Handler-signature kinds for the input-event handlers must be importable
// at module root too: `#[actor]` emits `impl HandlesKind<K> for
// InputCapability {}` markers always-on, outside the `feature = "runtime"`
// gate, for every `#[handler]` parameter type the moved `#[runtime] impl`
// declares — including these ten stream-event kinds, not just the
// subscribe/unsubscribe family.
use aether_kinds::{
    ImePreedit, Key, KeyRelease, Modifiers, MouseButton, MouseButtonRelease, MouseMove, MouseWheel, TextInput,
    WindowSize,
};

/// `aether.input` cap **identity** (ADR-0122 identity/runtime split). A
/// ZST carrying only the addressing — the `Addressable` / `HandlesKind`
/// markers and the name-inventory entry, all emitted always-on by
/// `#[actor]`. The state-bearing runtime (`InputCapabilityState`,
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
///    `MouseButton`, `MouseButtonRelease`, `MouseWheel`, `WindowSize`,
///    `TextInput`, `ImePreedit`, `Modifiers`) — pushed by the chassis
///    driver after each platform event; the cap fans out one mail per
///    subscriber. Fire-and-forget; no reply.
#[actor(singleton)]
pub struct InputCapability;

// The reply kind (`SubscribeInputResult`) rides the native gate (not
// `runtime`): the `#[actor]` macro's ADR-0109 `HandlerEntry` inventory
// submission — emitted on every native build, runtime or not — names each
// handler's reply kind `::ID`, so a transport-only build must still see
// it. It is already always-on in scope via the unconditional
// `pub use kinds::*;` above (no local `kinds` cfg gate to mirror), so no
// separate import is needed here. The rest of the runtime half (the
// `aether_substrate`-typed imports, the state struct + its `fanout`
// helper, and the shared mailbox-validation fn) sits behind the one
// `feature = "runtime"` gate.

// The runtime half — the `aether_substrate`-typed imports, the state
// struct + its `fanout` helper, and the `#[runtime] impl` — lives in
// `runtime.rs`, gated once here. Nothing in this file names a runtime
// type directly, so there is no `use runtime::*` glob (matching
// `fs/mod.rs`).
#[cfg(feature = "runtime")]
mod runtime;

/// Sender-side facade for callers addressing [`InputCapability`] via
/// `ctx.actor::<InputCapability>()`.
///
/// Lifts the cap-shaped operations (`subscribe::<K>()`,
/// `subscribe_for::<K>(mailbox)`, the `unsubscribe` twins,
/// `unsubscribe_all(mailbox)`) one indirection above the raw
/// `.send(&SubscribeInput { .. })` so component code stops
/// reconstructing the kind struct at every call site. Same shape and
/// rationale as [`aether_fs::FsMailboxExt`]
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
///   sends, gated on `#[cfg(not(target_family = "wasm"))]`.
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
        self.send(&SubscribeInput { kind: K::ID, mailbox });
    }
    fn unsubscribe<K: Kind>(&self) {
        self.send(&UnsubscribeInputSelf { kind: K::ID });
    }
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeInput { kind: K::ID, mailbox });
    }
    fn unsubscribe_all(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeAll { mailbox });
    }
}

#[cfg(all(not(target_family = "wasm"), feature = "runtime"))]
impl InputMailboxExt for NativeActorMailbox<'_, InputCapability> {
    fn subscribe<K: Kind>(&self) {
        self.send(&SubscribeInputSelf { kind: K::ID });
    }
    fn subscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&SubscribeInput { kind: K::ID, mailbox });
    }
    fn unsubscribe<K: Kind>(&self) {
        self.send(&UnsubscribeInputSelf { kind: K::ID });
    }
    fn unsubscribe_for<K: Kind>(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeInput { kind: K::ID, mailbox });
    }
    fn unsubscribe_all(&self, mailbox: MailboxId) {
        self.send(&UnsubscribeAll { mailbox });
    }
}
